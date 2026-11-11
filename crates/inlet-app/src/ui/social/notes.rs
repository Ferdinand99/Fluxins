// SPDX-License-Identifier: AGPL-3.0-or-later
//! Personal notes: the account's own channel (type 999, its ID is the account's), which
//! the DM list shows as its own row under Friends, as Fluxer does. It opens like a DM;
//! the state and the sidebar's channel list learn it when it is first opened.

use gpui::{Context, Window};

use crate::app::Inlet;
use crate::backend::messages::parse::to_channel;

impl Inlet {
    /// The Personal notes row: opens the account's notes.
    pub(crate) fn open_personal_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(account) = self.account.clone() else {
            return;
        };
        let id = account
            .update(cx, |s, _| s.ensure_personal_notes())
            .to_string();
        self.keep_personal_notes(cx);
        if self.selected_guild.is_some() {
            self.follow_channel_link(None, id, window, cx);
        } else {
            self.select_channel(id, window, cx);
        }
    }

    /// Keeps the notes channel in the DM list's channels while it is known, so an open
    /// notes channel stays open when the list is loaded again.
    pub(crate) fn keep_personal_notes(&mut self, cx: &mut Context<Self>) {
        if self.selected_guild.is_some() {
            return;
        }
        let Some(state) = self.account.as_ref().map(|a| a.read(cx)) else {
            return;
        };
        let Some(notes) = state.channel(inlet_state::ChannelId(state.me_id().0)) else {
            return;
        };
        let shown = to_channel(notes);
        if !self.channels.iter().any(|c| c.id == shown.id) {
            self.channels.push(shown);
        }
    }
}

#[cfg(test)]
mod tests {
    use inlet_protocol::Ready;
    use inlet_state::AccountState;
    use serde_json::json;

    use crate::backend::messages::parse::to_channel;
    use crate::model::ChannelKind;

    #[test]
    fn personal_notes_open_as_a_text_channel_of_the_account() {
        let ready: Ready = serde_json::from_value(json!({
            "session_id": "s", "user": {"id": "41", "username": "me"}, "guilds": [],
            "users": [], "private_channels": [], "read_states": [], "user_guild_settings": []
        }))
        .unwrap();
        let mut s = AccountState::from_ready_at(&ready, 1);
        let id = s.ensure_personal_notes();
        let shown = to_channel(s.channel(id).unwrap());
        assert_eq!(shown.id, "41");
        assert_eq!(shown.kind, ChannelKind::Text);
        assert!(shown.is_messageable());
        assert_eq!(shown.name, t!("channel-personal-notes"));
    }
}
