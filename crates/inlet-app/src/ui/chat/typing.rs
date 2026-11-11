// SPDX-License-Identifier: AGPL-3.0-or-later
//! "Ada is typing…" in the composer's status row, from the account state's typing list
//! (an entry lasts 10 s after the last TYPING_START, see `inlet-state`). Up to three
//! names are shown, in bold, then "Several people are typing…", as in Fluxer.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, HighlightStyle, StyledText};
use inlet_state::{AccountState, Changes, ChannelId};

use crate::app::Inlet;
use crate::model::clean_name;
use crate::theme;

/// Names shown before the line becomes "Several people are typing…".
const MAX_NAMES: usize = 3;
/// Characters of one name.
const MAX_NAME_CHARS: usize = 32;

/// The typing line of a channel: its text and where the names are in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypingLine {
    pub text: String,
    pub names: Vec<std::ops::Range<usize>>,
}

/// Who is typing in `channel`, as one line; `None` when nobody is.
pub fn typing_line(state: &AccountState, channel: ChannelId) -> Option<TypingLine> {
    let users = state.typing(channel);
    if users.is_empty() {
        return None;
    }
    if users.len() > MAX_NAMES {
        return Some(TypingLine {
            text: t!("typing-several"),
            names: Vec::new(),
        });
    }
    let guild = state.channels().guild_of(channel);
    let names: Vec<String> = users
        .iter()
        .map(|u| {
            let nick = guild
                .and_then(|g| state.member(g, *u))
                .and_then(|m| m.display_name())
                .map(str::to_owned);
            let name = nick
                .or_else(|| state.user(*u).map(|u| u.display_name().to_owned()))
                .map(|n| clean_name(&n))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| t!("typing-someone"));
            name.chars().take(MAX_NAME_CHARS).collect()
        })
        .collect();
    let text = match names.as_slice() {
        [a] => t!("typing-one", a = a.clone()),
        [a, b] => t!("typing-two", a = a.clone(), b = b.clone()),
        [a, b, c, ..] => t!("typing-three", a = a.clone(), b = b.clone(), c = c.clone()),
        [] => return None,
    };
    // The names in the translated line, in order.
    let mut ranges = Vec::new();
    let mut from = 0;
    for name in &names {
        if let Some(at) = text[from..].find(name.as_str()) {
            ranges.push(from + at..from + at + name.len());
            from += at + name.len();
        }
    }
    Some(TypingLine {
        text,
        names: ranges,
    })
}

/// The typing line as an element (12 px, names in bold).
pub fn typing_element(line: TypingLine) -> AnyElement {
    let bold = HighlightStyle {
        font_weight: Some(FontWeight::SEMIBOLD),
        color: Some(theme::text_muted()),
        ..Default::default()
    };
    div()
        .text_size(px(12.))
        .text_color(theme::text_tertiary())
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(
            StyledText::new(line.text).with_highlights(line.names.into_iter().map(|r| (r, bold))),
        )
        .into_any_element()
}

impl Inlet {
    /// The composer shows the typing line of the open channel: draw it again when that
    /// line changed (a TYPING_START, an expiry, the person's message).
    pub(crate) fn typing_changed(&mut self, changes: &Changes, cx: &mut Context<Self>) {
        let Some(ch) = self.open_channel_id() else {
            return;
        };
        if changes.reset || changes.typing.contains(&ch) {
            self.composer.view.update(cx, |_, cx| cx.notify());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlet_protocol::Dispatch;
    use serde_json::json;

    fn state() -> AccountState {
        let ready: inlet_protocol::Ready = serde_json::from_value(json!({
            "session_id": "s", "user": {"id": "10", "username": "me"},
            "guilds": [{"id": "100", "properties": {"id": "100", "name": "G", "owner_id": "10"},
                        "channels": [{"id": "301", "type": 0, "guild_id": "100", "name": "general"}],
                        "roles": [{"id": "100", "name": "@everyone", "permissions": "2251799813685247", "position": 0}],
                        "members": [{"user": {"id": "10", "username": "me"}, "roles": []},
                                    {"user": {"id": "21", "username": "ana"}, "nick": "Ana\u{202E}", "roles": []}]}],
            "private_channels": [],
            "users": [{"id": "22", "username": "bo", "global_name": "Bo"},
                      {"id": "23", "username": "cy"}, {"id": "24", "username": "di"}],
            "read_states": [], "user_guild_settings": []
        }))
        .unwrap();
        AccountState::from_ready_at(&ready, 1_000)
    }

    fn typing(s: &mut AccountState, user: &str) {
        s.apply(&Dispatch::parse(
            "TYPING_START",
            json!({"channel_id": "301", "user_id": user, "guild_id": "100"}),
        ));
    }

    #[test]
    fn the_typing_line_names_up_to_three_people() {
        let mut s = state();
        let ch = ChannelId::parse("301").unwrap();
        assert_eq!(typing_line(&s, ch), None);
        typing(&mut s, "21");
        let one = typing_line(&s, ch).unwrap();
        // The nickname, cleaned of direction marks.
        assert_eq!(one.text, "Ana is typing\u{2026}");
        assert_eq!(&one.text[one.names[0].clone()], "Ana");
        typing(&mut s, "22");
        let two = typing_line(&s, ch).unwrap();
        assert_eq!(two.text, "Ana and Bo are typing\u{2026}");
        assert_eq!(two.names.len(), 2);
        typing(&mut s, "23");
        assert_eq!(
            typing_line(&s, ch).unwrap().text,
            "Ana, Bo and cy are typing\u{2026}"
        );
        typing(&mut s, "24");
        assert_eq!(
            typing_line(&s, ch).unwrap().text,
            "Several people are typing\u{2026}"
        );
        // Ten seconds later they are gone.
        s.tick(1_000 + inlet_state::limits::TYPING_EXPIRY_MS);
        assert_eq!(typing_line(&s, ch), None);
    }
}
