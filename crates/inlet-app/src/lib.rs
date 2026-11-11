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
        bind_keys(cx);
        assets::load_fonts(cx);
        desktop::init(cx);
        ui::notifications::alerts::init(cx);
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

/// The key bindings of every domain that has some, in this order.
fn bind_keys(cx: &mut App) {
    text_input::bind_keys(cx);
    ui::notifications::bind_keys(cx);
    ui::composer::bind_keys(cx);
    ui::composer::pickers::bind_keys(cx);
    ui::chat::bind_keys(cx);
    ui::members::bind_keys(cx);
    ui::search::bind_keys(cx);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use gpui::{App, TestAppContext};

    /// A domain's `bind_keys`.
    type Bind = fn(&mut App);

    /// The domains with key bindings. A new `fn bind_keys` must be added here and to
    /// [`super::bind_keys`].
    const DOMAINS: [(&str, Bind); 7] = [
        ("text_input", crate::text_input::bind_keys),
        ("ui::notifications", crate::ui::notifications::bind_keys),
        ("ui::composer", crate::ui::composer::bind_keys),
        (
            "ui::composer::pickers",
            crate::ui::composer::pickers::bind_keys,
        ),
        ("ui::chat", crate::ui::chat::bind_keys),
        ("ui::members", crate::ui::members::bind_keys),
        ("ui::search", crate::ui::search::bind_keys),
    ];

    fn bound(cx: &mut TestAppContext, bind: Bind) -> BTreeSet<String> {
        cx.update(|cx| {
            cx.clear_key_bindings();
            bind(cx);
            let keymap = cx.key_bindings();
            let names = keymap
                .borrow()
                .bindings()
                .map(|b| b.action().name().to_string())
                .collect();
            names
        })
    }

    /// Key bindings made outside a domain's `bind_keys`: (file, function, the call in
    /// `run` that reaches it).
    const OTHER_BINDERS: [(&str, &str, &str); 1] =
        [("desktop.rs", "install_actions", "desktop::init(cx)")];

    /// The non-test part of every source file under `dir` except `lib.rs`, with its path
    /// below `src` (`ui/chat/mod.rs`).
    fn sources(src: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).expect("read src/") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                sources(src, &path, out);
            } else if path.extension().is_some_and(|e| e == "rs") && path != src.join("lib.rs") {
                let text = std::fs::read_to_string(&path).expect("read a source file");
                let code = match text.find("#[cfg(test)]\nmod tests") {
                    Some(at) => text[..at].to_string(),
                    None => text,
                };
                let name = path
                    .strip_prefix(src)
                    .expect("below src/")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((name, code));
            }
        }
    }

    /// The name of the function the byte offset `at` of `code` is in.
    fn enclosing_fn(code: &str, at: usize) -> &str {
        let before = &code[..at];
        let start = before.rfind("fn ").map_or(0, |i| i + 3);
        let name = &before[start..];
        let end = name
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(name.len());
        &name[..end]
    }

    /// Every function that binds keys is a domain's `bind_keys` (whatever its
    /// visibility) or one of [`OTHER_BINDERS`], and every one of those is reached from
    /// start-up. A new place that binds keys fails here until it is listed.
    #[test]
    fn every_place_that_binds_keys_is_reached_from_start_up() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&src, &src, &mut files);
        let definitions: usize = files
            .iter()
            .map(|(_, code)| code.matches("fn bind_keys(").count())
            .sum();
        assert_eq!(
            definitions,
            DOMAINS.len(),
            "a `fn bind_keys` is missing from DOMAINS and from lib.rs's bind_keys"
        );
        for (file, code) in &files {
            for (at, _) in code.match_indices(".bind_keys(") {
                let function = enclosing_fn(code, at);
                let listed = OTHER_BINDERS
                    .iter()
                    .any(|(f, name, _)| f == file && *name == function);
                assert!(
                    function == "bind_keys" || listed,
                    "{file}: `{function}` binds keys; make it a domain's bind_keys or list it \
                     in OTHER_BINDERS"
                );
            }
        }
        let lib = std::fs::read_to_string(src.join("lib.rs")).expect("read lib.rs");
        let run = &lib[lib.find("pub fn run()").expect("run in lib.rs")..];
        for (file, function, call) in OTHER_BINDERS {
            let (_, code) = files
                .iter()
                .find(|(f, _)| f == file)
                .unwrap_or_else(|| panic!("{file} is gone"));
            assert!(
                code.matches(&format!("{function}(cx)")).count() >= 1,
                "{file}: nothing calls {function}"
            );
            assert!(run.contains(call), "start-up does not call {call}");
        }
    }

    /// Start-up registers the bindings of every domain. A merge that drops a call from
    /// `bind_keys` (as happened when `main.rs` became `lib.rs`) fails here.
    #[gpui::test]
    fn start_up_binds_the_keys_of_every_domain(cx: &mut TestAppContext) {
        let all = bound(cx, super::bind_keys);
        for (domain, bind) in DOMAINS {
            let own = bound(cx, bind);
            assert!(!own.is_empty(), "{domain} binds no keys");
            let missing: Vec<_> = own.difference(&all).collect();
            assert!(
                missing.is_empty(),
                "start-up does not bind {domain}'s keys: {missing:?}"
            );
        }
    }
}
