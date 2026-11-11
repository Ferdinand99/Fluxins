// SPDX-License-Identifier: AGPL-3.0-or-later
//! The pinned messages of a channel: one page through the account's limiter (see
//! `inbox.rs` for the budget). Pinning and unpinning are message actions (`actions.rs`).

use inlet_protocol as p;

use super::inbox::{item, Answer, Ev, PINS_PAGE};
use super::parse::SharedNames;

/// Loads a page of pins (newest first) and answers it.
pub(super) async fn load(
    client: &p::Client,
    names: &SharedNames,
    channel_id: String,
    before: Option<String>,
    answer: Answer,
    fail: &impl Fn(p::Error) -> Option<String>,
) {
    match client.pins(&channel_id, before.as_deref(), PINS_PAGE).await {
        Ok(page) => {
            let request = answer.request();
            answer.send(page_event(channel_id, request, &page, names));
        }
        Err(err) => answer.fail(fail(err)),
    }
}

/// The answer for a page of pins: the messages for the panel, and the `before` of the
/// next page when there is one.
pub(crate) fn page_event(
    channel_id: String,
    request: u64,
    page: &p::models::PinsPage,
    names: &SharedNames,
) -> Ev {
    let next = page
        .has_more
        .then(|| page.items.last().map(|pin| pin.pinned_at.clone()))
        .flatten()
        .filter(|at| !at.is_empty());
    Ev::Pins {
        channel_id,
        request,
        items: page
            .items
            .iter()
            .map(|pin| item(&pin.message, names))
            .collect(),
        next,
    }
}
