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
    actions, div, point, prelude::*, px, size, App, Bounds, Context, FontWeight, Global,
    KeyBinding, MenuItem, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions,
};
use inlet_platform::{
    autostart, deep_link, gpui_glue, Badge, DeepLink, Icon, Instance, PlatformEvent, Tray,
    TrayItem, TrayState,
};
use inlet_state::{AccountState, ChannelId, GuildId, Unread};

use crate::app::{CurrentBackend, Inlet, Screen};
use crate::backend::accounts;
use crate::backend::platform::links::{self, Incoming, LinkGate, Lookup};
use crate::backend::platform::prefs::{self, DesktopPrefs};
use crate::backend::platform::window_state::{self, Monitor, Placement, Rect};
use crate::backend::platform::{self as platform, Ev, SecondLaunch, TRAY_OPEN, TRAY_QUIT};
use crate::model::clean_name;
use crate::ui::widgets::button;
use crate::{assets, theme};

actions!(desktop, [ShowInlet, QuitInlet, ZoomIn, ZoomOut, ZoomReset]);

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
pub struct Start {
    /// The login item asked for a start hidden in the tray.
    pub hidden: bool,
}

impl Start {
    /// Reads Inlet's own command line. Only options before any `--` count.
    pub fn from_env() -> Self {
        let args: Vec<String> = std::env::args_os()
            .skip(1)
            .filter_map(|a| a.into_string().ok())
            .collect();
        Start {
            hidden: autostart::start_minimized(&args),
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
        cx.on_window_closed(|cx, _| cx.quit()).detach();
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
                        .on_click(cx.listener(|_, _, _, cx| cx.quit())),
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
    window: Option<WindowHandle<Inlet>>,
    gate: LinkGate,
    last_unread: Option<DesktopUnread>,
    /// When the window last moved, while that is not written yet.
    moved_at: Option<Instant>,
}

impl Global for Desktop {}

impl Desktop {
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
    cx.set_global(Desktop {
        prefs,
        path,
        tray: None,
        autostart: None,
        autostart_error: None,
        window: None,
        gate: LinkGate::default(),
        last_unread: None,
        moved_at: None,
    });
    gpui_glue::install(cx);

    let [zoom_in, zoom_in_plus, zoom_out, zoom_reset] = ZOOM_KEYS;
    cx.bind_keys([
        KeyBinding::new(zoom_in, ZoomIn, None),
        KeyBinding::new(zoom_in_plus, ZoomIn, None),
        KeyBinding::new(zoom_out, ZoomOut, None),
        KeyBinding::new(zoom_reset, ZoomReset, None),
    ]);
    cx.on_action(|_: &ShowInlet, cx| show_main_window(cx));
    cx.on_action(|_: &QuitInlet, cx| quit(cx));
    cx.on_action(|_: &ZoomIn, cx| zoom_by(1, cx));
    cx.on_action(|_: &ZoomOut, cx| zoom_by(-1, cx));
    cx.on_action(|_: &ZoomReset, cx| set_zoom(1.0, cx));
    // The macOS dock menu, and the Windows jump list (whose entries start Inlet with
    // `--dock-action <n>`, see `SecondLaunch`). Same order as `platform::DOCK_ENTRIES`.
    cx.set_dock_menu(vec![
        MenuItem::action(t!("desktop-tray-open"), ShowInlet),
        MenuItem::action(t!("desktop-tray-quit"), QuitInlet),
    ]);

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

/// The main window's options: back where it was on its monitor, or centred.
pub fn window_options(start: &Start, cx: &mut App) -> WindowOptions {
    let memory = cx.global::<Desktop>().prefs.window.clone();
    let displays = cx.displays();
    let monitors: Vec<Monitor> = displays
        .iter()
        .filter_map(|d| {
            Some(Monitor {
                id: d.uuid().ok()?.to_string(),
                area: window_space(d.bounds()),
            })
        })
        .collect();
    let main = cx.primary_display().map(|d| {
        let s = d.bounds().size;
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

/// A monitor's area in the coordinates gpui uses for window bounds: global on Windows
/// and Linux, relative to the monitor on macOS.
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
    cx.global_mut::<Desktop>().window = Some(handle);
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

    create_tray(start.hidden, cx);

    // Once a second: the unread state for the tray and the badge, and the window's
    // place once it has stayed put.
    cx.spawn(async move |cx| loop {
        cx.background_executor().timer(Duration::from_secs(1)).await;
        let alive = handle.update(cx, |this, _, cx| {
            let unread = this
                .account
                .as_ref()
                .map(|a| account_unread(a.read(cx)))
                .unwrap_or_default();
            sync_unread(unread, cx);
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
        // Once the demo account is in, as a real link would find it.
        cx.spawn(async move |cx| {
            for _ in 0..80 {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let shown = handle.update(cx, |this, _, cx| {
                    let ready = this.screen == Screen::Main && this.account.is_some();
                    if ready {
                        this.ask_about_link(Incoming::Deep(link.clone()), cx);
                    }
                    ready
                });
                if shown.unwrap_or(true) {
                    return;
                }
            }
        })
        .detach();
    }
}

fn create_tray(start_hidden: bool, cx: &mut App) {
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
                // Linux opens the window and hides it once the icon is there.
                if start_hidden && shown && cfg!(target_os = "linux") {
                    hide_main_window(cx);
                }
            }
            Err(err) => log::info!("no tray icon: {err}"),
        });
    })
    .detach();
}

/// Brings the main window to the front.
pub fn show_main_window(cx: &mut App) {
    let Some(handle) = cx.try_global::<Desktop>().and_then(|d| d.window) else {
        return;
    };
    let _ = handle.update(cx, |_, window, cx| gpui_glue::show_window(window, cx));
}

fn hide_main_window(cx: &mut App) {
    let Some(handle) = cx.try_global::<Desktop>().and_then(|d| d.window) else {
        return;
    };
    let _ = handle.update(cx, |_, window, cx| gpui_glue::hide_window(window, cx));
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
    let Some(handle) = d.window else { return };
    let _ = handle.update(cx, |_, window, cx| {
        window.set_rem_size(px(BASE_REM * zoom));
        window.refresh();
        cx.notify();
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

fn account_unread(state: &AccountState) -> DesktopUnread {
    DesktopUnread::of(
        state
            .guilds()
            .map(|g| state.guild_unread(g.id))
            .chain(std::iter::once(state.private_unread())),
    )
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
                if let Some(n) = dock_action {
                    cx.perform_dock_menu_action(n);
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

    /// An `inlet://` link from outside: through the gate, then a prompt. While a prompt
    /// is open, further links are dropped, so the prompt cannot change under the cursor.
    fn offer_deep_link(&mut self, link: DeepLink, cx: &mut Context<Self>) {
        if self.overlays.link_prompt.is_some() {
            log::info!("a link prompt is open; another link was dropped");
            return;
        }
        if cx.global_mut::<Desktop>().gate.admit(&link, Instant::now()) {
            self.ask_about_link(Incoming::Deep(link), cx);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
