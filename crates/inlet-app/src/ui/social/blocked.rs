// SPDX-License-Identifier: AGPL-3.0-or-later
//! Messages from blocked people, as Fluxer's `BlockedMessageGroups`: each run of them in
//! a channel folds into one centred line ("2 blocked messages") on the sidebar's
//! colour; a click shows the run, another folds it again. The folding happens where
//! the rows are built (`ui/chat/messages.rs`, [`crate::ui::chat::messages::Row::Blocked`]),
//! so it holds in the chat and the thread panel. Notifications and typing already
//! leave blocked people out (`inlet-state`). The Blocked tab of the friends page lists
//! them (`friends.rs`).

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};
use inlet_state::MessageId;

use crate::app::Inlet;
use crate::theme;

impl Inlet {
    /// The line of a run of blocked people's messages.
    pub(crate) fn render_blocked_run(
        &mut self,
        first: MessageId,
        count: usize,
        revealed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(SharedString::from(format!("blocked-{first}")))
            .debug_selector(|| "blocked-run".into())
            .w_full()
            .py(px(4.))
            .px_4()
            .flex()
            .justify_center()
            .bg(theme::bg_app())
            .cursor_pointer()
            .text_size(px(12.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme::text_muted())
            .hover(|s| s.text_color(theme::text_secondary()))
            .child(if revealed {
                t!("social-blocked-messages-hide", count = count)
            } else {
                t!("social-blocked-messages", count = count)
            })
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_blocked_run(first, cx)))
            .into_any_element()
    }

    /// Shows or folds the run that starts at `first`, in the chat and in the thread
    /// panel (the run is in one of them), and rebuilds both lists.
    pub(crate) fn toggle_blocked_run(&mut self, first: MessageId, cx: &mut Context<Self>) {
        self.chat.view.toggle_blocked(first);
        self.refresh_rows(cx);
        if let Some(panel) = self.threads.panel.as_mut() {
            panel.view.toggle_blocked(first);
            self.refresh_panel_rows(cx);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "demo")]
    mod live {
        use gpui::{AppContext, TestAppContext};
        use inlet_protocol::Dispatch;
        use inlet_state::{AccountState, ChannelId, GuildId, MessagesPage};
        use serde_json::json;

        use crate::app::Inlet;
        use crate::backend::{messages, Event};
        use crate::ui::chat::messages::Row;
        use crate::ui::threads::panel::Panel;

        const DM: &str = "100";
        const GUILD: &str = "500";
        const GENERAL: &str = "501";
        const THREAD: &str = "502";

        fn msg(id: u64, channel: &str, author: &str) -> inlet_protocol::Message {
            serde_json::from_value(json!({
                "id": (id << 22).to_string(), "channel_id": channel, "type": 0,
                "content": format!("m{id}"), "timestamp": "2026-01-01T10:00:00+00:00",
                "author": {"id": author, "username": format!("u{author}")}
            }))
            .unwrap()
        }

        fn state() -> AccountState {
            let ready: inlet_protocol::Ready = serde_json::from_value(json!({
                "session_id": "s", "user": {"id": "1", "username": "me"}, "users": [],
                "guilds": [{
                    "id": GUILD,
                    "properties": {"id": GUILD, "name": "g", "owner_id": "1"},
                    "roles": [{"id": GUILD, "name": "@everyone", "position": 0,
                               "permissions": "274877975552"}],
                    "channels": [{"id": GENERAL, "type": 0, "name": "general", "position": 0}],
                    "threads": [{"id": THREAD, "type": 11, "parent_id": GENERAL, "name": "t"}],
                    "members": [{"user": {"id": "1", "username": "me"}, "roles": [],
                                 "joined_at": "2026-01-01T00:00:00Z"}]
                }],
                "private_channels": [{"id": DM, "type": 3,
                    "recipients": [{"id": "2", "username": "u2"}, {"id": "3", "username": "u3"}]}],
                "relationships": [], "read_states": [], "user_guild_settings": []
            }))
            .unwrap();
            let mut s = AccountState::from_ready_at(&ready, 1_800_000_000_000);
            for ch in [DM, THREAD] {
                let id = ChannelId::parse(ch).unwrap();
                let _ = s.begin_load(id);
                let page = vec![msg(3, ch, "2"), msg(2, ch, "3"), msg(1, ch, "2")];
                let _ = s.apply_messages(id, MessagesPage::latest(page, 50));
            }
            s
        }

        fn folded(rows: &[Row]) -> Vec<String> {
            rows.iter()
                .filter_map(|r| match r {
                    Row::Message(m) => Some(format!("m{}", m.id.0 >> 22)),
                    Row::Blocked {
                        count, revealed, ..
                    } => Some(format!(
                        "blocked {count}{}",
                        if *revealed { " shown" } else { "" }
                    )),
                    _ => None,
                })
                .collect()
        }

        /// A block from the gateway folds the open DM's messages at once, through the
        /// path every event takes, and the thread panel's fold opens on a click.
        #[gpui::test]
        fn blocking_is_live_and_the_panel_fold_opens(cx: &mut TestAppContext) {
            let (inlet, cx) = cx.add_window_view(Inlet::detached);
            inlet.update_in(cx, |this, window, cx| {
                this.account = Some(cx.new(|_| state()));
                this.selected_channel = Some(DM.into());
                this.refresh_rows(cx);
                assert_eq!(folded(&this.chat.view.rows), ["m1", "m2", "m3"]);

                let block = Dispatch::parse(
                    "RELATIONSHIP_ADD",
                    json!({"id": "3", "type": 2, "user": {"id": "3", "username": "u3"}}),
                );
                let event = Event::Messages(messages::Ev::Dispatch {
                    dispatch: Box::new(block),
                    rendered: Vec::new(),
                });
                this.handle_event(event, window, cx);
                assert_eq!(
                    folded(&this.chat.view.rows),
                    ["m1", "blocked 1", "m3"],
                    "folded without a new message"
                );

                // The same person's message in a thread: the panel folds it, and a
                // click on the line shows it there.
                let thread = ChannelId::parse(THREAD).unwrap();
                let guild = GuildId::parse(GUILD).unwrap();
                let parent = ChannelId::parse(GENERAL).unwrap();
                this.threads.panel = Some(Panel::new(thread, guild, parent, &this.chat.view, 0));
                this.refresh_panel_rows(cx);
                let panel_rows =
                    |this: &Inlet| folded(&this.threads.panel.as_ref().unwrap().view.rows);
                assert_eq!(panel_rows(this), ["m1", "blocked 1", "m3"]);
                this.toggle_blocked_run(inlet_state::MessageId(2 << 22), cx);
                assert_eq!(panel_rows(this), ["m1", "blocked 1 shown", "m2", "m3"]);

                // Unblocked from elsewhere: both lists show the messages again.
                let unblock = Dispatch::parse("RELATIONSHIP_REMOVE", json!({"id": "3"}));
                let event = Event::Messages(messages::Ev::Dispatch {
                    dispatch: Box::new(unblock),
                    rendered: Vec::new(),
                });
                this.handle_event(event, window, cx);
                assert_eq!(folded(&this.chat.view.rows), ["m1", "m2", "m3"]);
                assert_eq!(panel_rows(this), ["m1", "m2", "m3"]);
            });
        }
    }

    use inlet_protocol::{Dispatch, Ready};
    use inlet_state::{AccountState, ChannelId, MessageId, MessagesPage};
    use serde_json::json;

    use crate::ui::chat::messages::{MessageView, Row};

    fn msg(id: u64, author: &str) -> inlet_protocol::Message {
        serde_json::from_value(json!({
            "id": (id << 22).to_string(), "channel_id": "100", "type": 0,
            "content": format!("m{id}"), "timestamp": "2026-01-01T10:00:00+00:00",
            "author": {"id": author, "username": format!("u{author}")}
        }))
        .unwrap()
    }

    #[test]
    fn blocked_user_messages_are_collapsed() {
        let ready: Ready = serde_json::from_value(json!({
            "session_id": "s", "user": {"id": "1", "username": "me"},
            "guilds": [], "users": [],
            "private_channels": [{"id": "100", "type": 3,
                "recipients": [{"id": "2", "username": "u2"}, {"id": "3", "username": "u3"}]}],
            "relationships": [{"id": "3", "type": 2}],
            "read_states": [], "user_guild_settings": []
        }))
        .unwrap();
        let mut state = AccountState::from_ready_at(&ready, 1);
        let ch = ChannelId(100);
        let _ = state.begin_load(ch);
        // 2, then three from the blocked 3, then 2, then 3 again.
        let page: Vec<_> = [(1, "2"), (2, "3"), (3, "3"), (4, "3"), (5, "2"), (6, "3")]
            .into_iter()
            .map(|(i, a)| msg(i, a))
            .rev()
            .collect();
        let _ = state.apply_messages(ch, MessagesPage::latest(page, 50));
        let mut view = MessageView::default();
        let shape = |rows: &[Row]| -> Vec<String> {
            rows.iter()
                .filter_map(|r| match r {
                    Row::Message(m) => Some(format!("m{}", m.id.0 >> 22)),
                    Row::Blocked {
                        count, revealed, ..
                    } => Some(format!(
                        "blocked {count}{}",
                        if *revealed { " shown" } else { "" }
                    )),
                    _ => None,
                })
                .collect()
        };
        let rows = view.build_rows(&state, ch);
        assert_eq!(shape(&rows), ["m1", "blocked 3", "m5", "blocked 1"]);

        // A click shows the run, with its messages under the line.
        view.toggle_blocked(MessageId(2 << 22));
        let rows = view.build_rows(&state, ch);
        assert_eq!(
            shape(&rows),
            ["m1", "blocked 3 shown", "m2", "m3", "m4", "m5", "blocked 1"]
        );
        // Another folds it again.
        view.toggle_blocked(MessageId(2 << 22));
        assert_eq!(shape(&view.build_rows(&state, ch)).len(), 4);

        // Unblocked: the messages show as usual.
        let _ = state.apply(&Dispatch::parse("RELATIONSHIP_REMOVE", json!({"id": "3"})));
        let rows = view.build_rows(&state, ch);
        assert_eq!(shape(&rows), ["m1", "m2", "m3", "m4", "m5", "m6"]);
    }
}
