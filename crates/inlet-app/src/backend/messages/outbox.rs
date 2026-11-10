// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sending a failed message again ("Retry"), with the nonce it was first sent with.
//! Fluxer's `sendOncePerNonce` drops a second send with the same nonce, but only within
//! 5 minutes, so a copy that reached the server the first time is not posted twice when
//! Retry comes within that time. The composer's send path does the work: it sends the
//! message exactly as it was first sent (with its `allowed_mentions`), and after an
//! unclear failure older than that it first looks whether the message arrived. The row
//! shows "sending" until the REST answer or the gateway's MESSAGE_CREATE with that nonce
//! removes it, or shows the new reason if it fails again. One request per click.

use crate::backend::Backend;

impl Backend {
    pub(super) fn resend(&mut self, channel_id: String, nonce: String, text: String) {
        self.composer_resend(channel_id, nonce, text);
    }
}
