// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Pending tab of the friends page: incoming requests (accept, ignore) first, then
//! the ones the account sent (cancel). Its rows come from the account state, so a
//! RELATIONSHIP_ADD, RELATIONSHIP_UPDATE or RELATIONSHIP_REMOVE shows at once, from
//! this client, another client or the other person. The tab's count is on its button
//! and on the Friends row of the DM list.

use inlet_state::AccountState;

use super::friends::{rows, PersonRow, Tab};

/// The Pending tab's rows for `query`.
pub fn pending_rows(state: &AccountState, query: &str) -> Vec<PersonRow> {
    rows(state, Tab::Pending, query)
}

/// Incoming requests: the badge on the Pending tab and on the Friends row.
pub fn incoming(state: &AccountState) -> usize {
    state.relationships().incoming_count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlet_protocol::{Dispatch, Ready};
    use inlet_state::relationships::Kind;
    use inlet_state::UserId;
    use serde_json::json;

    fn state() -> AccountState {
        let ready: Ready = serde_json::from_value(json!({
            "session_id": "s", "user": {"id": "1", "username": "me"},
            "guilds": [], "private_channels": [], "users": [{"id": "3", "username": "cy"}],
            "relationships": [{"id": "3", "type": 4}],
            "read_states": [], "user_guild_settings": []
        }))
        .unwrap();
        AccountState::from_ready_at(&ready, 1)
    }

    fn event(name: &str, data: serde_json::Value) -> Dispatch {
        Dispatch::parse(name, data)
    }

    #[test]
    fn friend_request_events_update_the_pending_tab() {
        let mut s = state();
        // The request the account sent is there from READY, with its name.
        let list = pending_rows(&s, "");
        assert_eq!(list.len(), 1);
        assert_eq!(
            (list[0].kind, list[0].name.as_str()),
            (Kind::Outgoing, "cy")
        );
        assert_eq!(incoming(&s), 0);

        // Someone sends a request: it shows first, and counts.
        let c = s.apply(&event(
            "RELATIONSHIP_ADD",
            json!({"id": "2", "type": 3, "user": {"id": "2", "username": "bo"}}),
        ));
        assert!(c.relationships);
        let list = pending_rows(&s, "");
        assert_eq!(
            list.iter()
                .map(|r| (r.kind, r.name.as_str()))
                .collect::<Vec<_>>(),
            [(Kind::Incoming, "bo"), (Kind::Outgoing, "cy")]
        );
        assert_eq!(incoming(&s), 1);
        assert_eq!(pending_rows(&s, "B").len(), 1, "the search matches names");

        // Accepted (here or elsewhere): it leaves the tab and is a friend.
        s.apply(&event("RELATIONSHIP_UPDATE", json!({"id": "2", "type": 1})));
        assert_eq!(pending_rows(&s, "").len(), 1);
        assert_eq!(incoming(&s), 0);
        let friends = rows(&s, Tab::All, "");
        assert_eq!(friends.len(), 1);
        assert_eq!(friends[0].user, UserId(2));
        assert_eq!(friends[0].name, "bo", "the name from the request stays");

        // The other side cancels the request the account sent: gone.
        s.apply(&event("RELATIONSHIP_REMOVE", json!({"id": "3"})));
        assert!(pending_rows(&s, "").is_empty());
    }
}
