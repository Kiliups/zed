//! Shows a live WebKitGTK web view composited into a GPUI window on X11.
//!
//! It needs X11, or XWayland with `WAYLAND_DISPLAY` unset, and WebKitGTK 4.1
//! where the dynamic loader finds it.
//!
//! Things to try:
//! - Press `ctrl-shift-p` while typing in the page. The palette opens across the page's
//!   edge, takes keyboard input, and `escape` returns the caret to the same page field.
//! - Click the palette's button where it overlaps the page, then scroll the exposed page.
//! - Click the page while the palette is open, which dismisses the palette.
//! - Click the GPUI text box and type with the pointer over the page.
//! - Press `ctrl-k` in the page, which it keeps for itself, while `ctrl-shift-p` is
//!   reserved, so it opens the palette whatever the page does.

#[cfg(target_os = "linux")]
#[path = "example_support/fonts.rs"]
mod example_support;

#[cfg(target_os = "linux")]
mod example {
    use gpui::{
        App, Context, FocusHandle, KeyBinding, KeyDownEvent, NativeView, Window, WindowOptions,
        actions, anchored, deferred, div, point, prelude::*, px, rgb,
    };
    use std::{
        ffi::{CStr, c_char, c_int, c_ulong, c_void},
        ptr::{self, NonNull},
        time::Duration,
    };

    actions!(native_view_example, [TogglePalette, DismissPalette]);

    const PAGE: &CStr = cr#"<!doctype html>
<html>
  <body style="font: 15px sans-serif; margin: 24px; color: #222; background: #fdf6e3">
    <h2>A live WebKitGTK web view</h2>
    <p><input placeholder="Type here, then press ctrl-shift-p" size="36" autofocus /></p>
    <p id="kept">This page keeps ctrl-k for itself.</p>
    <button onclick="this.textContent = 'Clicked ' + (++window.clicks || (window.clicks = 1))">
      A page button
    </button>
    <script>
      document.addEventListener("keydown", (event) => {
        if (event.ctrlKey && event.key === "k") {
          event.preventDefault();
          window.keptCount = (window.keptCount || 0) + 1;
          document.getElementById("kept").textContent =
            "This page kept ctrl-k " + window.keptCount + " times.";
        }
      });
      for (let line = 1; line <= 60; line++) {
        const paragraph = document.createElement("p");
        paragraph.textContent = "Scrollable line " + line;
        document.body.appendChild(paragraph);
      }
    </script>
  </body>
</html>"#;

    /// Looks up a function of WebKitGTK or the GTK libraries it loaded.
    ///
    /// # Safety
    ///
    /// `F` must be a function pointer type matching the C function's signature.
    unsafe fn function<F>(name: &CStr) -> F {
        // SAFETY: `name` is nul-terminated, and the caller guarantees `F`.
        unsafe {
            let function = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
            assert!(!function.is_null(), "WebKitGTK has no {name:?}");
            std::mem::transmute_copy::<*mut c_void, F>(&function)
        }
    }

    type Object = *mut c_void;

    /// Starts GTK on the X display GPUI uses, and returns the X window of a GTK
    /// window holding a web view, which is realized but not mapped yet.
    ///
    /// # Safety
    ///
    /// This must run once, on the main thread.
    unsafe fn create_web_view(cx: &mut App) -> Result<(Object, c_ulong), String> {
        unsafe {
            let library = c"libwebkit2gtk-4.1.so.0";
            if libc::dlopen(library.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL).is_null() {
                return Err(format!("couldn't load {library:?}"));
            }
            function::<unsafe extern "C" fn(*const c_char)>(c"gdk_set_allowed_backends")(
                c"x11".as_ptr(),
            );
            let init = function::<unsafe extern "C" fn(*mut c_int, *mut c_void) -> c_int>(
                c"gtk_init_check",
            );
            if init(ptr::null_mut(), ptr::null_mut()) == 0 {
                return Err("GTK couldn't open the X display".into());
            }
            // GPUI doesn't run a GLib main loop, so GTK's events are processed on a timer.
            let pending = function::<unsafe extern "C" fn() -> c_int>(c"gtk_events_pending");
            let iterate =
                function::<unsafe extern "C" fn(c_int) -> c_int>(c"gtk_main_iteration_do");
            cx.spawn(async move |cx| {
                loop {
                    // GTK was started on this thread, which runs GPUI's foreground tasks.
                    while pending() != 0 {
                        iterate(0);
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(8))
                        .await;
                }
            })
            .detach();

            const TOPLEVEL: c_int = 0;
            let window =
                function::<unsafe extern "C" fn(c_int) -> Object>(c"gtk_window_new")(TOPLEVEL);
            let web_view = function::<unsafe extern "C" fn() -> Object>(c"webkit_web_view_new")();
            function::<unsafe extern "C" fn(Object, Object)>(c"gtk_container_add")(
                window, web_view,
            );
            function::<unsafe extern "C" fn(Object, *const c_char, *const c_char)>(
                c"webkit_web_view_load_html",
            )(web_view, PAGE.as_ptr(), ptr::null());
            function::<unsafe extern "C" fn(Object)>(c"gtk_widget_realize")(window);
            // The window has to exist on the server before GPUI, on its own
            // connection, can embed it.
            let display =
                function::<unsafe extern "C" fn() -> Object>(c"gdk_display_get_default")();
            function::<unsafe extern "C" fn(Object)>(c"gdk_display_sync")(display);
            let gdk_window = function::<unsafe extern "C" fn(Object) -> Object>(
                c"gtk_widget_get_window",
            )(window);
            let x_window = function::<unsafe extern "C" fn(Object) -> c_ulong>(
                c"gdk_x11_window_get_xid",
            )(gdk_window);
            Ok((window, x_window))
        }
    }

    struct Example {
        web_view: NativeView,
        text_box_focus: FocusHandle,
        text_box_text: String,
        palette_focus: FocusHandle,
        palette_text: String,
        palette_clicks: usize,
        focus_before_palette: Option<FocusHandle>,
    }

    impl Example {
        fn new(
            gtk_window: Object,
            x_window: c_ulong,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Self {
            let x_window = NonNull::new(x_window as usize as *mut c_void).expect("an X window");
            // SAFETY: The pointer is the ID of the GTK window's X window, which is
            // not in another window, on the X display GPUI uses.
            let web_view = unsafe { NativeView::new(x_window, window, cx) }
                .expect("native views need X11; unset WAYLAND_DISPLAY to run through XWayland");
            // SAFETY: The GTK window is live, on the main thread. It is mapped only
            // once it is inside GPUI's window, which shows it.
            unsafe { function::<unsafe extern "C" fn(Object)>(c"gtk_widget_show_all")(gtk_window) };
            // The page sees keys before GPUI does, so without this a page that
            // handled this key itself would keep the palette from opening.
            web_view.reserve_action::<TogglePalette>();
            Self {
                web_view,
                text_box_focus: cx.focus_handle(),
                text_box_text: String::new(),
                palette_focus: cx.focus_handle(),
                palette_text: String::new(),
                palette_clicks: 0,
                focus_before_palette: None,
            }
        }

        fn toggle_palette(
            &mut self,
            _: &TogglePalette,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            if self.focus_before_palette.is_some() {
                self.dismiss_palette(&DismissPalette, window, cx);
                return;
            }
            self.focus_before_palette = Some(
                window
                    .focused(cx)
                    .unwrap_or_else(|| self.text_box_focus.clone()),
            );
            window.focus(&self.palette_focus, cx);
            cx.notify();
        }

        fn dismiss_palette(
            &mut self,
            _: &DismissPalette,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            if let Some(previous_focus) = self.focus_before_palette.take() {
                window.focus(&previous_focus, cx);
                cx.notify();
            }
        }

        fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
            deferred(
                anchored().position(point(px(170.), px(120.))).child(
                    div()
                        .id("palette")
                        .track_focus(&self.palette_focus)
                        .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                            this.dismiss_palette(&DismissPalette, window, cx);
                        }))
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                            if let Some(text) = &event.keystroke.key_char {
                                this.palette_text.push_str(text);
                                cx.notify();
                            }
                        }))
                        .occlude()
                        .w(px(320.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_lg()
                        .shadow_xl()
                        .bg(rgb(0x1f2933))
                        .text_color(gpui::white())
                        .child("A GPUI palette over the page")
                        .child(
                            div()
                                .p_2()
                                .rounded_md()
                                .bg(rgb(0x323f4b))
                                .child(format!("Typed: {}", self.palette_text)),
                        )
                        .child(
                            div()
                                .id("palette-button")
                                .p_2()
                                .rounded_md()
                                .bg(rgb(0x3e7bfa))
                                .cursor_pointer()
                                .child(format!("Clicked {} times", self.palette_clicks))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.palette_clicks += 1;
                                    cx.notify();
                                })),
                        ),
                ),
            )
            .with_priority(1)
        }
    }

    impl Render for Example {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let text_box_focused = self.text_box_focus.is_focused(window);
            div()
                .size_full()
                .flex()
                .bg(rgb(0xf0f4f8))
                .text_color(rgb(0x1f2933))
                .on_action(cx.listener(Self::toggle_palette))
                .on_action(cx.listener(Self::dismiss_palette))
                .child(
                    div()
                        .flex_none()
                        .w(px(240.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child("GPUI content")
                        .child(
                            div()
                                .id("text-box")
                                .track_focus(&self.text_box_focus)
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if let Some(text) = &event.keystroke.key_char {
                                        this.text_box_text.push_str(text);
                                        cx.notify();
                                    }
                                }))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    window.focus(&this.text_box_focus, cx);
                                }))
                                .min_h(px(60.))
                                .p_2()
                                .rounded_md()
                                .border_1()
                                .border_color(if text_box_focused {
                                    rgb(0x3e7bfa)
                                } else {
                                    rgb(0x9aa5b1)
                                })
                                .bg(gpui::white())
                                .child(format!("Click, then type: {}", self.text_box_text)),
                        )
                        .child("ctrl-shift-p toggles the palette, escape dismisses it."),
                )
                .child(div().flex_1().child(self.web_view.clone()))
                .when(self.focus_before_palette.is_some(), |this| {
                    this.child(self.render_palette(cx))
                })
        }
    }

    pub fn run() {
        gpui_platform::application().run(|cx: &mut App| {
            if !crate::example_support::load_fonts(cx) {
                return;
            }
            // SAFETY: This runs once, on the main thread.
            let (gtk_window, x_window) = match unsafe { create_web_view(cx) } {
                Ok(web_view) => web_view,
                Err(error) => {
                    eprintln!("{error}");
                    cx.quit();
                    return;
                }
            };
            cx.bind_keys([
                KeyBinding::new("ctrl-shift-p", TogglePalette, None),
                KeyBinding::new("escape", DismissPalette, None),
            ]);
            cx.open_window(WindowOptions::default(), |window, cx| {
                cx.new(|cx| Example::new(gtk_window, x_window, window, cx))
            })
            .expect("opening the example window");
            cx.activate(true);
        });
    }
}

#[cfg(target_os = "linux")]
fn main() {
    example::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("This example shows a WebKitGTK view on Linux, under X11.");
}
