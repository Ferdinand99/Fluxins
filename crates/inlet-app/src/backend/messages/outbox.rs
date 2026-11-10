// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sending a failed message again ("Retry"), with the nonce it was first sent with.
//! Fluxer's `sendOncePerNonce` drops a second send with the same nonce, but only within
//! 5 minutes, so a copy that reached the server the first time is not posted twice when
//! Retry comes within that time. The composer's send path does the work: it sends the
//! message exactly as it was first sent (with its `allowed_mentions`). After an unclear
//! failure (a timeout, a broken connection, a server error) a Retry later than that
//! first reads the channel's messages since the first send, and sends only if no copy
//! is there (see [`is_own_copy`]). The row shows "sending" until the REST answer or the
//! gateway's MESSAGE_CREATE with that nonce removes it, or shows the new reason if it
//! fails again. One request per click, two for a late retry that looks first.

use inlet_protocol as p;

use crate::backend::Backend;

/// How much older than the local send time a copy's ID may look: the local clock and
/// the server's may differ by a few seconds.
pub const CLOCK_SLACK_MS: u64 = 10_000;

/// Whether `m` is a copy of a message this account sent at `sent_ms` (Unix ms): written
/// by `me`, with the same wire text and the same number of attachments, and created no
/// earlier than the send. Fluxer puts the nonce only in the answer to the send and in
/// the gateway's MESSAGE_CREATE, never in message lists, so a list can only be matched
/// on what it does carry. An identical message the person sent in that time also
/// counts; then no text is lost either.
pub fn is_own_copy(
    m: &p::Message,
    me: &str,
    content: &str,
    attachments: usize,
    sent_ms: u64,
) -> bool {
    m.author.id == me
        && m.content == content
        && m.attachments.len() == attachments
        && p::models::snowflake_timestamp_ms(&m.id)
            .is_some_and(|created| created.saturating_add(CLOCK_SLACK_MS) >= sent_ms)
}

impl Backend {
    pub(super) fn resend(&mut self, channel_id: String, nonce: String, text: String) {
        self.composer_resend(channel_id, nonce, text);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A message as a list gives it: no nonce.
    pub(crate) fn listed(id_ms: u64, author: &str, content: &str, files: usize) -> p::Message {
        let attachments: Vec<_> = (0..files)
            .map(|i| {
                json!({"id": (900 + i).to_string(), "filename": "a.png", "size": 1,
                       "url": "https://cdn.invalid/a.png"})
            })
            .collect();
        serde_json::from_value(json!({
            "id": p::models::snowflake_at_ms(id_ms),
            "channel_id": "301",
            "author": {"id": author, "username": "u"},
            "content": content,
            "attachments": attachments,
        }))
        .unwrap()
    }

    #[test]
    fn a_copy_is_own_text_from_the_send_on() {
        let sent = 1_800_000_000_000;
        let ok = listed(sent + 300, "42", "hi <@7>", 0);
        assert!(is_own_copy(&ok, "42", "hi <@7>", 0, sent));
        // A server clock a little behind still matches.
        let behind = listed(sent - 2_000, "42", "hi <@7>", 0);
        assert!(is_own_copy(&behind, "42", "hi <@7>", 0, sent));

        let other = listed(sent + 300, "43", "hi <@7>", 0);
        assert!(
            !is_own_copy(&other, "42", "hi <@7>", 0, sent),
            "someone else's"
        );
        let text = listed(sent + 300, "42", "hi @7", 0);
        assert!(!is_own_copy(&text, "42", "hi <@7>", 0, sent), "other text");
        let files = listed(sent + 300, "42", "hi <@7>", 1);
        assert!(
            !is_own_copy(&files, "42", "hi <@7>", 0, sent),
            "attachments"
        );
        let older = listed(sent - 60_000, "42", "hi <@7>", 0);
        assert!(
            !is_own_copy(&older, "42", "hi <@7>", 0, sent),
            "from before the send"
        );
    }
}
