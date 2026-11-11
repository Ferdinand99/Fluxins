// SPDX-License-Identifier: AGPL-3.0-or-later
//! Message search and the quick switcher.
//!
//! - `bar`: the search field in the channel header and the search panel it opens.
//! - `filters`: the typed filters (`from:`, `in:`, `has:`, `before:`, `after:`,
//!   `mentions:`) and the request built from them, field by field.
//! - `results`: the results, the jump from a result, and the pacing of the requests.
//! - `quick_switcher`: Ctrl+K (Cmd+K on macOS) over every signed-in account.
//!
//! Both the search panel and the quick switcher are layers (`desktop::OpenLayers`): they
//! take the keyboard, Escape closes them first, typing never reaches the composer, and
//! nothing is marked as read behind them (`Inlet::chat_is_visible`).

pub mod bar;
pub mod filters;
pub mod quick_switcher;
pub mod results;

use gpui::{actions, App, Context, InteractiveElement, KeyBinding, Window};

use crate::app::{Inlet, Screen};
use crate::backend;
use crate::desktop::Layer;
use crate::text_input::TextInput;

pub use bar::header_button;

actions!(search, [ToggleQuickSwitcher, SwitcherUp, SwitcherDown]);

/// The key context of the quick switcher's window.
pub const SWITCHER_CONTEXT: &str = "inlet_quick_switcher";

/// The quick switcher's key: Cmd+K on macOS, Ctrl+K elsewhere (Fluxer's).
#[cfg(target_os = "macos")]
pub const SWITCHER_KEY: &str = "cmd-k";
#[cfg(not(target_os = "macos"))]
pub const SWITCHER_KEY: &str = "ctrl-k";

/// The keys of this domain: the quick switcher anywhere, and moving in its list.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(SWITCHER_KEY, ToggleQuickSwitcher, None),
        KeyBinding::new("up", SwitcherUp, Some(SWITCHER_CONTEXT)),
        KeyBinding::new("down", SwitcherDown, Some(SWITCHER_CONTEXT)),
    ]);
}

/// This domain's view state.
#[derive(Default)]
pub struct State {
    pub bar: bar::Bar,
    pub results: results::Results,
    pub switcher: quick_switcher::QuickSwitcher,
    /// Demo scenes: the scene's search or switcher was opened.
    #[cfg(feature = "demo")]
    demo_done: bool,
}

impl Inlet {
    /// Whether the search panel is open (a layer only over the main screen).
    pub(crate) fn search_panel_open(&self) -> bool {
        self.search.bar.open && self.screen == Screen::Main && self.account.is_some()
    }

    /// Whether the quick switcher is open.
    pub(crate) fn quick_switcher_open(&self) -> bool {
        self.search.switcher.open && self.screen == Screen::Main
    }

    /// Whether a search layer covers the chat (nothing is read behind it).
    pub(crate) fn search_covers_chat(&self) -> bool {
        self.search_panel_open() || self.quick_switcher_open()
    }

    /// Whether the field of the topmost search layer has the keyboard. It is focused
    /// when the layer opens, before the layer is first drawn, so the layer's own focus
    /// rule cannot see it inside the layer during that first frame.
    pub(crate) fn search_field_has_keyboard(&self, window: &Window, cx: &App) -> bool {
        let focused = |input: Option<&gpui::Entity<TextInput>>| {
            input.is_some_and(|i| gpui::Focusable::focus_handle(i.read(cx), cx).is_focused(window))
        };
        (self.is_top_layer(Layer::QuickSwitcher) && focused(self.search.switcher.input()))
            || (self.is_top_layer(Layer::Search) && focused(self.search.bar.input()))
    }

    /// Escape on a search layer.
    pub(crate) fn close_search_layer(&mut self, layer: Layer) {
        match layer {
            Layer::QuickSwitcher => self.search.switcher.open = false,
            Layer::Search => self.search.bar.open = false,
            _ => {}
        }
    }

    /// Another account is shown, or none: the search of the old one goes. The visits
    /// of every account stay for the quick switcher until all are signed out.
    pub(crate) fn reset_search(&mut self, signed_out: bool) {
        self.search.bar.clear();
        self.search.results.clear();
        self.search.switcher.open = false;
        if signed_out {
            self.search.switcher.visits.clear();
        }
    }

    /// A channel was opened: the quick switcher remembers it.
    pub(crate) fn note_channel_visit(&mut self, channel_id: &str) {
        if let (Some(key), Some(ch)) = (
            self.switcher.active.clone(),
            inlet_state::ChannelId::parse(channel_id),
        ) {
            let now = chrono::Utc::now().timestamp_millis();
            self.search.switcher.visits.note(key, ch, now);
        }
    }

    /// The window's handler of this domain (Ctrl+K), and the demo scenes.
    pub(crate) fn search_keys<E: InteractiveElement>(
        &mut self,
        el: E,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> E {
        #[cfg(feature = "demo")]
        self.demo_search(window, cx);
        let _ = window;
        el.on_action(cx.listener(|this, _: &ToggleQuickSwitcher, window, cx| {
            this.toggle_quick_switcher(window, cx)
        }))
    }

    /// Events from the backend's search domain.
    pub(crate) fn on_search_event(
        &mut self,
        event: backend::search::Ev,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_search_results(event, cx);
    }

    /// Demo scenes open the search with a query, or the quick switcher, once the
    /// channel's messages are on screen.
    #[cfg(feature = "demo")]
    fn demo_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::demo::scenes::{current, Scene};
        if self.search.demo_done {
            return;
        }
        let scene = current();
        if !matches!(scene, Scene::SearchResults | Scene::QuickSwitcher) {
            self.search.demo_done = true;
            return;
        }
        if self.open_channel_id().is_none() || !self.chat.view.is_loaded() {
            return;
        }
        self.search.demo_done = true;
        cx.defer_in(window, move |this, window, cx| {
            if scene == Scene::SearchResults {
                this.open_search(window, cx);
                this.type_search(crate::demo::search::SCENE_QUERY, cx);
                this.submit_search(cx);
            } else {
                this.toggle_quick_switcher(window, cx);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, TestAppContext};

    /// Ctrl+K (Cmd+K) is bound once, to the quick switcher, with every domain's keys
    /// in place: nothing else takes it, in any context.
    #[gpui::test]
    fn the_quick_switcher_key_is_its_own(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.clear_key_bindings();
            crate::bind_keys(cx);
            let typed = [Keystroke::parse(SWITCHER_KEY).unwrap()];
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            let taken: Vec<&str> = keymap
                .bindings()
                .filter(|b| b.match_keystrokes(&typed).is_some())
                .map(|b| b.action().name())
                .collect();
            assert_eq!(taken, vec!["search::ToggleQuickSwitcher"]);
        });
    }
}
