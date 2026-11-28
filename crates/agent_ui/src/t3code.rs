//! T3 Code's web interface, shown in the agent panel as a native WebKitGTK view.

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use gpui::{
    App, AppContext as _, BackgroundExecutor, Context, Entity, Global, Rgba, SharedString, Task,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};
use theme::ActiveTheme as _;
use ui::AgentThreadStatus;
use util::command::Stdio;

use crate::ThreadId;

/// The agent id T3 Code threads are stored under in the sidebar.
pub const AGENT_ID: &str = "t3code";

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub(crate) struct WebView {
    pub(crate) native_view: gpui::NativeView,
}

#[cfg(not(target_os = "linux"))]
impl WebView {
    pub(crate) fn navigate(&self, _: &str) -> Result<(), String> {
        Err("T3 Code can only be shown on Linux.".into())
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn new_web_view(
    _: &Pairing,
    _: &str,
    _: Rgba,
    _: &mut gpui::Window,
    _: &mut App,
) -> Result<WebView, String> {
    Err("T3 Code can only be shown on Linux.".into())
}

#[cfg(target_os = "linux")]
pub(crate) use linux::{WebView, new_web_view};

/// A thread in T3 Code, as `t3 thread list` reports it.
#[derive(Clone, Debug)]
pub struct T3Thread {
    pub thread_id: ThreadId,
    pub title: SharedString,
    pub updated_at: DateTime<Utc>,
    pub status: AgentThreadStatus,
    pub environment_id: SharedString,
    pub project_id: SharedString,
}

impl T3Thread {
    pub(crate) fn path(&self) -> String {
        format!(
            "/{}/{}",
            self.environment_id,
            self.thread_id.to_key_string()
        )
    }
}

/// T3 Code's threads for each workspace root an agent panel has registered, kept current.
pub struct T3CodeStore {
    workspaces: BTreeMap<PathBuf, T3Workspace>,
    poll_task: Option<Task<()>>,
}

#[derive(Default)]
struct T3Workspace {
    threads: Vec<T3Thread>,
    project_ready: bool,
    refresh_task: Option<Task<()>>,
}

struct GlobalT3CodeStore(Entity<T3CodeStore>);
impl Global for GlobalT3CodeStore {}

impl T3CodeStore {
    pub(crate) fn init(cx: &mut App) {
        if cx.has_global::<GlobalT3CodeStore>() {
            return;
        }
        let store = cx.new(|_| Self {
            workspaces: BTreeMap::new(),
            poll_task: None,
        });
        cx.set_global(GlobalT3CodeStore(store));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalT3CodeStore>()
            .map(|global| global.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalT3CodeStore>().0.clone()
    }

    pub fn threads(&self, root: &Path) -> &[T3Thread] {
        self.workspaces
            .get(root)
            .map_or(&[], |workspace| &workspace.threads)
    }

    pub fn thread(&self, thread_id: ThreadId) -> Option<&T3Thread> {
        self.workspaces
            .values()
            .flat_map(|workspace| &workspace.threads)
            .find(|thread| thread.thread_id == thread_id)
    }

    /// Lists `root`'s threads, and keeps them current from then on.
    pub fn ensure_workspace(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.workspaces.entry(root.clone()).or_default();
        self.refresh(root, cx);
        if self.poll_task.is_none() {
            self.poll_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(10))
                        .await;
                    let refreshed = this.update(cx, |this, cx| {
                        let roots = this.workspaces.keys().cloned().collect::<Vec<_>>();
                        for root in roots {
                            this.refresh(root, cx);
                        }
                    });
                    if refreshed.is_err() {
                        break;
                    }
                }
            }));
        }
    }

    fn refresh(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspaces.get_mut(&root) else {
            return;
        };
        if workspace.refresh_task.is_some() {
            return;
        }
        let add_project = !workspace.project_ready;
        workspace.refresh_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let threads = with_server(&executor, || async {
                if add_project {
                    ensure_project(&root).await?;
                }
                list_threads(&root).await
            })
            .await;
            this.update(cx, |this, cx| {
                let Some(workspace) = this.workspaces.get_mut(&root) else {
                    return;
                };
                workspace.refresh_task = None;
                match threads {
                    Ok(threads) => {
                        workspace.project_ready = true;
                        workspace.threads = threads;
                        cx.notify();
                    }
                    Err(error) => log::warn!("couldn't list T3 Code threads: {error:#}"),
                }
            })
            .ok();
        }));
    }
}

async fn ensure_project(root: &Path) -> Result<()> {
    let output = util::command::new_command("t3")
        .args(["project", "add"])
        .arg(root)
        .output()
        .await
        .context("couldn't run `t3 project add`")?;
    if output.status.success() {
        return Ok(());
    }
    let printed = printed(&output);
    if printed.contains("ProjectAlreadyExistsError") {
        return Ok(());
    }
    bail!("`t3 project add` failed: {printed}");
}

/// T3 Code's CLI prints its errors to stdout or stderr depending on its environment, so both are
/// read.
fn printed(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct T3ThreadEntry {
    thread_id: String,
    project_id: String,
    title: String,
    status: String,
    updated_at: DateTime<Utc>,
    link: String,
}

async fn list_threads(root: &Path) -> Result<Vec<T3Thread>> {
    let output = util::command::new_command("t3")
        .args(["thread", "list", "--project"])
        .arg(root)
        .args(["--json"])
        .output()
        .await
        .context("couldn't run `t3 thread list`")?;
    if !output.status.success() {
        bail!("`t3 thread list` failed: {}", printed(&output));
    }
    let entries: Vec<T3ThreadEntry> =
        serde_json::from_slice(&output.stdout).context("`t3 thread list` printed invalid JSON")?;
    let mut threads = entries
        .into_iter()
        .filter_map(|entry| {
            let thread_id = ThreadId::from_uuid(uuid::Uuid::parse_str(&entry.thread_id).ok()?);
            let environment_id = entry
                .link
                .split("/threads/")
                .nth(1)
                .and_then(|rest| rest.split('/').next())?;
            Some(T3Thread {
                thread_id,
                title: entry.title.into(),
                updated_at: entry.updated_at,
                status: match entry.status.as_str() {
                    "running" | "preparing" | "working" => AgentThreadStatus::Running,
                    "waiting" | "input" | "approval" => AgentThreadStatus::WaitingForConfirmation,
                    "failed" | "limited" => AgentThreadStatus::Error,
                    _ => AgentThreadStatus::Completed,
                },
                environment_id: environment_id.into(),
                project_id: entry.project_id.into(),
            })
        })
        .collect::<Vec<_>>();
    threads.sort_by_key(|thread| std::cmp::Reverse(thread.updated_at));
    Ok(threads)
}

/// The address of the running T3 Code server, and a one-time URL that signs a web view in to it.
pub(crate) struct Pairing {
    /// Like `http://127.0.0.1:3773`, which the paths of T3 Code's pages are relative to.
    pub(crate) origin: String,
    pub(crate) url: String,
}

/// Adds `workspace_root` to T3 Code as a project, so that the page can open it, and mints a
/// pairing with the server's own CLI.
pub(crate) async fn pairing(
    executor: BackgroundExecutor,
    workspace_root: Option<PathBuf>,
) -> Result<Pairing> {
    with_server(&executor, || async {
        if let Some(root) = &workspace_root {
            ensure_project(root).await?;
        }
        mint_pairing().await
    })
    .await
}

/// Runs `operation`, starting T3 Code's server and retrying when none is running.
async fn with_server<T, F: Future<Output = Result<T>>>(
    executor: &BackgroundExecutor,
    operation: impl Fn() -> F,
) -> Result<T> {
    // `t3` calls run one at a time, so that only one of them starts a server, and so that
    // `t3 project add`, which takes the server's place while none runs, can't race its start.
    static SERVER_LOCK: LazyLock<futures::lock::Mutex<()>> =
        LazyLock::new(|| futures::lock::Mutex::new(()));
    let _guard = SERVER_LOCK.lock().await;
    match operation().await {
        Err(error) if is_no_running_server(&error) => {}
        result => return result,
    }
    // The server isn't killed with Zed, like T3 Code's own background service, so that running
    // agent turns outlive it.
    let mut server = util::command::new_command("t3")
        .args(["serve", "--no-browser"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("couldn't start `t3 serve`")?;
    for _ in 0..60 {
        executor.timer(Duration::from_millis(500)).await;
        match operation().await {
            Err(error) if is_no_running_server(&error) => {}
            result => return result,
        }
        if let Some(status) = server.try_status()? {
            bail!("`t3 serve` exited with {status}");
        }
    }
    bail!("`t3 serve` didn't start within 30 seconds")
}

/// `t3 pair` reports a missing server as "No running T3 Code server found.", and the other
/// commands as "No running T3 Code server uses …".
fn is_no_running_server(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("No running T3 Code server")
}

async fn mint_pairing() -> Result<Pairing> {
    let output = util::command::new_command("t3")
        .args(["pair", "--ttl", "1m", "--label", "Zed"])
        .output()
        .await
        .context("couldn't run `t3 pair`")?;
    if !output.status.success() {
        bail!("`t3 pair` failed: {}", printed(&output));
    }
    let url = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Pairing URL: "))
        .map(|url| url.trim().to_string())
        .context("`t3 pair` printed no pairing URL")?;
    let (origin, _) = url
        .split_once("/pair")
        .context("`t3 pair` printed an unexpected pairing URL")?;
    Ok(Pairing {
        origin: origin.to_string(),
        url,
    })
}

/// The script that gives T3 Code the matching VS Code theme, opens `target`, and makes new
/// threads start in `workspace_root`, and the color of the theme's panels, to show while the
/// page loads.
pub(crate) fn setup_script(
    workspace_root: Option<&Path>,
    target: Option<&T3Thread>,
    cx: &App,
) -> (String, Rgba) {
    let theme = cx.theme();
    let panel = Rgba::from(theme.colors().panel_background);
    let setup = serde_json::json!({
        "theme": theme_for_t3(cx),
        "workspaceRoot": workspace_root.and_then(Path::to_str),
        "projectKey": target.map(|thread| format!("{}:{}", thread.environment_id, thread.project_id)),
        "targetPath": target.map(T3Thread::path),
    });
    let script = format!(
        "{{ const setup = {setup};
{} }}",
        include_str!("t3code.js")
    );
    (script, panel)
}

fn theme_for_t3(cx: &App) -> Option<serde_json::Value> {
    let zed_theme = cx.theme();
    let vs_code_theme = find_vs_code_theme(zed_theme.name.as_ref())?;
    let colors = vs_code_roles(&vs_code_theme, &zed_roles(zed_theme))?;
    let appearance = match vs_code_theme.kind.as_deref() {
        Some("light") | Some("hc-light") => "light",
        Some("dark") | Some("hc-black") => "dark",
        _ if zed_theme.appearance.is_light() => "light",
        _ => "dark",
    };
    Some(serde_json::json!({
        "id": "zed",
        "label": zed_theme.name,
        "appearance": appearance,
        "colors": colors,
    }))
}

#[derive(Deserialize)]
struct VsCodePackage {
    contributes: Option<VsCodeContributes>,
}

#[derive(Deserialize)]
struct VsCodeContributes {
    themes: Option<Vec<VsCodeThemeContribution>>,
}

#[derive(Deserialize)]
struct VsCodeThemeContribution {
    label: Option<String>,
    path: Option<String>,
}

#[derive(Deserialize)]
struct VsCodeTheme {
    #[serde(rename = "type")]
    kind: Option<String>,
    colors: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Finds the VS Code theme with the same name as Zed's active theme.
fn find_vs_code_theme(name: &str) -> Option<VsCodeTheme> {
    let home = util::paths::home_dir();
    for root in [
        ".vscode/extensions",
        ".vscode-oss/extensions",
        ".vscode-server/extensions",
    ] {
        let Ok(entries) = std::fs::read_dir(home.join(root)) else {
            continue;
        };
        for entry in entries.flatten() {
            let extension = entry.path();
            let Ok(package) = std::fs::read_to_string(extension.join("package.json")) else {
                continue;
            };
            let Ok(package) = serde_json_lenient::from_str::<VsCodePackage>(&package) else {
                continue;
            };
            let Some(themes) = package
                .contributes
                .and_then(|contributes| contributes.themes)
            else {
                continue;
            };
            for contribution in themes {
                let (Some(label), Some(path)) = (contribution.label, contribution.path) else {
                    continue;
                };
                if !label.eq_ignore_ascii_case(name) {
                    continue;
                }
                let Ok(theme) =
                    std::fs::read_to_string(extension.join(path.trim_start_matches("./")))
                else {
                    continue;
                };
                let Ok(theme) = serde_json_lenient::from_str::<VsCodeTheme>(&theme) else {
                    continue;
                };
                return Some(theme);
            }
        }
    }
    None
}

fn parse_hex(value: &str) -> Option<Rgba> {
    let hex = value.trim().trim_start_matches('#');
    if !hex.is_ascii() {
        return None;
    }
    let expand = |part: &str| u8::from_str_radix(part, 16).ok();
    let (r, g, b, a) = match hex.len() {
        3 | 4 => (
            expand(&hex[0..1].repeat(2))?,
            expand(&hex[1..2].repeat(2))?,
            expand(&hex[2..3].repeat(2))?,
            if hex.len() == 4 {
                expand(&hex[3..4].repeat(2))?
            } else {
                255
            },
        ),
        6 | 8 => (
            expand(&hex[0..2])?,
            expand(&hex[2..4])?,
            expand(&hex[4..6])?,
            if hex.len() == 8 {
                expand(&hex[6..8])?
            } else {
                255
            },
        ),
        _ => return None,
    };
    Some(Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: a as f32 / 255.0,
    })
}

/// The roles T3 Code uses, filled from Zed's theme so that every role has a value.
fn zed_roles(theme: &theme::Theme) -> BTreeMap<&'static str, Rgba> {
    let colors = theme.colors();
    let status = theme.status();
    let player = theme.players().local();
    // T3 Code ignores the transparency of a color, so translucent ones are shown over the panel.
    let panel = Rgba::from(colors.panel_background);
    let color = |color: gpui::Hsla| panel.blend(Rgba::from(color));
    [
        ("canvas", colors.panel_background),
        ("chrome", colors.panel_background),
        ("toolbar", colors.panel_background),
        ("toolbarForeground", colors.text),
        ("toolbarBorder", colors.border_variant),
        ("toolbarControl", colors.element_background),
        ("toolbarControlForeground", colors.text),
        ("toolbarControlHover", colors.element_hover),
        ("surface", colors.surface_background),
        ("surfaceRaised", colors.elevated_surface_background),
        ("surfaceOverlay", colors.elevated_surface_background),
        ("text", colors.text),
        ("textMuted", colors.text_muted),
        ("border", colors.border),
        ("input", colors.editor_background),
        ("focus", colors.border_focused),
        ("accent", colors.text_accent),
        ("accentForeground", colors.editor_background),
        ("secondary", colors.element_background),
        ("secondaryForeground", colors.text),
        ("muted", colors.element_background),
        ("mutedForeground", colors.text_muted),
        ("placeholder", colors.text_placeholder),
        ("secondaryLabel", colors.text_muted),
        ("iconMuted", colors.icon_muted),
        ("error", status.error),
        ("errorForeground", status.error),
        ("errorSurface", status.error_background),
        ("warning", status.warning),
        ("warningForeground", status.warning),
        ("warningSurface", status.warning_background),
        ("update", status.info),
        ("updateForeground", status.info),
        ("updateSurface", status.info_background),
        ("accentSurface", colors.element_selected),
        ("accentSurfaceForeground", colors.text),
        ("messageSurface", colors.element_background),
        ("messageForeground", colors.text),
        ("messageAction", colors.text_accent),
        ("messageActionForeground", colors.editor_background),
        ("messageActionHover", colors.link_text_hover),
        ("codeBackground", colors.editor_background),
        ("codeForeground", colors.editor_foreground),
        ("searchMatchBackground", colors.search_match_background),
        ("searchMatchForeground", colors.text),
        ("terminalBackground", colors.terminal_background),
        ("terminalForeground", colors.terminal_foreground),
        ("terminalCursor", player.cursor),
        ("terminalSelection", player.selection),
        ("terminalScrollbar", colors.scrollbar_thumb_background),
        (
            "terminalScrollbarHover",
            colors.scrollbar_thumb_hover_background,
        ),
    ]
    .into_iter()
    .map(|(role, color_value)| (role, color(color_value)))
    .collect()
}

/// Fills Zed's roles with the matching VS Code theme's workbench colors.
fn vs_code_roles(
    theme: &VsCodeTheme,
    base: &BTreeMap<&'static str, Rgba>,
) -> Option<BTreeMap<&'static str, Rgba>> {
    let colors = theme.colors.as_ref()?;
    let pick = |keys: &[&str]| -> Option<Rgba> {
        keys.iter().find_map(|key| {
            colors
                .get(*key)
                .and_then(|value| value.as_str())
                .and_then(parse_hex)
        })
    };
    let canvas = pick(&["editor.background", "editorPane.background"])?;
    let mut roles = base.clone();
    macro_rules! set {
        ($role:literal, $base:expr, $($key:literal),+) => {
            if let Some(color) = pick(&[$($key),+]) {
                roles.insert($role, $base.blend(color));
            }
        };
    }

    set!("canvas", canvas, "editor.background");
    set!(
        "chrome",
        canvas,
        "sideBar.background",
        "activityBar.background"
    );
    set!(
        "toolbar",
        canvas,
        "sideBar.background",
        "activityBar.background"
    );
    set!("toolbarForeground", canvas, "sideBar.foreground");
    set!("toolbarBorder", canvas, "sideBar.border", "panel.border");
    set!("toolbarControl", canvas, "input.background");
    set!("toolbarControlForeground", canvas, "sideBar.foreground");
    set!("toolbarControlHover", canvas, "list.hoverBackground");
    set!(
        "surface",
        canvas,
        "editorWidget.background",
        "dropdown.background"
    );
    set!(
        "surfaceRaised",
        canvas,
        "dropdown.background",
        "editorWidget.background"
    );
    set!(
        "surfaceOverlay",
        canvas,
        "menu.background",
        "quickInput.background",
        "dropdown.background"
    );
    set!("text", canvas, "editor.foreground", "foreground");
    set!(
        "textMuted",
        canvas,
        "disabledForeground",
        "descriptionForeground"
    );
    set!(
        "border",
        canvas,
        "panel.border",
        "editorGroup.border",
        "contrastBorder"
    );
    set!("input", canvas, "input.background", "input.border");
    set!("focus", canvas, "focusBorder");
    set!(
        "accent",
        canvas,
        "focusBorder",
        "button.background",
        "textLink.foreground"
    );
    set!("accentForeground", canvas, "button.foreground");
    set!("secondary", canvas, "list.activeSelectionBackground");
    set!(
        "secondaryForeground",
        canvas,
        "list.activeSelectionForeground"
    );
    set!("muted", canvas, "editorWidget.background");
    set!("mutedForeground", canvas, "disabledForeground");
    set!("placeholder", canvas, "input.placeholderForeground");
    set!("secondaryLabel", canvas, "disabledForeground");
    set!("iconMuted", canvas, "disabledForeground");
    set!("error", canvas, "editorError.foreground");
    set!("errorForeground", canvas, "editorError.foreground");
    set!("warning", canvas, "editorWarning.foreground");
    set!("warningForeground", canvas, "editorWarning.foreground");
    set!("update", canvas, "editorInfo.foreground");
    set!("updateForeground", canvas, "editorInfo.foreground");
    for (surface, key) in [
        ("errorSurface", "editorError.foreground"),
        ("warningSurface", "editorWarning.foreground"),
        ("updateSurface", "editorInfo.foreground"),
    ] {
        if let Some(color) = pick(&[key]) {
            roles.insert(surface, canvas.blend(Rgba { a: 0.15, ..color }));
        }
    }
    set!(
        "accentSurface",
        canvas,
        "list.activeSelectionBackground",
        "list.hoverBackground"
    );
    set!(
        "accentSurfaceForeground",
        canvas,
        "list.activeSelectionForeground"
    );
    set!(
        "messageSurface",
        canvas,
        "editorWidget.background",
        "dropdown.background"
    );
    set!(
        "messageForeground",
        canvas,
        "editor.foreground",
        "foreground"
    );
    set!("messageAction", canvas, "button.background");
    set!("messageActionForeground", canvas, "button.foreground");
    set!("messageActionHover", canvas, "button.hoverBackground");
    set!(
        "codeBackground",
        canvas,
        "textCodeBlock.background",
        "editorWidget.background"
    );
    set!("codeForeground", canvas, "editor.foreground");
    set!(
        "searchMatchBackground",
        canvas,
        "editor.findMatchHighlightBackground"
    );
    set!(
        "searchMatchForeground",
        canvas,
        "editor.findMatchHighlightForeground",
        "editor.foreground"
    );
    set!(
        "terminalBackground",
        canvas,
        "terminal.background",
        "panel.background"
    );
    set!("terminalForeground", canvas, "terminal.foreground");
    set!(
        "terminalCursor",
        canvas,
        "terminalCursor.foreground",
        "editorCursor.foreground"
    );
    set!(
        "terminalSelection",
        canvas,
        "terminal.selectionBackground",
        "editor.selectionBackground"
    );
    set!("terminalScrollbar", canvas, "scrollbarSlider.background");
    Some(roles)
}

/// WebKitGTK is loaded when T3 Code is first shown, so that building Zed doesn't need it.
#[cfg(target_os = "linux")]
mod linux {
    use super::Pairing;
    use gpui::{App, NativeView, Rgba, Window};
    use std::{
        ffi::{CStr, CString, c_char, c_int, c_ulong, c_void},
        ptr::{self, NonNull},
        sync::OnceLock,
        time::Duration,
    };

    type Object = *mut c_void;

    /// T3 Code's page, in a WebKitGTK web view that is embedded in the window.
    pub(crate) struct WebView {
        pub(crate) native_view: NativeView,
        origin: String,
        page: Object,
    }

    impl WebView {
        /// Shows `path` on T3 Code's origin in this same page, so that it stays signed in.
        pub(crate) fn navigate(&self, path: &str) -> Result<(), String> {
            let url = CString::new(format!("{}{path}", self.origin))
                .map_err(|error| error.to_string())?;
            // SAFETY: GTK keeps `page` alive with its window, which this module never destroys,
            // and the signature matches WebKitGTK's.
            unsafe {
                function::<unsafe extern "C" fn(Object, *const c_char)>(
                    c"webkit_web_view_load_uri",
                )?(self.page, url.as_ptr());
            }
            Ok(())
        }
    }

    /// # Safety
    ///
    /// `F` must be a function pointer type matching the C function's signature.
    unsafe fn function<F>(name: &CStr) -> Result<F, String> {
        // SAFETY: `name` is nul-terminated, and the caller guarantees `F`.
        unsafe {
            let function = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
            if function.is_null() {
                return Err(format!("WebKitGTK has no {name:?}"));
            }
            Ok(std::mem::transmute_copy::<*mut c_void, F>(&function))
        }
    }

    /// Loads WebKitGTK and starts GTK on the X display GPUI uses, once.
    fn start_gtk(cx: &mut App) -> Result<(), String> {
        static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
        let mut started_now = false;
        let result = STARTED.get_or_init(|| {
            started_now = true;
            let library = c"libwebkit2gtk-4.1.so.0";
            // SAFETY: GTK is started once, on the main thread, with matching signatures.
            unsafe {
                if libc::dlopen(library.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL).is_null() {
                    return Err(format!("Couldn't load {library:?}."));
                }
                function::<unsafe extern "C" fn(*const c_char)>(c"gdk_set_allowed_backends")?(
                    c"x11".as_ptr(),
                );
                let init = function::<unsafe extern "C" fn(*mut c_int, *mut c_void) -> c_int>(
                    c"gtk_init_check",
                )?;
                if init(ptr::null_mut(), ptr::null_mut()) == 0 {
                    return Err("GTK couldn't open the X display.".into());
                }
            }
            Ok(())
        });
        if started_now && result.is_ok() {
            // SAFETY: The signatures match GTK's.
            let (pending, iterate) = unsafe {
                (
                    function::<unsafe extern "C" fn() -> c_int>(c"gtk_events_pending")?,
                    function::<unsafe extern "C" fn(c_int) -> c_int>(c"gtk_main_iteration_do")?,
                )
            };
            // GPUI doesn't run a GLib main loop, so GTK's events are processed on a timer.
            cx.spawn(async move |cx| {
                loop {
                    // SAFETY: GTK was started on this thread, which runs GPUI's foreground tasks.
                    unsafe {
                        while pending() != 0 {
                            iterate(0);
                        }
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(8))
                        .await;
                }
            })
            .detach();
        }
        result.clone()
    }

    /// Shows `url` in a web view that runs `script` before each page, and that is `background`
    /// until a page paints.
    pub(crate) fn new_web_view(
        pairing: &Pairing,
        script: &str,
        background: Rgba,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<WebView, String> {
        let url = CString::new(pairing.url.as_str()).map_err(|error| error.to_string())?;
        let script = CString::new(script).map_err(|error| error.to_string())?;
        start_gtk(cx)?;
        // SAFETY: GTK was started on this thread, the signatures match WebKitGTK's,
        // and the X window is realized but in no other window when it is embedded.
        unsafe {
            const TOPLEVEL: c_int = 0;
            const TOP_FRAME: c_int = 1;
            const AT_DOCUMENT_START: c_int = 0;
            let gtk_window =
                function::<unsafe extern "C" fn(c_int) -> Object>(c"gtk_window_new")?(TOPLEVEL);
            let web_view = function::<unsafe extern "C" fn() -> Object>(c"webkit_web_view_new")?();
            // WebKit's GPU compositing, embedded in another X11 client's window, paints nothing on
            // NVIDIA's driver, so the page is rendered in software.
            const HARDWARE_ACCELERATION_NEVER: c_int = 2;
            let settings = function::<unsafe extern "C" fn(Object) -> Object>(
                c"webkit_web_view_get_settings",
            )?(web_view);
            function::<unsafe extern "C" fn(Object, c_int)>(
                c"webkit_settings_set_hardware_acceleration_policy",
            )?(settings, HARDWARE_ACCELERATION_NEVER);
            // A `GdkRGBA`, which is four doubles.
            let background =
                [background.r, background.g, background.b, background.a].map(f64::from);
            function::<unsafe extern "C" fn(Object, *const f64)>(
                c"webkit_web_view_set_background_color",
            )?(web_view, background.as_ptr());
            let content_manager = function::<unsafe extern "C" fn(Object) -> Object>(
                c"webkit_web_view_get_user_content_manager",
            )?(web_view);
            let user_script = function::<
                unsafe extern "C" fn(
                    *const c_char,
                    c_int,
                    c_int,
                    *const c_void,
                    *const c_void,
                ) -> Object,
            >(c"webkit_user_script_new")?(
                script.as_ptr(),
                TOP_FRAME,
                AT_DOCUMENT_START,
                ptr::null(),
                ptr::null(),
            );
            function::<unsafe extern "C" fn(Object, Object)>(
                c"webkit_user_content_manager_add_script",
            )?(content_manager, user_script);
            function::<unsafe extern "C" fn(Object)>(c"webkit_user_script_unref")?(user_script);
            function::<unsafe extern "C" fn(Object, Object)>(c"gtk_container_add")?(
                gtk_window, web_view,
            );
            function::<unsafe extern "C" fn(Object, *const c_char)>(c"webkit_web_view_load_uri")?(
                web_view,
                url.as_ptr(),
            );
            function::<unsafe extern "C" fn(Object)>(c"gtk_widget_realize")?(gtk_window);
            // The window has to exist on the server before GPUI, on its own
            // connection, can embed it.
            let display =
                function::<unsafe extern "C" fn() -> Object>(c"gdk_display_get_default")?();
            function::<unsafe extern "C" fn(Object)>(c"gdk_display_sync")?(display);
            let gdk_window = function::<unsafe extern "C" fn(Object) -> Object>(
                c"gtk_widget_get_window",
            )?(gtk_window);
            let x_window = function::<unsafe extern "C" fn(Object) -> c_ulong>(
                c"gdk_x11_window_get_xid",
            )?(gdk_window);
            let x_window = NonNull::new(x_window as usize as *mut c_void)
                .ok_or("GTK didn't create an X window.")?;
            let native_view = NativeView::new(x_window, window, cx).map_err(|_| {
                "T3 Code needs Zed on X11. Start Zed with WAYLAND_DISPLAY unset to run it through XWayland."
                    .to_string()
            })?;
            function::<unsafe extern "C" fn(Object)>(c"gtk_widget_show_all")?(gtk_window);
            Ok(WebView {
                native_view,
                origin: pairing.origin.clone(),
                page: web_view,
            })
        }
    }
}
