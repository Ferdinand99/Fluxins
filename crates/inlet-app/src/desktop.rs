// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inlet as a desktop app: one running instance, the tray icon with the unread state,
//! the dock or taskbar badge, closing to the tray, starting at login, `inlet://` and
//! invite links, the dock menu (macOS) and jump list (Windows), the window's place per
//! monitor, and zoom. The rules that need no gpui are in `backend::platform`; this file
//! wires them to the app.
//!
//! - **One instance.** `main` asks [`claim_instance`] first. A later start hands its
//!   command line to the running Inlet and exits; that one comes to the front. Any
//!   error (another Inlet holds the lock but does not answer, say) shows
//!   [`run_start_error`] and exits: Inlet never runs twice.
//! - **Tray and badge** only through `inlet_platform::gpui_glue`, which runs each call
//!   in a task outside gpui's borrow of the app.
//! - **Closing to the tray** only while a tray icon is shown. On Linux that is checked
//!   each time (the tray host can go away); without one the window closes normally.
//! - **Links** go through [`LinkGate`] and always end in a prompt (see `links`).

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    actions, div, point, prelude::*, px, size, AnyWindowHandle, App, Bounds, Context, Focusable,
    FontWeight, Global, KeyBinding, TitlebarOptions, Window, WindowBounds, WindowHandle,
    WindowOptions,
};
use inlet_platform::{
    autostart, deep_link, gpui_glue, Badge, DeepLink, Icon, Instance, PlatformEvent, Tray,
    TrayItem, TrayState,
};
use inlet_state::{AccountState, ChannelId, GuildId, Unread};

use crate::app::{CurrentBackend, Inlet, Screen};
use crate::backend::accounts;
use crate::backend::platform::links::{
    self, Incoming, LinkGate, LinkQueue, Lookup, Next, Prompt, Session,
};
use crate::backend::platform::prefs::{self, DesktopPrefs};
use crate::backend::platform::window_state::{self, Monitor, Placement, Rect};
use crate::backend::platform::{
    self as platform, Ev, SecondLaunch, DOCK_QUIT, TRAY_OPEN, TRAY_QUIT,
};
use crate::model::clean_name;
use crate::ui::widgets::button;
use crate::{assets, theme};

actions!(desktop, [ShowInlet, ZoomIn, ZoomOut, ZoomReset, CloseLayer]);

/// The key context of a layer over the chat (link prompt, settings view).
pub const LAYER_CONTEXT: &str = "InletLayer";

/// The rem size at zoom 100 %.
const BASE_REM: f32 = 16.0;
/// The window's size before anything is remembered.
const DEFAULT_SIZE: (f32, f32) = (1200.0, 780.0);
/// Zoom in (= and +), out and back to 100 %: Command on macOS, Ctrl elsewhere.
#[cfg(target_os = "macos")]
const ZOOM_KEYS: [&str; 4] = ["cmd-=", "cmd-+", "cmd--", "cmd-0"];
#[cfg(not(target_os = "macos"))]
const ZOOM_KEYS: [&str; 4] = ["ctrl-=", "ctrl-+", "ctrl--", "ctrl-0"];

/// How long the window must stay put before its place is written.
const SAVE_AFTER: Duration = Duration::from_secs(2);

const TRAY_PNG: &[u8] =
    include_bytes!("../../../assets/brand/linux/hicolor/32x32/apps/no.opland.inlet.png");

/// Demo builds with sample data write nothing and register nothing.
fn demo_mode() -> bool {
    cfg!(feature = "demo") && std::env::var_os("INLET_DEMO").is_some()
}

/// How this process was started.
#[derive(Debug, PartialEq, Eq)]
pub struct Start {
    /// The login item asked for a start hidden in the tray.
    pub hidden: bool,
    /// The `inlet://` link Inlet was started with (the registered handler passes it
    /// after `--`). It is asked about once the account is ready, like any other link.
    pub link: Option<DeepLink>,
    /// The jump list's "Quit Inlet" started Inlet, which was not running: nothing to
    /// quit, so this start ends at once.
    pub quit_at_once: bool,
}

impl Start {
    /// Reads Inlet's own command line.
    pub fn from_env() -> Self {
        let args: Vec<String> = std::env::args_os()
            .skip(1)
            .filter_map(|a| a.into_string().ok())
            .collect();
        Self::from_args(&args)
    }

    /// Only options before any `--` count; the link is taken with `deep_link::extract`.
    pub fn from_args(args: &[String]) -> Self {
        let launch = SecondLaunch::from_args(args);
        Start {
            hidden: autostart::start_minimized(args),
            link: launch.link,
            quit_at_once: launch.dock_action == Some(DOCK_QUIT),
        }
    }

    /// Whether the window opens hidden. Only where a tray is certain (Windows, and
    /// macOS with its dock icon); Linux opens it and minimizes it once a tray icon is
    /// shown.
    fn opens_hidden(&self) -> bool {
        self.hidden && cfg!(any(windows, target_os = "macos"))
    }
}

/// Becomes the one running Inlet. `Ok(None)`: another Inlet runs and has this start's
/// command line; exit. `Err`: exit after [`run_start_error`].
pub fn claim_instance() -> inlet_platform::Result<Option<mpsc::Receiver<Vec<String>>>> {
    // Demo runs (screenshots) never meet a real Inlet of the same user.
    let id = if demo_mode() {
        "no.opland.inlet-demo"
    } else {
        inlet_platform::APP_ID
    };
    match inlet_platform::single_instance(id)? {
        Instance::Primary(listener) => Ok(Some(listener)),
        Instance::Secondary => Ok(None),
    }
}

/// Shows why Inlet cannot start in a small window, then exits.
pub fn run_start_error(err: inlet_platform::Error) -> ! {
    log::error!("cannot start: {err}");
    let detail = err.to_string();
    gpui_platform::application().run(move |cx: &mut App| {
        assets::load_fonts(cx);
        let bounds = Bounds::centered(None, size(px(480.), px(220.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(t!("app-window-title").into()),
                ..Default::default()
            }),
            app_id: Some(inlet_platform::APP_ID.into()),
            ..Default::default()
        };
        let detail = detail.clone();
        if cx
            .open_window(options, |_, cx| cx.new(|_| StartError { detail }))
            .is_err()
        {
            cx.quit();
            return;
        }
        // Exit here with 1: on macOS `run` never returns and `quit` would exit with 0.
        cx.on_window_closed(|_, _| std::process::exit(1)).detach();
        cx.activate(true);
    });
    std::process::exit(1)
}

struct StartError {
    detail: String,
}

impl Render for StartError {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .p_6()
            .flex()
            .flex_col()
            .gap_3()
            .bg(theme::bg_chat())
            .text_color(theme::text())
            .font_family(assets::ui_font())
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_strong())
                    .child(t!("desktop-start-error-title")),
            )
            .child(t!("desktop-start-error-body", error = self.detail.as_str()))
            .child(
                div().flex().flex_row().child(
                    button("start-error-close", t!("desktop-start-error-close"), true)
                        .on_click(cx.listener(|_, _, _, _| std::process::exit(1))),
                ),
            )
    }
}

/// What the desktop side keeps, as a gpui global.
pub struct Desktop {
    pub prefs: DesktopPrefs,
    /// `settings.json`; `None` in demo runs, which write nothing.
    path: Option<PathBuf>,
    tray: Option<Rc<Tray>>,
    /// Whether the login item is on; `None` until the system has answered.
    pub autostart: Option<bool>,
    /// The last error from changing the login item, for the settings tab.
    pub autostart_error: Option<String>,
    window: Option<AnyWindowHandle>,
    gate: LinkGate,
    /// A link waiting for the account to be ready.
    links: LinkQueue,
    last_unread: Option<DesktopUnread>,
    /// When the window last moved, while that is not written yet.
    moved_at: Option<Instant>,
}

impl Global for Desktop {}

impl Desktop {
    fn new(prefs: DesktopPrefs, path: Option<PathBuf>) -> Self {
        Desktop {
            prefs,
            path,
            tray: None,
            autostart: None,
            autostart_error: None,
            window: None,
            gate: LinkGate::default(),
            links: LinkQueue::default(),
            last_unread: None,
            moved_at: None,
        }
    }

    /// Another account became active: a link waiting for the one before is dropped.
    pub fn forget_waiting_link(&mut self) {
        self.links.clear();
    }

    /// Whether a tray icon is shown right now.
    pub fn tray_shown(&self) -> bool {
        self.tray.as_ref().is_some_and(|t| t.has_host())
    }

    fn save(&mut self) {
        self.moved_at = None;
        if let Some(path) = &self.path {
            if let Err(err) = prefs::save(path, &self.prefs) {
                log::warn!("could not write the desktop settings: {err}");
            }
        }
    }
}

/// Sets up the desktop side, before the main window opens.
pub fn init(cx: &mut App) {
    let path = if demo_mode() {
        None
    } else {
        prefs::settings_path()
    };
    let prefs = path.as_deref().map(prefs::load).unwrap_or_default();
    cx.set_global(Desktop::new(prefs, path));
    gpui_glue::install(cx);
    install_actions(cx);
    // The dock menu on macOS (Quit is already in it), and the jump list on Windows:
    // gpui builds its jump list on a thread without COM, so Inlet builds its own.
    // Its entries start Inlet with `--dock-action <n>`, see `SecondLaunch`.
    #[cfg(target_os = "macos")]
    cx.set_dock_menu(vec![gpui::MenuItem::action(
        t!("desktop-tray-open"),
        ShowInlet,
    )]);
    #[cfg(windows)]
    if let Err(err) = inlet_platform::jump_list::set_tasks(vec![
        inlet_platform::jump_list::JumpTask {
            title: t!("desktop-tray-open"),
            args: format!("--dock-action {}", platform::DOCK_OPEN),
        },
        inlet_platform::jump_list::JumpTask {
            title: t!("desktop-tray-quit"),
            args: format!("--dock-action {DOCK_QUIT}"),
        },
    ]) {
        log::warn!("jump list: {err}");
    }

    if !demo_mode() {
        // Release builds make this executable the handler of inlet:// links (Windows:
        // the user's registry, Linux: a hidden desktop entry; macOS: the bundle's
        // Info.plist). Debug builds leave an installed Inlet's handler alone.
        if !cfg!(debug_assertions) {
            cx.background_spawn(async {
                if let Err(err) = deep_link::register(deep_link::SCHEME) {
                    log::info!("inlet:// links not registered: {err}");
                }
            })
            .detach();
        }
        let asked = cx.background_spawn(async { autostart::get() });
        cx.spawn(async move |cx| {
            let on = asked.await;
            cx.update_global::<Desktop, _>(|d, _| match on {
                Ok(on) => d.autostart = Some(on),
                Err(err) => log::info!("could not read the login item: {err}"),
            });
        })
        .detach();
    }
}

/// The keys and the app-wide handlers of this file's actions.
fn install_actions(cx: &mut App) {
    let [zoom_in, zoom_in_plus, zoom_out, zoom_reset] = ZOOM_KEYS;
    cx.bind_keys([
        KeyBinding::new(zoom_in, ZoomIn, None),
        KeyBinding::new(zoom_in_plus, ZoomIn, None),
        KeyBinding::new(zoom_out, ZoomOut, None),
        KeyBinding::new(zoom_reset, ZoomReset, None),
        // A layer over the chat (link prompt, settings) closes with Escape; this
        // binding is in the layer's own context, so it wins over the chat's Escape.
        KeyBinding::new("escape", CloseLayer, Some(LAYER_CONTEXT)),
    ]);
    cx.on_action(|_: &ShowInlet, cx| show_main_window(cx));
    cx.on_action(|_: &ZoomIn, cx| zoom_by(1, cx));
    cx.on_action(|_: &ZoomOut, cx| zoom_by(-1, cx));
    cx.on_action(|_: &ZoomReset, cx| set_zoom(1.0, cx));
}

/// The main window's options: back where it was on its monitor, or centred.
pub fn window_options(start: &Start, cx: &mut App) -> WindowOptions {
    let memory = cx.global::<Desktop>().prefs.window.clone();
    let displays = cx.displays();
    let monitors: Vec<Monitor> = displays
        .iter()
        .filter_map(|d| {
            Some(Monitor {
                id: d.uuid().ok()?.to_string(),
                area: window_space(d.visible_bounds()),
            })
        })
        .collect();
    let main = cx.primary_display().map(|d| {
        let s = d.visible_bounds().size;
        (s.width.as_f32(), s.height.as_f32())
    });
    let (bounds, display_id) = match window_state::place(&memory, &monitors, main, DEFAULT_SIZE) {
        Placement::Restore {
            monitor,
            rect,
            maximized,
        } => {
            let display = displays
                .iter()
                .find(|d| d.uuid().ok().map(|u| u.to_string()).as_deref() == Some(&monitor));
            let b = Bounds::new(point(px(rect.x), px(rect.y)), size(px(rect.w), px(rect.h)));
            let bounds = if maximized {
                WindowBounds::Maximized(b)
            } else {
                WindowBounds::Windowed(b)
            };
            (bounds, display.map(|d| d.id()))
        }
        Placement::Centre { w, h } => (
            WindowBounds::Windowed(Bounds::centered(None, size(px(w), px(h)), cx)),
            None,
        ),
    };
    // A scene file of the visual checks fixes the size, whatever was remembered. Under
    // Xvfb the pointer cannot be moved and stays in the middle of the screen, so on
    // Linux the window goes to the top left corner, clear of it (the script makes the
    // screen more than twice the window's size); elsewhere the scripts park the pointer
    // beside a centred window. Either way nothing shows a hover state.
    #[cfg(feature = "demo")]
    let (bounds, display_id) = match crate::demo::scenes::window_size() {
        Some((w, h)) => {
            let b = if cfg!(any(target_os = "linux", target_os = "freebsd")) {
                Bounds::new(point(px(0.), px(0.)), size(px(w), px(h)))
            } else {
                Bounds::centered(None, size(px(w), px(h)), cx)
            };
            (WindowBounds::Windowed(b), None)
        }
        None => (bounds, display_id),
    };
    let hidden = start.opens_hidden();
    WindowOptions {
        window_bounds: Some(bounds),
        display_id,
        show: !hidden,
        focus: !hidden,
        titlebar: Some(TitlebarOptions {
            title: Some(t!("app-window-title").into()),
            ..Default::default()
        }),
        app_id: Some(inlet_platform::APP_ID.into()),
        // X11 only; Windows takes the icon from the .exe resource (build.rs).
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        icon: crate::brand_icon::window_icon(),
        ..Default::default()
    }
}

/// A monitor's usable area (without task bar or dock) in the coordinates gpui uses for
/// window bounds: global on Windows and Linux, relative to the monitor on macOS. On X11
/// gpui reports the whole X screen as one monitor, so all monitors share one entry.
fn window_space(bounds: Bounds<gpui::Pixels>) -> Rect {
    let (x, y) = if cfg!(target_os = "macos") {
        (0.0, 0.0)
    } else {
        (bounds.origin.x.as_f32(), bounds.origin.y.as_f32())
    };
    Rect {
        x,
        y,
        w: bounds.size.width.as_f32(),
        h: bounds.size.height.as_f32(),
    }
}

/// Wires the opened main window to the desktop: closing, moving, zoom, the tray, the
/// starts handed over by later launches, and the events of the operating system.
pub fn attach(
    handle: WindowHandle<Inlet>,
    forwarded: mpsc::Receiver<Vec<String>>,
    start: &Start,
    cx: &mut App,
) {
    cx.global_mut::<Desktop>().window = Some(handle.into());
    let zoom = cx.global::<Desktop>().prefs.zoom;
    let _ = handle.update(cx, |_, window, cx| {
        window.set_rem_size(px(BASE_REM * zoom));
        window.on_window_should_close(cx, should_close);
        cx.observe_window_bounds(window, |_, window, cx| note_window_place(window, cx))
            .detach();
    });

    // Later starts, through a bounded queue.
    let (tx, mut rx) = futures::channel::mpsc::channel(platform::FORWARD_QUEUE);
    if let Err(err) = platform::forward_second_launches(forwarded, tx) {
        log::warn!("later starts cannot reach this one: {err}");
    }
    cx.spawn(async move |cx| {
        while let Some(ev) = rx.next().await {
            let alive = handle.update(cx, |this, window, cx| {
                this.on_platform_event(ev, window, cx)
            });
            if alive.is_err() {
                return;
            }
        }
    })
    .detach();
    // Tray clicks, notification clicks and links macOS hands over. Read only here.
    let os = inlet_platform::events();
    cx.spawn(async move |cx| {
        while let Ok(ev) = os.recv().await {
            let alive = handle.update(cx, |this, window, cx| {
                this.on_platform_event(Ev::Os(ev), window, cx)
            });
            if alive.is_err() {
                return;
            }
        }
    })
    .detach();

    create_tray(
        start.opens_hidden(),
        start.hidden && cfg!(target_os = "linux"),
        cx,
    );
    // The link Inlet was started with waits for the account like any other.
    if let Some(link) = start.link.clone() {
        let _ = handle.update(cx, |this, _, cx| this.offer_deep_link(link, cx));
    }

    // Once a second: the unread state for the tray and the badge, and the window's
    // place once it has stayed put.
    cx.spawn(async move |cx| loop {
        cx.background_executor().timer(Duration::from_secs(1)).await;
        let alive = handle.update(cx, |this, _, cx| {
            let unread = this.desktop_unread(cx);
            sync_unread(unread, cx);
            this.release_waiting_link(cx);
            let d = cx.global_mut::<Desktop>();
            if d.moved_at.is_some_and(|t| t.elapsed() >= SAVE_AFTER) {
                d.save();
            }
        });
        if alive.is_err() {
            return;
        }
    })
    .detach();

    #[cfg(feature = "demo")]
    if let Ok(tab) = std::env::var("INLET_DEMO_SETTINGS") {
        let _ = handle.update(cx, |this, _, cx| this.open_settings(Some(&tab), cx));
    }
    #[cfg(feature = "demo")]
    if let Some(link) = std::env::var("INLET_DEMO_DEEP_LINK")
        .ok()
        .and_then(|l| deep_link::parse(&l))
    {
        // Waits for the demo account, as a link Inlet was started with does.
        let _ = handle.update(cx, |this, _, cx| this.offer_deep_link(link, cx));
    }
}

/// `opened_hidden`: the window was opened hidden for a login start (Windows, macOS).
/// `hide_when_shown`: a login start on Linux, where the window is hidden only once a
/// tray icon is there.
fn create_tray(opened_hidden: bool, hide_when_shown: bool, cx: &mut App) {
    let icon = match Icon::from_png(TRAY_PNG) {
        Ok(icon) => icon,
        Err(err) => {
            log::warn!("tray icon: {err}");
            return;
        }
    };
    let menu = vec![
        TrayItem::action(TRAY_OPEN, t!("desktop-tray-open")),
        TrayItem::separator(),
        TrayItem::action(TRAY_QUIT, t!("desktop-tray-quit")),
    ];
    let created = gpui_glue::create_tray(cx, icon, menu);
    cx.spawn(async move |cx| {
        let result = created.await;
        cx.update(|cx| match result {
            Ok(tray) => {
                let shown = tray.has_host();
                let d = cx.global_mut::<Desktop>();
                d.tray = Some(tray);
                // Draw the current state on the new icon at the next tick.
                d.last_unread = None;
                if hide_when_shown && shown {
                    hide_main_window(cx);
                }
            }
            Err(err) => {
                log::info!("no tray icon: {err}");
                // Without a tray icon a hidden window could not be found again.
                if opened_hidden {
                    show_main_window(cx);
                }
            }
        });
    })
    .detach();
}

/// Runs `f` on the main window once the current update is done. Actions, keys and
/// clicks are handled inside an update of that very window, during which gpui has
/// taken the window out of the app; updating it there fails. Deferred, the window is
/// back. A failure is logged, never swallowed.
fn with_main_window(
    cx: &mut App,
    what: &'static str,
    f: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    let Some(handle) = cx.try_global::<Desktop>().and_then(|d| d.window) else {
        return;
    };
    cx.defer(move |cx| {
        if let Err(err) = handle.update(cx, |_, window, cx| f(window, cx)) {
            log::warn!("could not {what}: {err}");
        }
    });
}

/// Brings the main window to the front.
pub fn show_main_window(cx: &mut App) {
    with_main_window(cx, "show the window", |window, cx| {
        gpui_glue::show_window(window, cx)
    });
}

fn hide_main_window(cx: &mut App) {
    with_main_window(cx, "hide the window", |window, cx| {
        gpui_glue::hide_window(window, cx)
    });
}

/// Writes the window's place, ends the session (bounded wait) and quits.
pub fn quit(cx: &mut App) {
    if cx.has_global::<Desktop>() {
        cx.global_mut::<Desktop>().save();
    }
    // Let the backend end an unconfirmed browser sign-in first (bounded wait).
    if let Some(current) = cx.try_global::<CurrentBackend>() {
        let (tx, rx) = std::sync::mpsc::channel();
        current.0.send(accounts::Cmd::Shutdown(tx));
        let _ = rx.recv_timeout(Duration::from_secs(4));
    }
    cx.quit();
}

/// The close button: hide to the tray when that is on and a tray icon is shown,
/// otherwise close (and quit).
fn should_close(window: &mut Window, cx: &mut App) -> bool {
    note_window_place(window, cx);
    let d = cx.global_mut::<Desktop>();
    let to_tray = d.prefs.close_to_tray && d.tray_shown();
    d.save();
    if to_tray {
        gpui_glue::hide_window(window, cx);
        false
    } else {
        true
    }
}

/// Notes the window's place for its monitor; written once it stays put.
fn note_window_place(window: &Window, cx: &mut App) {
    let (bounds, maximized) = match window.window_bounds() {
        WindowBounds::Windowed(b) => (b, false),
        WindowBounds::Maximized(b) => (b, true),
        // Full screen keeps the place it had before.
        WindowBounds::Fullscreen(_) => return,
    };
    let Some(monitor) = window.display(cx).and_then(|d| d.uuid().ok()) else {
        return;
    };
    let rect = Rect {
        x: bounds.origin.x.as_f32(),
        y: bounds.origin.y.as_f32(),
        w: bounds.size.width.as_f32(),
        h: bounds.size.height.as_f32(),
    };
    let d = cx.global_mut::<Desktop>();
    if d.prefs
        .window
        .remember(&monitor.to_string(), rect, maximized)
    {
        d.moved_at = Some(Instant::now());
    }
}

/// One zoom step up or down.
pub fn zoom_by(direction: i32, cx: &mut App) {
    let zoom = prefs::zoom_step(cx.global::<Desktop>().prefs.zoom, direction);
    set_zoom(zoom, cx);
}

/// Sets the zoom level and keeps it.
pub fn set_zoom(zoom: f32, cx: &mut App) {
    let zoom = prefs::nearest_step(zoom);
    let d = cx.global_mut::<Desktop>();
    if d.prefs.zoom == zoom {
        return;
    }
    d.prefs.zoom = zoom;
    d.save();
    with_main_window(cx, "apply the zoom", move |window, _| {
        window.set_rem_size(px(BASE_REM * zoom));
        window.refresh();
    });
}

/// Turns closing to the tray on or off.
pub fn set_close_to_tray(on: bool, cx: &mut App) {
    let d = cx.global_mut::<Desktop>();
    d.prefs.close_to_tray = on;
    d.save();
}

/// Turns the login item on or off, or changes whether it starts Inlet hidden. The
/// system call runs in the background; the settings tab redraws when it is done.
pub fn set_autostart(on: bool, hidden: bool, cx: &mut App) {
    let d = cx.global_mut::<Desktop>();
    d.prefs.start_minimized = hidden;
    d.autostart_error = None;
    d.save();
    if demo_mode() {
        return;
    }
    let task = cx.background_spawn(async move { autostart::set(on, hidden) });
    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| {
            let d = cx.global_mut::<Desktop>();
            match result {
                Ok(()) => d.autostart = Some(on),
                Err(err) => {
                    log::warn!("could not change the login item: {err}");
                    d.autostart_error = Some(err.to_string());
                }
            }
            cx.refresh_windows();
        });
    })
    .detach();
}

/// What the tray and the badge show, over every guild and the direct messages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DesktopUnread {
    /// Unread mentions, muted places included (as the rail counts them).
    pub mentions: u32,
    /// Unread messages somewhere that is not muted.
    pub unread: bool,
}

impl DesktopUnread {
    /// Adds up the unread state of guilds and direct messages.
    pub fn of(parts: impl IntoIterator<Item = Unread>) -> Self {
        let mut out = DesktopUnread::default();
        for u in parts {
            out.mentions = out.mentions.saturating_add(u.mentions);
            out.unread |= u.unread && !u.muted;
        }
        out
    }

    pub fn tray_state(&self) -> TrayState {
        if self.mentions > 0 {
            TrayState::Mention
        } else if self.unread {
            TrayState::Unread
        } else {
            TrayState::Normal
        }
    }

    pub fn badge(&self) -> Badge {
        if self.mentions > 0 {
            Badge::Count(self.mentions)
        } else if self.unread {
            Badge::Dot
        } else {
            Badge::None
        }
    }

    fn tooltip(&self) -> String {
        if self.mentions > 0 {
            t!("desktop-tray-mentions", count = self.mentions)
        } else if self.unread {
            t!("desktop-tray-unread")
        } else {
            t!("desktop-tray-idle")
        }
    }
}

/// One account's part of what the tray and the badge show.
fn account_unread(state: &AccountState) -> DesktopUnread {
    DesktopUnread::of(
        state
            .guilds()
            .map(|g| state.guild_unread(g.id))
            .chain(std::iter::once(state.private_unread())),
    )
}

/// The active account's unread state with the marks of the other signed-in accounts.
fn with_other_accounts(
    mut out: DesktopUnread,
    others: impl IntoIterator<Item = crate::ui::overlays::account_switcher::Summary>,
) -> DesktopUnread {
    for s in others {
        out.mentions = out.mentions.saturating_add(s.mentions);
        out.unread |= s.unread;
    }
    out
}

/// Shows the unread state on the tray icon and the badge when it changed.
fn sync_unread(unread: DesktopUnread, cx: &mut App) {
    let d = cx.global_mut::<Desktop>();
    if d.last_unread == Some(unread) {
        return;
    }
    d.last_unread = Some(unread);
    let tray = d.tray.clone();
    if let Some(tray) = &tray {
        gpui_glue::set_tray_state(cx, tray, unread.tray_state());
        gpui_glue::set_tray_tooltip(cx, tray, unread.tooltip());
    }
    gpui_glue::set_badge(cx, unread.badge());
}

/// The signed-in account, as the link rules see it.
struct AccountLookup<'a> {
    inlet: &'a Inlet,
    state: Option<&'a AccountState>,
}

impl Lookup for AccountLookup<'_> {
    fn instance(&self) -> Option<&str> {
        (self.inlet.screen == Screen::Main && !self.inlet.instance.is_empty())
            .then_some(self.inlet.instance.as_str())
    }

    fn channel(&self, guild: Option<u64>, channel: u64) -> Option<(String, Option<String>)> {
        let state = self.state?;
        let id = ChannelId(channel);
        let record = state.channel(id)?;
        match guild {
            Some(g) => {
                if state.channels().guild_of(id) != Some(GuildId(g)) {
                    return None;
                }
                let guild_name = self
                    .inlet
                    .guilds
                    .iter()
                    .find(|x| x.id == g.to_string())
                    .map(|x| x.name.clone());
                let name = clean_name(record.name.as_deref().unwrap_or_default());
                Some((format!("#{name}"), guild_name))
            }
            None => {
                if state.channels().guild_of(id).is_some() {
                    return None;
                }
                Some((t!("desktop-direct-message"), None))
            }
        }
    }
}

impl Inlet {
    /// Events from the desktop side: later starts and the operating system.
    pub(crate) fn on_desktop_event(
        &mut self,
        event: Ev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.shows_window() {
            gpui_glue::show_window(window, cx);
        }
        match event {
            Ev::SecondLaunch(SecondLaunch { link, dock_action }) => {
                // "Open Inlet" in the jump list only shows the window (done above).
                if dock_action == Some(DOCK_QUIT) {
                    quit(cx);
                    return;
                }
                if let Some(link) = link {
                    self.offer_deep_link(link, cx);
                }
            }
            Ev::Os(PlatformEvent::DeepLink(link)) => self.offer_deep_link(link, cx),
            Ev::Os(PlatformEvent::TrayMenu(id)) if id == TRAY_QUIT => quit(cx),
            Ev::Os(_) => {}
        }
    }

    /// What the tray and the badge show. Every signed-in account adds its part here:
    /// with several accounts, this is the one place that sums them.
    /// Every signed-in account counts: the active one from its state, the others from
    /// their marks in the account switcher.
    fn desktop_unread(&self, cx: &App) -> DesktopUnread {
        let active = self
            .account
            .as_ref()
            .map(|a| account_unread(a.read(cx)))
            .unwrap_or_default();
        with_other_accounts(active, self.switcher.background_marks())
    }

    /// Where the session is, for links.
    fn link_session(&self) -> Session {
        session_of(&self.screen, self.account.is_some())
    }

    /// An `inlet://` link from outside: through the gate, then a prompt, or held until
    /// the account is ready (Inlet was started by the link, or is reconnecting). While
    /// a prompt is open, further links are dropped, so the prompt cannot change under
    /// the cursor.
    pub(crate) fn offer_deep_link(&mut self, link: DeepLink, cx: &mut Context<Self>) {
        if self.overlays.link_prompt.is_some() {
            log::info!("a link prompt is open; another link was dropped");
            return;
        }
        let now = Instant::now();
        let session = self.link_session();
        let d = cx.global_mut::<Desktop>();
        if !d.gate.admit(&link, now) {
            return;
        }
        if let Some(next) = d.links.offer(Incoming::Deep(link), session, now) {
            self.show_next_link(next, cx);
        }
    }

    /// Shows a held link once the account is ready (called once a second).
    fn release_waiting_link(&mut self, cx: &mut Context<Self>) {
        if self.overlays.link_prompt.is_some() {
            return;
        }
        let session = self.link_session();
        if let Some(next) = cx
            .global_mut::<Desktop>()
            .links
            .poll(session, Instant::now())
        {
            self.show_next_link(next, cx);
        }
    }

    fn show_next_link(&mut self, next: Next, cx: &mut Context<Self>) {
        match next {
            Next::Ask(link) => self.ask_about_link(link, cx),
            Next::SignInFirst => {
                self.overlays.link_prompt = Some(Prompt::SignInFirst);
                cx.notify();
            }
            Next::Unavailable => {
                self.overlays.link_prompt = Some(Prompt::Unavailable);
                cx.notify();
            }
        }
    }

    /// The layers over the chat that are open now.
    pub(crate) fn open_layers(&self) -> OpenLayers {
        OpenLayers {
            link_prompt: self.overlays.link_prompt.is_some(),
            link_confirm: self.overlays.pending_link.is_some(),
            notification_settings: self.notifications.settings.is_some(),
            switcher: self.switcher_layer_open(),
            settings: self.settings.view.is_some(),
            viewer: self.media.viewer.is_some(),
        }
    }

    /// Whether a layer covers the chat: the link prompt, the link confirmation, the
    /// notification settings window, the account switcher, the settings view or the
    /// image viewer.
    pub(crate) fn layer_open(&self) -> bool {
        self.open_layers().topmost().is_some()
    }

    /// Whether `layer` is the topmost open one, so it has the keyboard and Escape.
    pub(crate) fn is_top_layer(&self, layer: Layer) -> bool {
        self.open_layers().topmost() == Some(layer)
    }

    /// Keeps the keyboard on an open layer, so typing never reaches the composer hidden
    /// behind it, and gives it back to the composer when the last layer closes. Called
    /// when the window is drawn.
    pub(crate) fn focus_layer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.layer_open() {
            let handle = self
                .overlays
                .layer_focus
                .get_or_insert_with(|| cx.focus_handle())
                .clone();
            if !handle.is_focused(window) {
                if !self.overlays.layer_had_focus {
                    // Where the keyboard was, to give it back when the layers close.
                    self.overlays.focus_before = window.focused(cx);
                    self.overlays.layer_had_focus = true;
                }
                cx.defer_in(window, move |_, window, cx| handle.focus(window, cx));
            }
        } else if self.overlays.layer_had_focus {
            self.overlays.layer_had_focus = false;
            // Back where it was; if nothing had the keyboard, the field of the screen
            // that is up: the sign-in field, or the composer in the chat.
            let target = self
                .overlays
                .focus_before
                .take()
                .or_else(|| match self.screen {
                    Screen::SignIn => Some(self.sign_in.instance_input.focus_handle(cx)),
                    Screen::Main => Some(self.composer.view.focus_handle(cx)),
                    _ => None,
                });
            if let Some(target) = target {
                cx.defer_in(window, move |_, window, cx| target.focus(window, cx));
            }
        }
    }

    /// Adds the layer's focus, key context and Escape to the topmost layer's element.
    pub(crate) fn layer_keys<E: InteractiveElement>(
        &self,
        element: E,
        cx: &mut Context<Self>,
    ) -> E {
        let element = element
            .key_context(LAYER_CONTEXT)
            .on_action(cx.listener(|this, _: &CloseLayer, _, cx| this.close_top_layer(cx)));
        match &self.overlays.layer_focus {
            Some(handle) => element.track_focus(handle),
            None => element,
        }
    }

    /// Escape on a layer: it closes the topmost one (see [`OpenLayers::topmost`]).
    pub(crate) fn close_top_layer(&mut self, cx: &mut Context<Self>) {
        match self.open_layers().topmost() {
            Some(Layer::LinkPrompt) => self.overlays.link_prompt = None,
            Some(Layer::LinkConfirm) => self.overlays.pending_link = None,
            Some(Layer::NotificationSettings) => self.notifications.settings = None,
            Some(Layer::AccountSwitcher) => self.switcher.open = false,
            Some(Layer::Settings) => self.settings.view = None,
            Some(Layer::Viewer) => self.media.viewer = None,
            None => {}
        }
        cx.notify();
    }

    /// Shows the prompt for a link. Nothing happens until the person clicks in it.
    pub(crate) fn ask_about_link(&mut self, link: Incoming, cx: &mut Context<Self>) {
        let prompt = {
            let lookup = AccountLookup {
                inlet: self,
                state: self.account.as_ref().map(|a| a.read(cx)),
            };
            links::decide(&link, &lookup)
        };
        self.overlays.link_prompt = Some(prompt);
        cx.notify();
    }

    /// "Open" in a channel prompt: the guild first, then the channel once its list is
    /// in (see `take_link_channel`).
    pub(crate) fn follow_channel_link(
        &mut self,
        guild: Option<String>,
        channel: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_guild == guild {
            self.chat.pending_channel_jump = Some(channel);
        } else {
            self.overlays.link_channel = Some(channel);
            self.select_guild(guild, cx);
        }
        let _ = window;
        cx.notify();
    }

    /// Called when a guild's channel list arrives: opens the channel a link asked for.
    pub(crate) fn take_link_channel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(channel) = self.overlays.link_channel.take() {
            self.chat.pending_channel_jump = Some(channel);
            self.take_channel_jump(window, cx);
        }
    }
}

/// A layer drawn over the chat that takes the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layer {
    LinkPrompt,
    LinkConfirm,
    NotificationSettings,
    AccountSwitcher,
    Settings,
    Viewer,
}

/// Which layers are open. They are drawn bottom first (the viewer, the settings view,
/// the account switcher, the notification settings window, the link confirmation, the
/// link prompt, see `ui/overlays`), and the topmost one has the keyboard: Escape
/// closes it first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OpenLayers {
    pub link_prompt: bool,
    pub link_confirm: bool,
    pub notification_settings: bool,
    pub switcher: bool,
    pub settings: bool,
    pub viewer: bool,
}

impl OpenLayers {
    pub fn topmost(self) -> Option<Layer> {
        if self.link_prompt {
            Some(Layer::LinkPrompt)
        } else if self.link_confirm {
            Some(Layer::LinkConfirm)
        } else if self.notification_settings {
            Some(Layer::NotificationSettings)
        } else if self.switcher {
            Some(Layer::AccountSwitcher)
        } else if self.settings {
            Some(Layer::Settings)
        } else if self.viewer {
            Some(Layer::Viewer)
        } else {
            None
        }
    }

    /// The layers left after Escape.
    #[cfg(test)]
    fn after_escape(mut self) -> OpenLayers {
        match self.topmost() {
            Some(Layer::LinkPrompt) => self.link_prompt = false,
            Some(Layer::LinkConfirm) => self.link_confirm = false,
            Some(Layer::NotificationSettings) => self.notification_settings = false,
            Some(Layer::AccountSwitcher) => self.switcher = false,
            Some(Layer::Settings) => self.settings = false,
            Some(Layer::Viewer) => self.viewer = false,
            None => {}
        }
        self
    }
}

/// The session as links see it, from the screen and whether the account state is in.
fn session_of(screen: &Screen, has_account: bool) -> Session {
    match screen {
        Screen::Main if has_account => Session::Ready,
        Screen::Main | Screen::Starting { .. } | Screen::Finishing | Screen::Offline { .. } => {
            Session::Connecting
        }
        Screen::SignIn | Screen::Confirm { .. } => Session::SignedOut,
        // The backend stopped: nothing will become ready; a waiting link is dropped
        // with a notice.
        Screen::Fatal => Session::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn a_link_that_starts_inlet_is_kept_for_the_account() {
        let start = Start::from_args(&strings(&["--url", "--", "inlet://channels/1/2"]));
        assert_eq!(
            start.link,
            Some(DeepLink::Channel {
                guild: Some(1),
                channel: 2,
                message: None
            })
        );
        assert!(!start.hidden && !start.quit_at_once);
        // Flags after `--` do not count; a link that breaks out of its quotes is none.
        let start = Start::from_args(&strings(&[
            "--url",
            "--",
            "inlet://x\"",
            "--autostart",
            "--start-minimized",
        ]));
        assert_eq!(
            start,
            Start {
                hidden: false,
                link: None,
                quit_at_once: false
            }
        );
        assert!(Start::from_args(&strings(&["--autostart", "--start-minimized"])).hidden);
        assert!(Start::from_args(&strings(&["--dock-action", "1"])).quit_at_once);
        assert!(!Start::from_args(&strings(&["--dock-action", "0"])).quit_at_once);

        // The link waits while the saved session connects, then is asked about; a
        // signed-in person never sees "sign in first".
        let mut queue = LinkQueue::default();
        let now = Instant::now();
        let link = Incoming::Deep(start_link());
        let starting = Screen::Starting { instance: None };
        assert_eq!(
            queue.offer(link.clone(), session_of(&starting, false), now),
            None
        );
        let offline = Screen::Offline {
            instance: "https://chat.example".into(),
            message: String::new(),
            retry_in_secs: 5,
        };
        assert_eq!(queue.poll(session_of(&offline, false), now), None);
        assert_eq!(queue.poll(session_of(&Screen::Main, false), now), None);
        assert_eq!(
            queue.poll(session_of(&Screen::Main, true), now),
            Some(Next::Ask(link))
        );
        assert_eq!(session_of(&Screen::SignIn, false), Session::SignedOut);
        // The backend stopped: the link is dropped with a notice, not kept waiting.
        let mut queue = LinkQueue::default();
        queue.offer(
            Incoming::Deep(start_link()),
            session_of(&starting, false),
            now,
        );
        assert_eq!(
            queue.poll(session_of(&Screen::Fatal, false), now),
            Some(Next::Unavailable)
        );
        assert_eq!(queue.poll(session_of(&Screen::Main, true), now), None);
    }

    fn start_link() -> DeepLink {
        deep_link::parse("inlet://channels/1/2").unwrap()
    }

    /// A view for the zoom test: just a window to zoom.
    struct Probe;

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// Zoom applies at once, from inside the window's own dispatch (as a key press or a
    /// click in the settings tab is handled), not only after a restart.
    #[gpui::test]
    fn zoom_changes_the_rem_size_at_once(cx: &mut gpui::TestAppContext) {
        let window = cx.add_window(|_, _| Probe);
        cx.update(|cx| {
            cx.set_global(Desktop::new(DesktopPrefs::default(), None));
            cx.global_mut::<Desktop>().window = Some(window.into());
            install_actions(cx);
        });
        let rem = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |_, window, _| window.rem_size().as_f32())
                .unwrap()
        };
        assert_eq!(rem(cx), BASE_REM);

        // The action, dispatched in the window like a key press.
        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(ZoomIn), cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert!((rem(cx) - BASE_REM * 1.1).abs() < 0.01, "{}", rem(cx));

        // A button in the settings tab: a handler inside an update of that window.
        window.update(cx, |_, _, cx| zoom_by(1, cx)).unwrap();
        cx.run_until_parked();
        assert!((rem(cx) - BASE_REM * 1.25).abs() < 0.01, "{}", rem(cx));
        cx.update(|cx| assert_eq!(cx.global::<Desktop>().prefs.zoom, 1.25));

        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(ZoomReset), cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(rem(cx), BASE_REM);
    }

    /// Escape with a masked-link confirmation over the image viewer closes the
    /// confirmation first and leaves the viewer open; the next Escape closes the viewer.
    #[test]
    fn escape_closes_the_link_confirmation_before_the_viewer_under_it() {
        let mut layers = OpenLayers {
            viewer: true,
            link_confirm: true,
            ..OpenLayers::default()
        };
        assert_eq!(layers.topmost(), Some(Layer::LinkConfirm));
        layers = layers.after_escape();
        assert_eq!(layers.topmost(), Some(Layer::Viewer));
        assert!(layers.viewer && !layers.link_confirm);
        layers = layers.after_escape();
        assert_eq!(layers.topmost(), None);
    }

    /// The notification settings window is a layer: it has the keyboard (so typing
    /// never reaches the composer behind it) and Escape closes it, before the settings
    /// view under it and after a link confirmation over it.
    #[test]
    fn the_notification_settings_window_is_a_layer() {
        let alone = OpenLayers {
            notification_settings: true,
            ..OpenLayers::default()
        };
        assert_eq!(alone.topmost(), Some(Layer::NotificationSettings));
        assert_eq!(alone.after_escape().topmost(), None);
        let mut layers = OpenLayers {
            link_confirm: true,
            notification_settings: true,
            settings: true,
            ..OpenLayers::default()
        };
        assert_eq!(layers.topmost(), Some(Layer::LinkConfirm));
        layers = layers.after_escape();
        assert_eq!(layers.topmost(), Some(Layer::NotificationSettings));
        layers = layers.after_escape();
        assert_eq!(layers.topmost(), Some(Layer::Settings));
    }

    /// Every layer closes before the one under it, in the order they are drawn.
    #[test]
    fn layers_close_from_the_top_down() {
        let mut layers = OpenLayers {
            link_prompt: true,
            link_confirm: true,
            notification_settings: true,
            switcher: true,
            settings: true,
            viewer: true,
        };
        let mut order = Vec::new();
        while let Some(top) = layers.topmost() {
            order.push(top);
            layers = layers.after_escape();
        }
        assert_eq!(
            order,
            [
                Layer::LinkPrompt,
                Layer::LinkConfirm,
                Layer::NotificationSettings,
                Layer::AccountSwitcher,
                Layer::Settings,
                Layer::Viewer
            ]
        );
    }

    fn unread(unread: bool, mentions: u32, muted: bool) -> Unread {
        Unread {
            unread,
            mentions,
            muted,
        }
    }

    #[test]
    fn the_zoom_keys_parse() {
        for keys in ZOOM_KEYS {
            gpui::Keystroke::parse(keys).unwrap();
        }
    }

    #[test]
    fn the_tray_shows_mentions_first_and_skips_muted_unread() {
        let none = DesktopUnread::of([unread(false, 0, false)]);
        assert_eq!(none.tray_state(), TrayState::Normal);
        assert_eq!(none.badge(), Badge::None);
        let muted = DesktopUnread::of([unread(true, 0, true), unread(false, 0, false)]);
        assert_eq!(muted.tray_state(), TrayState::Normal);
        let plain = DesktopUnread::of([unread(true, 0, true), unread(true, 0, false)]);
        assert_eq!(plain.tray_state(), TrayState::Unread);
        assert_eq!(plain.badge(), Badge::Dot);
        // Mentions count in a muted guild too, as on the rail.
        let mentions = DesktopUnread::of([unread(true, 2, true), unread(true, 3, false)]);
        assert_eq!(mentions.tray_state(), TrayState::Mention);
        assert_eq!(mentions.badge(), Badge::Count(5));
        assert_eq!(
            DesktopUnread::of([unread(false, u32::MAX, false), unread(false, 1, false)]).mentions,
            u32::MAX
        );
    }

    #[test]
    fn the_tray_counts_every_signed_in_account() {
        use crate::ui::overlays::account_switcher::Summary;
        // The active account has nothing; another has two mentions, a third only unread.
        let active = DesktopUnread::of([unread(false, 0, false)]);
        let total = with_other_accounts(
            active,
            [
                Summary {
                    mentions: 2,
                    unread: true,
                },
                Summary {
                    mentions: 0,
                    unread: true,
                },
            ],
        );
        assert_eq!(total.badge(), Badge::Count(2));
        assert_eq!(total.tray_state(), TrayState::Mention);
        let quiet = with_other_accounts(
            active,
            [Summary {
                mentions: 0,
                unread: true,
            }],
        );
        assert_eq!(quiet.tray_state(), TrayState::Unread);
        assert_eq!(with_other_accounts(active, []), active);
    }
}
