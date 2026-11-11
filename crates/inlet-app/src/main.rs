// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inlet: a native desktop client that works with Fluxer.
//!
//! This file only starts things: logging, the gpui app and the main window. The view is
//! [`app::Inlet`]; the views of each domain are under `ui/`, the network side under
//! `backend/`, and the demo data under `demo/`.
// Release builds on Windows are GUI apps: no console window next to Inlet.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[macro_use]
mod i18n;

mod about;
mod app;
mod assets;
mod backend;
mod brand_icon;
#[cfg(feature = "demo")]
mod demo;
mod model;
mod reconnect;
mod text_input;
mod theme;
mod token_store;
mod ui;

use gpui::{px, size, App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions};

use app::{CurrentBackend, Inlet};
use backend::accounts;

fn main() {
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
    gpui_platform::application().run(|cx: &mut App| {
        text_input::bind_keys(cx);
        ui::notifications::bind_keys(cx);
        ui::composer::bind_keys(cx);
        assets::load_fonts(cx);
        // Fit small screens (CI runners have 1024x768): at most 90 % of the display.
        let (mut w, mut h) = (1200.0_f32, 780.0_f32);
        if let Some(display) = cx.primary_display() {
            let d = display.bounds().size;
            w = w.min(d.width.as_f32() * 0.9);
            h = h.min(d.height.as_f32() * 0.9);
        }
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(t!("app-window-title").into()),
                    ..Default::default()
                }),
                app_id: Some("no.opland.inlet".into()),
                // X11 only; Windows takes the icon from the .exe resource (build.rs).
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                icon: brand_icon::window_icon(),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Inlet::new(window, cx)),
        )
        .expect("open the main window");
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                // Let the backend end an unconfirmed browser sign-in first (bounded wait).
                if let Some(current) = cx.try_global::<CurrentBackend>() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    current.0.send(accounts::Cmd::Shutdown(tx));
                    let _ = rx.recv_timeout(std::time::Duration::from_secs(4));
                }
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
