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
            .on_click(cx.listener(move |this, _, _, cx| {
                this.chat.view.toggle_blocked(first);
                if let Some(panel) = this.threads.panel.as_mut() {
                    panel.view.toggle_blocked(first);
                }
                this.refresh_rows(cx);
                cx.notify();
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
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
