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

/// The action of the "Quick switcher" bar at the top of the DM list (Fluxer's). It is
/// in the sidebar's namespace because the sidebar sends it; this domain handles it. The
/// DM list uses this definition rather than its own.
pub mod sidebar_actions {
    gpui::actions!(
        sidebar,
        [
            /// Opens the quick switcher (never closes it).
            OpenQuickSwitcher
        ]
    );
}

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

fn field_focused(input: Option<&gpui::Entity<TextInput>>, window: &Window, cx: &App) -> bool {
    input.is_some_and(|i| gpui::Focusable::focus_handle(i.read(cx), cx).is_focused(window))
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

    /// Whether the quick switcher's field has the keyboard now.
    pub(crate) fn quick_switcher_field_focused(&self, window: &Window, cx: &App) -> bool {
        field_focused(self.search.switcher.input(), window, cx)
    }

    /// Whether the search field has the keyboard now.
    pub(crate) fn search_field_focused(&self, window: &Window, cx: &App) -> bool {
        field_focused(self.search.bar.input(), window, cx)
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

    /// The window's handlers of this domain (Ctrl+K, the DM list's quick switcher bar),
    /// and the demo scenes.
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
        .on_action(cx.listener(
            |this, _: &sidebar_actions::OpenQuickSwitcher, window, cx| {
                if !this.search.switcher.open {
                    this.toggle_quick_switcher(window, cx)
                }
            },
        ))
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
    use std::sync::Arc;

    use super::*;
    use crate::backend::accounts::{AccountInfo, AccountKey, Accounts};
    use crate::token_store::MemoryStore;
    use gpui::{AppContext, Entity, Keystroke, TestAppContext, VisualTestContext};
    use inlet_state::{AccountState, ChannelId};

    const GUILD: &str = "100";
    const OPEN: &str = "303";

    fn state() -> AccountState {
        let ready: inlet_protocol::Ready = serde_json::from_value(serde_json::json!({
            "session_id": "s",
            "user": {"id": "10", "username": "me", "mfa_enabled": false},
            "guilds": [{
                "id": GUILD,
                "properties": {"id": GUILD, "name": "g", "owner_id": "1"},
                "roles": [{"id": GUILD, "name": "@everyone", "position": 0,
                           "permissions": "274877975552"}],
                "channels": [
                    {"id": "301", "type": 0, "name": "chan-a", "position": 0},
                    {"id": "302", "type": 0, "name": "chan-b", "position": 1},
                    {"id": OPEN, "type": 0, "name": "general", "position": 2}
                ],
                "members": [{"user": {"id": "10", "username": "me"}, "roles": [],
                             "joined_at": "2026-01-01T00:00:00Z"}]
            }],
            "private_channels": [], "users": [], "read_states": [], "user_guild_settings": []
        }))
        .unwrap();
        AccountState::from_ready_at(&ready, 1_800_000_000_000)
    }

    /// A window with Inlet signed in (by hand) to one account, a guild with the
    /// channels `chan-a`, `chan-b` and `general` (open), and the app's keys.
    fn rig(cx: &mut TestAppContext) -> (Entity<Inlet>, &mut VisualTestContext) {
        cx.executor().allow_parking();
        cx.update(crate::bind_keys);
        let (inlet, cx) = cx.add_window_view(|window, cx| {
            Inlet::new_with(window, cx, |tx| {
                crate::backend::start_with(tx, |ui| {
                    Accounts::new(Arc::new(MemoryStore::default()), None, Some(ui))
                })
            })
        });
        // The backend answers that no account is stored; after that it is quiet.
        for _ in 0..200 {
            cx.run_until_parked();
            if inlet.read_with(cx, |i, _| i.screen == Screen::SignIn) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        inlet.update(cx, |this, cx| {
            let key = AccountKey::new("https://a.example", "10").unwrap();
            this.switcher.accounts = vec![AccountInfo {
                key: key.clone(),
                name: "me".into(),
                handle: "me".into(),
            }];
            this.switcher.active = Some(key);
            this.account = Some(cx.new(|_| state()));
            this.selected_guild = Some(GUILD.into());
            this.selected_channel = Some(OPEN.into());
            this.screen = Screen::Main;
            cx.notify();
        });
        cx.run_until_parked();
        (inlet, cx)
    }

    fn chosen_channel(inlet: &Entity<Inlet>, cx: &mut VisualTestContext) -> Option<ChannelId> {
        inlet.read_with(cx, |i, _| {
            i.search.switcher.chosen.as_ref().and_then(|c| c.channel)
        })
    }

    /// Up and Down move in the quick switcher's list, and Enter opens the row that is
    /// highlighted: here the second one.
    #[gpui::test]
    fn down_then_enter_opens_the_second_row(cx: &mut TestAppContext) {
        let (inlet, cx) = rig(cx);
        cx.update(|window, cx| inlet.update(cx, |this, cx| this.toggle_quick_switcher(window, cx)));
        cx.run_until_parked();
        cx.simulate_input("chan");
        cx.run_until_parked();
        cx.simulate_keystrokes("down up down enter");
        cx.run_until_parked();
        assert_eq!(chosen_channel(&inlet, cx), ChannelId::parse("302"));
        assert!(!inlet.read_with(cx, |i, _| i.search.switcher.open));
    }

    /// A deep-link prompt that opens over the quick switcher takes the keyboard in the
    /// same frame: Enter then never picks a row of the switcher behind it.
    #[gpui::test]
    fn enter_under_a_link_prompt_picks_nothing(cx: &mut TestAppContext) {
        let (inlet, cx) = rig(cx);
        cx.update(|window, cx| inlet.update(cx, |this, cx| this.toggle_quick_switcher(window, cx)));
        cx.run_until_parked();
        cx.simulate_input("chan");
        cx.run_until_parked();
        inlet.update(cx, |this, cx| {
            this.overlays.link_prompt = Some(crate::backend::platform::links::Prompt::Unavailable);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(chosen_channel(&inlet, cx), None);
        assert!(inlet.read_with(cx, |i, _| i.search.switcher.open));
    }

    /// The pins or the inbox opened over the search column take the keyboard at once:
    /// what is typed next no longer lands in the search field.
    #[gpui::test]
    fn a_panel_over_the_search_takes_the_keyboard(cx: &mut TestAppContext) {
        let (inlet, cx) = rig(cx);
        cx.update(|window, cx| inlet.update(cx, |this, cx| this.open_search(window, cx)));
        cx.run_until_parked();
        cx.simulate_input("ab");
        cx.run_until_parked();
        let typed = |inlet: &Entity<Inlet>, cx: &mut VisualTestContext| {
            inlet.read_with(cx, |i, cx| {
                i.search.bar.input().map(|e| e.read(cx).text().to_string())
            })
        };
        assert_eq!(typed(&inlet, cx).as_deref(), Some("ab"));
        inlet.update(cx, |this, cx| {
            this.inbox.open = Some(crate::ui::inbox::Panel::Inbox);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_input("cd");
        cx.run_until_parked();
        assert_eq!(typed(&inlet, cx).as_deref(), Some("ab"));
        let focused = cx.update(|window, cx| inlet.read(cx).search_field_focused(window, cx));
        assert!(!focused);
    }

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
