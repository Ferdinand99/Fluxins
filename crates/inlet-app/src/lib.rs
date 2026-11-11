// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inlet: a native desktop client that works with Fluxer.
//!
//! [`run`] starts things: logging, the one-instance check, the gpui app and the main
//! window. The view is `app::Inlet`; the views of each domain are under `ui/`, the
//! network side under `backend/`, the desktop integration in `desktop`, and the demo
//! data under `demo/`.
//!
//! This is a library only so the tests in `tests/` can open the real views; `main.rs`
//! calls [`run`] and nothing else uses it. It is not an API.

#[macro_use]
mod i18n;

mod about;
mod app;
mod assets;
mod backend;
mod brand_icon;
#[cfg(feature = "demo")]
mod demo;
mod desktop;
#[cfg(feature = "demo")]
#[doc(hidden)]
pub mod layout_probe;
mod model;
mod reconnect;
mod text_input;
mod theme;
mod token_store;
mod ui;

use gpui::{App, AppContext};

use app::Inlet;

/// The scene files of the visual checks, for `inlet-visual`. Demo builds only.
#[cfg(feature = "demo")]
#[doc(hidden)]
pub use demo::scene_file;

/// Starts Inlet and runs it until it quits.
pub fn run() {
    // Network libraries log frame contents at trace level; keep them quiet whatever
    // RUST_LOG says, so a token can never end up in a log.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .filter_module("tungstenite", log::LevelFilter::Off)
        .filter_module("tokio_tungstenite", log::LevelFilter::Off)
        .filter_module("hyper", log::LevelFilter::Off)
        .filter_module("hyper_util", log::LevelFilter::Off)
        .filter_module("reqwest", log::LevelFilter::Warn)
        .filter_module("rustls", log::LevelFilter::Warn)
        .init();
    // A demo scene file (tests/visual/scenes) sets the demo variables before anything
    // reads them.
    #[cfg(feature = "demo")]
    demo::scenes::load_scene_file();
    // One Inlet per user: a later start hands over its command line and exits.
    let forwarded = match desktop::claim_instance() {
        Ok(Some(forwarded)) => forwarded,
        Ok(None) => return,
        Err(err) => desktop::run_start_error(err),
    };
    let start = desktop::Start::from_env();
    if start.quit_at_once {
        // The jump list's "Quit Inlet" while Inlet was not running.
        return;
    }
    let app = gpui_platform::application();
    inlet_platform::gpui_glue::install_on_application(&app);
    app.on_reopen(desktop::show_main_window);
    app.run(move |cx: &mut App| {
        text_input::bind_keys(cx);
        ui::notifications::bind_keys(cx);
        ui::composer::bind_keys(cx);
        assets::load_fonts(cx);
        desktop::init(cx);
        let options = desktop::window_options(&start, cx);
        let window = cx
            .open_window(options, |window, cx| cx.new(|cx| Inlet::new(window, cx)))
            .expect("open the main window");
        desktop::attach(window, forwarded, &start, cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                desktop::quit(cx);
            }
        })
        .detach();
        if !start.hidden {
            cx.activate(true);
        }
    });
}
