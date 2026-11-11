// SPDX-License-Identifier: AGPL-3.0-or-later
//! Who reacted: right-click a reaction bubble for the people behind it (the first
//! page, [`actions::REACTORS_PAGE`] names), with a tab per emoji of the message.
//!
//! Budget: one request per list that is opened, never two at once, at most one a
//! second, and an answer is kept for [`KEEP_FOR`], so flipping between tabs or opening
//! the same list again asks the server nothing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};
use inlet_protocol as p;
use inlet_state::{ChannelId, MessageId};

use super::reactions::{emoji_el, ReactionChip};
use crate::app::Inlet;
use crate::backend::messages::{actions, Cmd};
use crate::theme;

/// How long a list of names is kept.
pub const KEEP_FOR: Duration = Duration::from_secs(30);
/// Shortest time between two requests.
pub const MIN_GAP: Duration = Duration::from_secs(1);
/// Lists kept at most.
const MAX_KEPT: usize = 64;
/// A request whose answer never came stops blocking new ones after this.
const ANSWER_WITHIN: Duration = Duration::from_secs(30);

/// The key of a list: the message and the emoji (custom emoji by ID).
pub fn key(message: MessageId, emoji: &p::Emoji) -> (MessageId, String) {
    (
        message,
        emoji.id.clone().unwrap_or_else(|| emoji.name.clone()),
    )
}

/// A list of names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    pub names: Vec<String>,
    pub more: bool,
}

/// What showing a list needs now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    /// The answer is here.
    Ready(Names),
    /// Ask the server, with this request number.
    Fetch(u64),
    /// A request is on its way, or the last one was less than [`MIN_GAP`] ago: ask again
    /// at this time.
    Wait(Instant),
}

/// The lists of names and the requests for them.
#[derive(Debug, Default)]
pub struct Reactors {
    kept: HashMap<(MessageId, String), (Instant, Names)>,
    in_flight: Option<(u64, (MessageId, String), Instant)>,
    last: Option<Instant>,
    next: u64,
    /// The reason the last request failed, for the list it was for.
    pub failed: Option<((MessageId, String), String)>,
}

impl Reactors {
    /// What showing the list `key` needs at `now`.
    pub fn want(&mut self, key: &(MessageId, String), now: Instant) -> Want {
        if let Some((at, names)) = self.kept.get(key) {
            if now.saturating_duration_since(*at) < KEEP_FOR {
                return Want::Ready(names.clone());
            }
        }
        if self
            .in_flight
            .as_ref()
            .is_some_and(|(_, _, at)| now.saturating_duration_since(*at) >= ANSWER_WITHIN)
        {
            // Lost (a task that panicked): it no longer holds the others up.
            self.in_flight = None;
        }
        if self.in_flight.is_some() {
            return Want::Wait(now + MIN_GAP);
        }
        if let Some(last) = self.last {
            if now.saturating_duration_since(last) < MIN_GAP {
                return Want::Wait(last + MIN_GAP);
            }
        }
        self.next += 1;
        self.in_flight = Some((self.next, key.clone(), now));
        self.last = Some(now);
        if self.failed.as_ref().is_some_and(|(k, _)| k == key) {
            self.failed = None;
        }
        Want::Fetch(self.next)
    }

    /// The answer to `request`. An answer to an older request is dropped.
    pub fn answered(&mut self, request: u64, result: Result<Names, String>, now: Instant) {
        let Some((current, key, _)) = self.in_flight.take_if(|(r, _, _)| *r == request) else {
            return;
        };
        debug_assert_eq!(current, request);
        match result {
            Ok(names) => {
                self.kept
                    .retain(|_, (at, _)| now.saturating_duration_since(*at) < KEEP_FOR);
                if self.kept.len() >= MAX_KEPT {
                    self.kept.clear();
                }
                self.kept.insert(key, (now, names));
            }
            Err(reason) => self.failed = Some((key, reason)),
        }
    }

    /// Forgets everything (signed out).
    pub fn clear(&mut self) {
        *self = Reactors {
            next: self.next,
            ..Reactors::default()
        };
    }
}

impl Inlet {
    /// Opens who reacted with `emoji` on message `id`, at `at` (window coordinates).
    pub(crate) fn open_reactors(
        &mut self,
        ch: ChannelId,
        id: MessageId,
        emoji: p::Emoji,
        at: gpui::Point<gpui::Pixels>,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.open_overlay(
            super::context_menu::Overlay::Reactors { ch, id, emoji, at },
            window,
            cx,
        );
        self.fetch_reactors(cx);
    }

    /// Asks for the open list if it is not kept, within the budget; asks again later
    /// when it has to wait.
    pub(crate) fn fetch_reactors(&mut self, cx: &mut Context<Self>) {
        let Some(super::context_menu::Overlay::Reactors { ch, id, emoji, .. }) =
            self.chat.actions.overlay.clone()
        else {
            return;
        };
        match self
            .chat
            .actions
            .reactors
            .want(&key(id, &emoji), Instant::now())
        {
            Want::Ready(_) => {}
            Want::Fetch(request) => self.backend.send(Cmd::Action(actions::Cmd::Reactors {
                channel_id: ch.to_string(),
                message_id: id.to_string(),
                emoji,
                request,
            })),
            Want::Wait(at) => {
                let wait = at.saturating_duration_since(Instant::now());
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(wait).await;
                    let _ = this.update(cx, |this, cx| this.fetch_reactors(cx));
                })
                .detach();
            }
        }
    }

    /// The list's content: a tab per emoji of the message, and the names.
    pub(super) fn render_reactors(
        &mut self,
        ch: ChannelId,
        id: MessageId,
        emoji: &p::Emoji,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chips: Vec<ReactionChip> = self
            .account
            .as_ref()
            .and_then(|a| a.read(cx).messages(ch)?.get(id).map(ReactionChip::of))
            .unwrap_or_default();
        let k = key(id, emoji);
        let kept = match self.chat.actions.reactors.kept.get(&k) {
            Some((at, names)) if at.elapsed() < KEEP_FOR => Some(names.clone()),
            _ => None,
        };
        let failed = self
            .chat
            .actions
            .reactors
            .failed
            .as_ref()
            .filter(|(fk, _)| *fk == k)
            .map(|(_, r)| r.clone());
        let mut tabs = div().flex().flex_row().flex_wrap().gap(px(4.)).pb(px(8.));
        for (i, chip) in chips.iter().enumerate() {
            let selected = chip.emoji.same_reaction(emoji);
            let e = chip.emoji.clone();
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("reactors-tab-{id}-{i}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .h(px(28.))
                    .px(px(6.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme::floating_hover()))
                    .hover(|s| s.bg(theme::floating_hover()))
                    .child(emoji_el(&format!("reactors-e-{id}-{i}"), &chip.emoji, 11.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(chip.count.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(super::context_menu::Overlay::Reactors { emoji, .. }) =
                            this.chat.actions.overlay.as_mut()
                        {
                            *emoji = e.clone();
                        }
                        this.fetch_reactors(cx);
                        cx.notify();
                    })),
            );
        }
        let body = match (kept, failed) {
            (Some(list), _) => {
                let mut col = div().flex().flex_col().gap(px(2.));
                if list.names.is_empty() {
                    col = col.child(muted(t!("reactors-none")));
                }
                for name in list.names {
                    col = col.child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text())
                            .child(name),
                    );
                }
                if list.more {
                    col = col.child(muted(t!("reactors-more")));
                }
                col.into_any_element()
            }
            (None, Some(reason)) => {
                muted(t!("reactors-failed", reason = reason)).into_any_element()
            }
            (None, None) => muted(t!("reactors-loading")).into_any_element(),
        };
        div()
            .w(px(240.))
            .flex()
            .flex_col()
            .child(tabs)
            .child(body)
            .into_any_element()
    }
}

fn muted(text: String) -> gpui::Div {
    div()
        .text_size(px(13.))
        .text_color(theme::text_tertiary())
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_reacted_is_asked_once_a_second_and_kept() {
        let t0 = Instant::now();
        let mut r = Reactors::default();
        let thumbs = key(MessageId(5), &p::Emoji::unicode("\u{1F44D}"));
        let blob = key(MessageId(5), &p::Emoji::custom("77", "blob", false));
        let Want::Fetch(first) = r.want(&thumbs, t0) else {
            panic!("the first list is asked for");
        };
        // Never two at once.
        assert!(matches!(r.want(&blob, t0), Want::Wait(_)));
        let names = Names {
            names: vec!["ada".into()],
            more: false,
        };
        r.answered(first, Ok(names.clone()), t0);
        // Kept: no request for the same list within 30 s.
        assert_eq!(
            r.want(&thumbs, t0 + Duration::from_secs(29)),
            Want::Ready(names)
        );
        // At most one a second.
        assert_eq!(
            r.want(&blob, t0 + Duration::from_millis(500)),
            Want::Wait(t0 + MIN_GAP)
        );
        let Want::Fetch(second) = r.want(&blob, t0 + MIN_GAP) else {
            panic!("asked after the gap");
        };
        // A late answer to an older request changes nothing.
        r.answered(
            first,
            Ok(Names {
                names: vec![],
                more: false,
            }),
            t0 + MIN_GAP,
        );
        assert!(matches!(r.want(&blob, t0 + MIN_GAP), Want::Wait(_)));
        r.answered(second, Err("no".into()), t0 + MIN_GAP);
        assert!(r.failed.is_some());
        // Kept lists expire.
        assert!(matches!(
            r.want(&thumbs, t0 + KEEP_FOR + MIN_GAP * 2),
            Want::Fetch(_)
        ));
        // An answer that never comes stops blocking after a while.
        let t1 = t0 + KEEP_FOR * 3;
        let Want::Fetch(_) = r.want(&thumbs, t1) else {
            panic!("asked");
        };
        assert!(matches!(r.want(&blob, t1 + MIN_GAP), Want::Wait(_)));
        assert!(matches!(r.want(&blob, t1 + ANSWER_WITHIN), Want::Fetch(_)));
    }
}
