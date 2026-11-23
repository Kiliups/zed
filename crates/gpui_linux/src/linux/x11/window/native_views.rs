//! Shows platform windows above a window's GPUI content.
//!
//! X11 draws a child window above its parent's content, so each native view is
//! reparented into a container window placed over the visible part of its
//! element, and shaped to leave out whatever GPUI paints over the view.
//!
//! GPUI's focus decides which window receives keyboard input: the input focus
//! is the native view whose handle GPUI has focused, and the GPUI window
//! otherwise. It is moved back there when a toolkit or the window manager
//! moves it anywhere else in the window.
//!
//! A container grabs the mouse buttons and the keyboard, so that GPUI sees each
//! press in it first. A mouse press on a view is replayed to the view after
//! GPUI is told of it. A key press for a view with keyboard input is offered to
//! GPUI, and replayed to the view if GPUI leaves it. Any other key press is
//! GPUI's, even with the pointer over a view, which X11 would send it to.

use super::{X11WindowState, X11WindowStatePtr, check_reply, get_reply, xcb_flush};
use anyhow::Context as _;
use gpui::{
    Bounds, KeyDownEvent, NativeViewId, PaintNativeView, Pixels, Point, ScaledPixels, Scene,
};
use gpui_util::{ResultExt as _, maybe};
use std::{
    any::Any,
    cell::RefCell,
    ffi::c_void,
    ptr::NonNull,
    rc::{Rc, Weak},
};
use x11rb::{
    connection::Connection as _,
    protocol::{
        shape::{self, ConnectionExt as _},
        xinput::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
    },
    xcb_ffi::XCBConnection,
};

#[derive(Default)]
pub(crate) struct NativeViews {
    views: Vec<AttachedView>,
    /// The view GPUI has focused, which has keyboard input while it is shown.
    focused: Option<NativeViewId>,
    /// The keys, by key code, whose presses GPUI took ahead of a native view.
    /// Their releases are GPUI's too.
    keys_taken_before_view: Vec<u8>,
}

struct AttachedView {
    id: NativeViewId,
    view: xproto::Window,
    container: xproto::Window,
    /// Where the view was last placed, or `None` while it is hidden.
    placement: Option<Placement>,
}

impl NativeViews {
    fn shown(&self, id: NativeViewId) -> Option<&AttachedView> {
        self.views
            .iter()
            .find(|attached| attached.id == id && attached.placement.is_some())
    }

    /// Whether a native view GPUI has focused is shown, and so takes keyboard input.
    pub(crate) fn has_keyboard_input(&self) -> bool {
        self.focused.is_some_and(|id| self.shown(id).is_some())
    }

    pub(crate) fn contains_view(&self, view: xproto::Window) -> bool {
        self.views.iter().any(|attached| attached.view == view)
    }

    /// Where a shown container is in the window, in device pixels.
    pub(crate) fn container_origin(&self, container: xproto::Window) -> Option<Point<i32>> {
        let attached = self
            .views
            .iter()
            .find(|attached| attached.container == container)?;
        Some(attached.placement.as_ref()?.container.origin)
    }

    pub(crate) fn note_key_taken_before_view(&mut self, key_code: u8) {
        if !self.keys_taken_before_view.contains(&key_code) {
            self.keys_taken_before_view.push(key_code);
        }
    }

    /// Whether GPUI took the press of this released key ahead of a view.
    pub(crate) fn take_key_taken_before_view(&mut self, key_code: u8) -> bool {
        let count = self.keys_taken_before_view.len();
        self.keys_taken_before_view
            .retain(|taken| *taken != key_code);
        self.keys_taken_before_view.len() != count
    }
}

/// Where a native view is shown, in device pixels.
#[derive(Debug, PartialEq)]
struct Placement {
    /// The visible part of the view, in the window.
    container: Bounds<i32>,
    /// The whole view, in the container.
    view: Bounds<i32>,
    /// The parts of the container that nothing is drawn over.
    shape: Vec<Bounds<i32>>,
}

impl Placement {
    fn new(scene: &Scene, native_view: &PaintNativeView) -> Option<Self> {
        let container = to_device_pixels(native_view.visible_bounds());
        if container.is_empty() {
            return None;
        }
        let in_container = |bounds: Bounds<i32>| Bounds {
            origin: bounds.origin - container.origin,
            size: bounds.size,
        };
        let mut shape = vec![container];
        for cover in scene.native_view_covers(native_view) {
            let cover = to_device_pixels(cover);
            shape = shape
                .iter()
                .flat_map(|part| subtract(part, &cover))
                .collect();
        }
        Some(Self {
            container,
            view: in_container(to_device_pixels(native_view.bounds)),
            shape: shape.into_iter().map(in_container).collect(),
        })
    }
}

fn to_device_pixels(bounds: Bounds<ScaledPixels>) -> Bounds<i32> {
    let round = |value: ScaledPixels| value.0.round() as i32;
    Bounds::from_corners(bounds.origin.map(round), bounds.bottom_right().map(round))
}

/// The parts of `area` that `cut` doesn't overlap.
fn subtract(area: &Bounds<i32>, cut: &Bounds<i32>) -> Vec<Bounds<i32>> {
    if !area.intersects(cut) {
        return vec![*area];
    }
    let overlap = area.intersect(cut);
    let band = |left, top, right, bottom| {
        Bounds::from_corners(Point::new(left, top), Point::new(right, bottom))
    };
    [
        band(area.left(), area.top(), area.right(), overlap.top()),
        band(area.left(), overlap.bottom(), area.right(), area.bottom()),
        band(area.left(), overlap.top(), overlap.left(), overlap.bottom()),
        band(
            overlap.right(),
            overlap.top(),
            area.right(),
            overlap.bottom(),
        ),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect()
}

struct Attachment {
    state: Weak<RefCell<X11WindowState>>,
    xcb: Rc<XCBConnection>,
    x_window: xproto::Window,
    id: NativeViewId,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut state = state.borrow_mut();
        let views = &mut state.native_views;
        let Some(index) = views
            .views
            .iter()
            .position(|attached| attached.id == self.id)
        else {
            return;
        };
        if views.focused == Some(self.id) {
            views.focused = None;
        }
        let attached = views.views.remove(index);
        let root = state.x_root_window;
        drop(state);
        detach(&self.xcb, self.x_window, root, &attached);
    }
}

/// Returns a view to the root window, unmapped, and destroys its container.
fn detach(
    xcb: &XCBConnection,
    x_window: xproto::Window,
    root: xproto::Window,
    attached: &AttachedView,
) {
    // X11 would revert keyboard input to the container, which is about to go.
    if input_holder(xcb, x_window, std::slice::from_ref(attached))
        == Some(InputHolder::NativeView(attached.id))
    {
        xcb.set_input_focus(xproto::InputFocus::PARENT, x_window, x11rb::CURRENT_TIME)
            .log_err();
    }
    let no_events =
        xproto::ChangeWindowAttributesAux::new().event_mask(xproto::EventMask::NO_EVENT);
    xcb.change_window_attributes(attached.view, &no_events)
        .log_err();
    xcb.unmap_window(attached.view).log_err();
    xcb.reparent_window(attached.view, root, 0, 0).log_err();
    xcb.destroy_window(attached.container).log_err();
    xcb_flush(xcb);
}

/// Detaches every view before the window is destroyed, which would destroy them too.
pub(super) fn detach_all(window: &X11WindowStatePtr, state: &mut X11WindowState) {
    for attached in std::mem::take(&mut state.native_views.views) {
        detach(&window.xcb, window.x_window, state.x_root_window, &attached);
    }
}

/// Reparents `view`, the ID of an X window, into a container that stays hidden
/// until a scene shows it.
pub(super) fn attach(
    window: &X11WindowStatePtr,
    id: NativeViewId,
    view: NonNull<c_void>,
) -> Option<Box<dyn Any>> {
    let view = xproto::Window::try_from(view.as_ptr() as usize)
        .context("a native view on X11 is the ID of a window")
        .log_err()?;
    let xcb = &*window.xcb;
    let root = window.state.borrow().x_root_window;
    let container = xcb.generate_id().log_err()?;
    let attached = AttachedView {
        id,
        view,
        container,
        placement: None,
    };
    let setup = maybe!({
        check_reply(
            || "X11 CreateWindow for a native view's container failed.",
            xcb.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                container,
                window.x_window,
                0,
                0,
                1,
                1,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &xproto::CreateWindowAux::new(),
            ),
        )?;
        // A toolkit can give its window keyboard input as it maps it, which
        // GPUI undoes, as only GPUI's focus or a click gives it to a view.
        check_reply(
            || "X11 ChangeWindowAttributes for a native view failed.",
            xcb.change_window_attributes(
                view,
                &xproto::ChangeWindowAttributesAux::new()
                    .event_mask(xproto::EventMask::FOCUS_CHANGE),
            ),
        )?;
        check_reply(
            || "X11 ReparentWindow for a native view failed.",
            xcb.reparent_window(view, container, 0, 0),
        )?;
        get_reply(
            || "X11 XIPassiveGrabDevice for a native view failed.",
            xcb.xinput_xi_passive_grab_device(
                x11rb::CURRENT_TIME,
                container,
                x11rb::NONE,
                // Any button.
                0,
                super::XINPUT_ALL_DEVICE_GROUPS,
                xinput::GrabType::BUTTON,
                xinput::GrabMode22::SYNC,
                xproto::GrabMode::ASYNC,
                xinput::GrabOwner::NO_OWNER,
                &[(xinput::XIEventMask::BUTTON_PRESS
                    | xinput::XIEventMask::BUTTON_RELEASE
                    | xinput::XIEventMask::MOTION)
                    .into()],
                &[xinput::ModifierMask::ANY.into()],
            ),
        )?;
        check_reply(
            || "X11 GrabKey for a native view failed.",
            xcb.grab_key(
                false,
                container,
                xproto::ModMask::ANY,
                xproto::Grab::ANY,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::SYNC,
            ),
        )
    });
    if setup.log_err().is_none() {
        detach(xcb, window.x_window, root, &attached);
        return None;
    }
    xcb_flush(xcb);
    window.state.borrow_mut().native_views.views.push(attached);
    Some(Box::new(Attachment {
        state: Rc::downgrade(&window.state),
        xcb: window.xcb.clone(),
        x_window: window.x_window,
        id,
    }))
}

/// Places, shapes, shows and hides the attached views as the scene paints them.
pub(super) fn apply_layout(window: &X11WindowStatePtr, scene: &Scene) {
    let xcb = &*window.xcb;
    let mut shown_or_hidden = false;
    for attached in &mut window.state.borrow_mut().native_views.views {
        let placement = scene
            .native_views
            .iter()
            .find(|native_view| native_view.id == attached.id)
            .and_then(|native_view| Placement::new(scene, native_view));
        if attached.placement == placement {
            continue;
        }
        let previous = attached.placement.take();
        shown_or_hidden |= previous.is_none() != placement.is_none();
        let Some(placement) = placement else {
            xcb.unmap_window(attached.container).log_err();
            continue;
        };
        let configure = |window, bounds: Bounds<i32>| {
            xcb.configure_window(
                window,
                &xproto::ConfigureWindowAux::new()
                    .x(bounds.origin.x)
                    .y(bounds.origin.y)
                    .width(bounds.size.width as u32)
                    .height(bounds.size.height as u32),
            )
            .log_err();
        };
        configure(attached.container, placement.container);
        configure(attached.view, placement.view);
        let rectangles = placement
            .shape
            .iter()
            .map(|part| xproto::Rectangle {
                x: part.origin.x as i16,
                y: part.origin.y as i16,
                width: part.size.width as u16,
                height: part.size.height as u16,
            })
            .collect::<Vec<_>>();
        xcb.shape_rectangles(
            shape::SO::SET,
            shape::SK::BOUNDING,
            xproto::ClipOrdering::UNSORTED,
            attached.container,
            0,
            0,
            &rectangles,
        )
        .log_err();
        if previous.is_none() {
            xcb.map_window(attached.container).log_err();
        }
        attached.placement = Some(placement);
    }
    xcb_flush(xcb);
    if shown_or_hidden {
        // A view that GPUI focused while it was hidden can take keyboard input now.
        window.reconcile_native_view_focus();
    }
}

/// What in a window has keyboard input.
#[derive(Clone, Copy, Debug, PartialEq)]
enum InputHolder {
    Window,
    /// An attached native view, or a window inside it.
    NativeView(NativeViewId),
    /// A window that GPUI didn't attach, which GPUI's focus says nothing about.
    Other,
}

/// What in the window has keyboard input, or `None` if nothing in it does.
fn input_holder(
    xcb: &XCBConnection,
    x_window: xproto::Window,
    views: &[AttachedView],
) -> Option<InputHolder> {
    let mut current = get_reply(|| "X11 GetInputFocus failed.", xcb.get_input_focus())
        .log_err()?
        .focus;
    if current == x_window {
        return Some(InputHolder::Window);
    }
    // `None` and `PointerRoot` are the only values that aren't windows.
    while current > 1 {
        if let Some(attached) = views.iter().find(|attached| attached.container == current) {
            return Some(InputHolder::NativeView(attached.id));
        }
        let tree = get_reply(|| "X11 QueryTree failed.", xcb.query_tree(current)).log_err()?;
        if tree.parent == x_window {
            return Some(InputHolder::Other);
        }
        if tree.parent == tree.root {
            return None;
        }
        current = tree.parent;
    }
    None
}

impl X11WindowStatePtr {
    pub(crate) fn set_focused_native_view(&self, id: Option<NativeViewId>) {
        self.state.borrow_mut().native_views.focused = id;
        self.reconcile_native_view_focus();
    }

    /// Moves keyboard input to match GPUI's focus, if the window has it and a
    /// window that GPUI didn't attach doesn't.
    pub(crate) fn reconcile_native_view_focus(&self) {
        self.move_keyboard_input(false);
    }

    /// Moves keyboard input to match GPUI's focus after a mouse press in the
    /// window's own content, which takes it from any window inside this one,
    /// as a click would on other platforms.
    pub(crate) fn reconcile_native_view_focus_after_press(&self, position: Point<Pixels>) {
        if !self.state.borrow().native_views.views.is_empty()
            && self.hit_test_native_view(position).is_none()
        {
            self.move_keyboard_input(true);
        }
    }

    fn move_keyboard_input(&self, take_from_other: bool) {
        let state = self.state.borrow();
        let views = &state.native_views;
        if views.views.is_empty() {
            return;
        }
        let (target, target_window) = match views.focused.and_then(|id| views.shown(id)) {
            Some(attached) => (InputHolder::NativeView(attached.id), attached.view),
            None => (InputHolder::Window, self.x_window),
        };
        match input_holder(&self.xcb, self.x_window, &views.views) {
            None => return,
            Some(InputHolder::Other) if !take_from_other => return,
            Some(holder) if holder == target => return,
            Some(_) => {}
        }
        self.xcb
            .set_input_focus(
                xproto::InputFocus::PARENT,
                target_window,
                x11rb::CURRENT_TIME,
            )
            .log_err();
        xcb_flush(&self.xcb);
    }

    /// The native view a mouse event at this position reaches.
    fn hit_test_native_view(&self, position: Point<Pixels>) -> Option<NativeViewId> {
        let mut callback = self.callbacks.borrow_mut().hit_test_native_view.take()?;
        let id = callback(position);
        self.callbacks.borrow_mut().hit_test_native_view = Some(callback);
        id.filter(|id| self.state.borrow().native_views.shown(*id).is_some())
    }

    /// Handles a mouse press in a container, after GPUI was told of it, and
    /// returns whether the press goes on to the view.
    pub(crate) fn native_view_pressed(&self, position: Point<Pixels>) -> bool {
        let Some(id) = self.hit_test_native_view(position) else {
            return false;
        };
        if self.state.borrow().native_views.focused != Some(id) {
            let callback = self.callbacks.borrow_mut().native_view_focused.take();
            if let Some(mut callback) = callback {
                callback(id);
                self.callbacks.borrow_mut().native_view_focused = Some(callback);
            }
        }
        self.move_keyboard_input(true);
        true
    }

    /// Offers GPUI a key press ahead of the native view with keyboard input,
    /// and returns whether GPUI took it.
    pub(crate) fn offer_key_down_before_native_view(&self, event: KeyDownEvent) -> bool {
        let callback = self
            .callbacks
            .borrow_mut()
            .key_down_before_native_view
            .take();
        let Some(mut callback) = callback else {
            return false;
        };
        let taken = callback(event);
        self.callbacks.borrow_mut().key_down_before_native_view = Some(callback);
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtracting_leaves_what_the_cut_misses() {
        let area = Bounds::from_corners(Point::new(0, 0), Point::new(100, 100));
        let cut = Bounds::from_corners(Point::new(20, 20), Point::new(40, 50));
        let parts = subtract(&area, &cut);
        let covered = |parts: &[Bounds<i32>]| -> i32 {
            parts
                .iter()
                .map(|part| part.size.width * part.size.height)
                .sum()
        };
        assert_eq!(covered(&parts), 100 * 100 - 20 * 30);
        assert!(parts.iter().all(|part| !part.intersects(&cut)));
        let outside = Bounds::from_corners(Point::new(200, 0), Point::new(300, 10));
        assert_eq!(subtract(&area, &outside), [area]);
    }
}
