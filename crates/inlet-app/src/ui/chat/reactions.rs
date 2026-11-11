// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reactions: the bubbles under a message (emoji and count, highlighted when the account
//! reacted), clicking one to add or take back the account's reaction, and the quick
//! reactions of the action bar.
//!
//! A click shows at once: the account state takes the same event the gateway would send
//! for the account's own reaction (the echo of the real one changes nothing, own
//! reactions are idempotent in the state). The request waits: [`ReactionClicks`] collects
//! the clicks on a message for [`WINDOW`] and then sends only the net change per emoji.
//! Clicking a reaction on and off ten times sends nothing; an odd number of clicks sends
//! one request. A request the server refuses undoes the early change.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};
use inlet_protocol as p;
use inlet_state::{ChannelId, MessageId, UserId};

use super::messages::MessageRow;
use crate::app::Inlet;
use crate::backend::messages::{actions, Cmd};
use crate::model;
use crate::theme;
use crate::ui::markdown::{self, Block, RenderCtx};

/// How long clicks on one message are collected before the net change goes out
/// (the request budget: reaction clicks are coalesced over 500 ms per message).
pub const WINDOW: Duration = Duration::from_millis(500);
/// Messages with clicks waiting at once; more clicks are ignored until a window ends.
pub const MAX_MESSAGES: usize = 50;
/// Emoji with clicks waiting per message.
pub const MAX_EMOJI: usize = 20;
/// The quick reactions in the action bar.
pub const QUICK: [&str; 3] = ["\u{1F44D}", "\u{2764}\u{FE0F}", "\u{1F602}"];
/// The emoji offered until the emoji picker arrives (the bar's "add reaction").
pub const PICKER: [&str; 16] = [
    "\u{1F44D}",
    "\u{1F44E}",
    "\u{2764}\u{FE0F}",
    "\u{1F602}",
    "\u{1F62E}",
    "\u{1F622}",
    "\u{1F621}",
    "\u{1F389}",
    "\u{1F525}",
    "\u{1F440}",
    "\u{1F64F}",
    "\u{1F44F}",
    "\u{2705}",
    "\u{274C}",
    "\u{1F914}",
    "\u{1F680}",
];
/// Longest label of an emoji that is not drawn as an image.
const MAX_LABEL_CHARS: usize = 32;

/// One bubble under a message.
#[derive(Clone, Debug, PartialEq)]
pub struct ReactionChip {
    pub emoji: p::Emoji,
    pub count: u64,
    /// The account reacted with it.
    pub me: bool,
}

impl ReactionChip {
    /// The bubbles of a raw message, in the server's order.
    pub fn of(m: &p::Message) -> Vec<ReactionChip> {
        m.reactions()
            .into_iter()
            .filter(|r| r.count > 0 || r.me)
            .map(|r| ReactionChip {
                emoji: r.emoji,
                count: r.count.max(u64::from(r.me)),
                me: r.me,
            })
            .collect()
    }
}

/// How an emoji is drawn: a known Unicode emoji as its Twemoji image, anything else as
/// short text (a custom emoji as `:name:` where it has no picture, see
/// [`Inlet::reaction_emoji`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmojiLabel {
    Glyph(String),
    Text(String),
}

pub fn label(emoji: &p::Emoji) -> EmojiLabel {
    if emoji.is_custom() {
        return EmojiLabel::Text(format!(
            ":{}:",
            cut(&model::clean_name(&emoji.name), MAX_LABEL_CHARS)
        ));
    }
    if inlet_emoji::by_glyph(&emoji.name).is_some() {
        EmojiLabel::Glyph(emoji.name.clone())
    } else {
        EmojiLabel::Text(cut(&model::clean_name(&emoji.name), 8))
    }
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('\u{2026}');
        out
    }
}

thread_local! {
    /// Parsed known emoji, so a bubble is not parsed again on every frame.
    static GLYPHS: RefCell<HashMap<String, Rc<Vec<Block>>>> = RefCell::default();
}

/// The parsed blocks of a known emoji (bounded: only known glyphs are ever parsed).
fn glyph_blocks(glyph: &str) -> Rc<Vec<Block>> {
    GLYPHS.with(|g| {
        let mut g = g.borrow_mut();
        if g.len() > 512 {
            g.clear();
        }
        g.entry(glyph.to_string())
            .or_insert_with(|| Rc::new(markdown::parse(glyph)))
            .clone()
    })
}

impl Inlet {
    /// An emoji of a reaction as an element, `size` the text size it is drawn at: a
    /// custom emoji as its picture (through the image client and the animation rules,
    /// like custom emoji in text), anything else as [`emoji_el`] draws it.
    pub(crate) fn reaction_emoji(
        &self,
        id: &str,
        emoji: &p::Emoji,
        size: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match emoji.id.as_deref() {
            Some(custom) if emoji.is_custom() => {
                // The size Twemoji get at this text size (1.375 em).
                self.custom_emoji(custom, emoji.animated, (size * 1.375).round(), cx)
            }
            _ => emoji_el(id, emoji, size),
        }
    }
}

/// An emoji as an element, `size` the text size it is drawn at.
pub fn emoji_el(id: &str, emoji: &p::Emoji, size: f32) -> AnyElement {
    match label(emoji) {
        EmojiLabel::Glyph(g) => {
            let ctx = RenderCtx {
                revealed: true,
                names: Rc::default(),
                jumbo: false,
                emoji: None,
                off_thread: None,
                on_link: Rc::new(|_, _, _, _| {}),
                on_reveal: Rc::new(|_, _| {}),
            };
            div()
                .flex_none()
                .text_size(px(size))
                .line_height(px(size * 1.5))
                .child(markdown::render(id, &glyph_blocks(&g), &ctx))
                .into_any_element()
        }
        EmojiLabel::Text(t) => div()
            .flex_none()
            .text_size(px(size))
            .text_color(theme::text_muted())
            .child(t)
            .into_any_element(),
    }
}

/// What a click did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Click {
    /// Over the limits: nothing changes.
    Ignored,
    /// The first click on the message in this window: flush after [`WINDOW`].
    Started,
    /// Joined a window that is already waiting.
    Joined,
}

/// A reaction request to send.
#[derive(Clone, Debug, PartialEq)]
pub struct ReactSend {
    pub channel_id: String,
    pub message_id: String,
    pub emoji: p::Emoji,
    pub add: bool,
}

#[derive(Debug)]
struct Toggle {
    emoji: p::Emoji,
    /// Whether the account had reacted when the window started (what the server has).
    before: bool,
    /// What the clicks ask for now.
    want: bool,
}

#[derive(Debug)]
struct Batch {
    channel_id: String,
    first: Instant,
    toggles: Vec<Toggle>,
}

/// Collects reaction clicks per message for [`WINDOW`] and gives the net change.
#[derive(Debug, Default)]
pub struct ReactionClicks {
    batches: HashMap<String, Batch>,
}

impl ReactionClicks {
    /// A click on `emoji` of a message on which the account's reaction is `me` now
    /// (with earlier clicks already shown).
    pub fn click(
        &mut self,
        channel_id: &str,
        message_id: &str,
        emoji: &p::Emoji,
        me: bool,
        now: Instant,
    ) -> Click {
        let started = !self.batches.contains_key(message_id);
        if started && self.batches.len() >= MAX_MESSAGES {
            return Click::Ignored;
        }
        let batch = self
            .batches
            .entry(message_id.to_string())
            .or_insert_with(|| Batch {
                channel_id: channel_id.to_string(),
                first: now,
                toggles: Vec::new(),
            });
        let full = batch.toggles.len() >= MAX_EMOJI;
        match batch
            .toggles
            .iter_mut()
            .find(|t| t.emoji.same_reaction(emoji))
        {
            Some(t) => t.want = !me,
            // MAX_EMOJI is above zero, so a full batch is never one just started.
            None if full => return Click::Ignored,
            None => batch.toggles.push(Toggle {
                emoji: emoji.clone(),
                before: me,
                want: !me,
            }),
        }
        if started {
            Click::Started
        } else {
            Click::Joined
        }
    }

    /// The requests of every window that is over: one per emoji whose state differs
    /// from where the window started, none for one clicked back.
    pub fn due(&mut self, now: Instant) -> Vec<ReactSend> {
        let over: Vec<String> = self
            .batches
            .iter()
            .filter(|(_, b)| now.saturating_duration_since(b.first) >= WINDOW)
            .map(|(id, _)| id.clone())
            .collect();
        let mut out = Vec::new();
        for id in over {
            let Some(batch) = self.batches.remove(&id) else {
                continue;
            };
            for t in batch.toggles {
                if t.want != t.before {
                    out.push(ReactSend {
                        channel_id: batch.channel_id.clone(),
                        message_id: id.clone(),
                        emoji: t.emoji,
                        add: t.want,
                    });
                }
            }
        }
        out
    }
}

/// The gateway event the account's own reaction (or its removal) would bring, for the
/// state: applied at once on a click, and the other way round when the server refuses.
pub fn own_reaction_event(
    add: bool,
    channel_id: &str,
    message_id: &str,
    me: UserId,
    emoji: &p::Emoji,
) -> p::Dispatch {
    let event = Box::new(p::models::ReactionEvent {
        user_id: me.to_string(),
        channel_id: channel_id.to_string(),
        message_id: message_id.to_string(),
        emoji: emoji.clone(),
        guild_id: None,
        member: None,
    });
    if add {
        p::Dispatch::MessageReactionAdd(event)
    } else {
        p::Dispatch::MessageReactionRemove(event)
    }
}

impl Inlet {
    /// A click on a reaction (a bubble, a quick reaction or the emoji list): the
    /// account's reaction flips at once; the request follows after [`WINDOW`].
    pub(crate) fn toggle_reaction(
        &mut self,
        ch: ChannelId,
        id: MessageId,
        emoji: p::Emoji,
        cx: &mut Context<Self>,
    ) {
        let Some(account) = self.account.clone() else {
            return;
        };
        let (me_id, mine, exists, facts) = {
            let state = account.read(cx);
            let Some(m) = state.messages(ch).and_then(|w| w.get(id)) else {
                return;
            };
            let found = m
                .reactions()
                .into_iter()
                .find(|r| r.emoji.same_reaction(&emoji));
            (
                state.me_id(),
                found.as_ref().is_some_and(|r| r.me),
                found.is_some(),
                super::action_bar::Perms::of(state, ch),
            )
        };
        // Taking back the account's own reaction is always allowed; adding needs the
        // rights (fail closed while they are unknown).
        if !mine && !facts.may_react(exists) {
            return;
        }
        let (channel_id, message_id) = (ch.to_string(), id.to_string());
        let click =
            self.chat
                .actions
                .clicks
                .click(&channel_id, &message_id, &emoji, mine, Instant::now());
        if click == Click::Ignored {
            return;
        }
        let event = own_reaction_event(!mine, &channel_id, &message_id, me_id, &emoji);
        let changes = account.update(cx, |s, _| s.apply(&event));
        self.after_changes(changes, false, cx);
        if click == Click::Started {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(WINDOW).await;
                let _ = this.update(cx, |this, _| this.flush_reactions());
            })
            .detach();
        }
        cx.notify();
    }

    /// Sends the net change of every reaction window that is over.
    /// Sends every reaction click still waiting in its window, at once: before the
    /// account changes, while the clicks still go out for the account they were made in.
    pub(crate) fn flush_all_reactions(&mut self) {
        let later = Instant::now() + WINDOW;
        for s in self.chat.actions.clicks.due(later) {
            self.backend.send(Cmd::Action(actions::Cmd::React {
                channel_id: s.channel_id,
                message_id: s.message_id,
                emoji: s.emoji,
                add: s.add,
            }));
        }
    }

    fn flush_reactions(&mut self) {
        for s in self.chat.actions.clicks.due(Instant::now()) {
            self.backend.send(Cmd::Action(actions::Cmd::React {
                channel_id: s.channel_id,
                message_id: s.message_id,
                emoji: s.emoji,
                add: s.add,
            }));
        }
    }

    /// The server refused a reaction: the early change is undone.
    pub(crate) fn reaction_failed(
        &mut self,
        channel_id: &str,
        message_id: &str,
        emoji: &p::Emoji,
        add: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(account) = self.account.clone() else {
            return;
        };
        let me = account.read(cx).me_id();
        let event = own_reaction_event(!add, channel_id, message_id, me, emoji);
        let changes = account.update(cx, |s, _| s.apply(&event));
        self.after_changes(changes, false, cx);
    }

    /// The bubbles under a message, or `None` when it has no reactions.
    pub(super) fn reactions_slot(
        &self,
        row: &MessageRow,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let chips = &row.meta.reactions;
        if chips.is_empty() {
            return None;
        }
        let ch = ChannelId::parse(&row.message.channel_id)?;
        let id = row.id;
        let mut list = div().flex().flex_row().flex_wrap().gap(px(4.)).pt(px(4.));
        for (i, chip) in chips.iter().enumerate() {
            let emoji = chip.emoji.clone();
            let for_list = chip.emoji.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("react-{id}-{i}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .h(px(28.))
                    .px(px(8.))
                    .rounded(px(8.))
                    .border_1()
                    .cursor_pointer()
                    .when(chip.me, |d| {
                        d.bg(theme::reaction_me_bg())
                            .border_color(theme::reaction_me_border())
                    })
                    .when(!chip.me, |d| {
                        d.bg(theme::reaction_bg())
                            .border_color(gpui::transparent_black())
                            .hover(|s| s.border_color(theme::accent_border()))
                    })
                    .child(self.reaction_emoji(&format!("react-e-{id}-{i}"), &chip.emoji, 11., cx))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if chip.me {
                                theme::reaction_me_text()
                            } else {
                                theme::text_muted()
                            })
                            .child(chip.count.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_reaction(ch, id, emoji.clone(), cx)
                    }))
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_reactors(ch, id, for_list.clone(), e.position, window, cx)
                        }),
                    ),
            );
        }
        Some(list.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::messages::{MessageView, Row};
    use inlet_state::{AccountState, MessagesPage};
    use serde_json::json;

    fn thumbs() -> p::Emoji {
        p::Emoji::unicode("\u{1F44D}")
    }

    /// Before an account switch every click still in its window goes out at once (the
    /// flush asks for what is due half a second later), so none is lost with the switch.
    #[test]
    fn clicks_in_their_window_are_all_due_before_a_switch() {
        let t0 = Instant::now();
        let mut clicks = ReactionClicks::default();
        clicks.click("1", "5", &thumbs(), false, t0);
        clicks.click("1", "6", &thumbs(), true, t0 + Duration::from_millis(400));
        assert!(clicks.due(t0 + Duration::from_millis(450)).is_empty());
        let sent = clicks.due(t0 + Duration::from_millis(450) + WINDOW);
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().any(|s| s.message_id == "5" && s.add));
        assert!(sent.iter().any(|s| s.message_id == "6" && !s.add));
    }

    #[test]
    fn reaction_click_spam_is_coalesced() {
        let t0 = Instant::now();
        let mut clicks = ReactionClicks::default();
        // Fifty-one clicks within the window: the reaction flips 51 times on screen
        // and one request goes out, for the net change.
        let mut me = false;
        for i in 0..51u64 {
            let at = t0 + Duration::from_millis(i * 8);
            let c = clicks.click("1", "5", &thumbs(), me, at);
            assert_eq!(c == Click::Started, i == 0);
            me = !me;
        }
        assert!(clicks.due(t0 + Duration::from_millis(499)).is_empty());
        let sent = clicks.due(t0 + WINDOW);
        assert_eq!(
            sent,
            vec![ReactSend {
                channel_id: "1".into(),
                message_id: "5".into(),
                emoji: thumbs(),
                add: true,
            }]
        );
        assert!(clicks.due(t0 + WINDOW * 4).is_empty(), "sent once");

        // Fifty clicks end where they started: nothing is sent.
        let t1 = t0 + Duration::from_secs(5);
        let mut me = true;
        for i in 0..50u64 {
            clicks.click("1", "5", &thumbs(), me, t1 + Duration::from_millis(i));
            me = !me;
        }
        assert!(clicks.due(t1 + WINDOW).is_empty());

        // Three emoji on one message: at most one request each, all after one window.
        let t2 = t1 + Duration::from_secs(5);
        let heart = p::Emoji::unicode("\u{2764}\u{FE0F}");
        let custom = p::Emoji::custom("77", "blob", false);
        assert_eq!(clicks.click("1", "6", &thumbs(), false, t2), Click::Started);
        assert_eq!(clicks.click("1", "6", &heart, true, t2), Click::Joined);
        assert_eq!(clicks.click("1", "6", &custom, false, t2), Click::Joined);
        assert_eq!(clicks.click("1", "6", &custom, true, t2), Click::Joined);
        let sent = clicks.due(t2 + WINDOW);
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().any(|s| s.emoji == thumbs() && s.add));
        assert!(sent.iter().any(|s| s.emoji == heart && !s.add));

        // Spam over many messages is capped: at most MAX_MESSAGES windows wait.
        let t3 = t2 + Duration::from_secs(5);
        let accepted = (0..200)
            .filter(|i| {
                clicks.click("1", &format!("{}", 1000 + i), &thumbs(), false, t3) != Click::Ignored
            })
            .count();
        assert_eq!(accepted, MAX_MESSAGES);
        assert_eq!(clicks.due(t3 + WINDOW).len(), MAX_MESSAGES);
        // And at most MAX_EMOJI emoji per message.
        let t4 = t3 + Duration::from_secs(5);
        let taken = (0..40)
            .filter(|i| {
                let e = p::Emoji::custom(format!("{}", 500 + i), "e", false);
                clicks.click("1", "9", &e, false, t4) != Click::Ignored
            })
            .count();
        assert_eq!(taken, MAX_EMOJI);
    }

    fn state() -> AccountState {
        let ready: p::Ready = serde_json::from_value(json!({
            "session_id": "s",
            "user": {"id": "10", "username": "me"},
            "private_channels": [{"id": "100", "type": 1, "recipients": [{"id": "7", "username": "ada"}]}],
        }))
        .unwrap();
        AccountState::from_ready_at(&ready, 1)
    }

    fn message(id: u64) -> p::Message {
        serde_json::from_value(json!({
            "id": id.to_string(),
            "channel_id": "100",
            "author": {"id": "7", "username": "ada"},
            "type": 0,
            "content": "hi",
            "timestamp": "2026-10-01T12:00:00Z"
        }))
        .unwrap()
    }

    fn chips(state: &AccountState, ch: ChannelId) -> Vec<ReactionChip> {
        MessageView::default()
            .build_rows(state, ch)
            .iter()
            .find_map(|r| match r {
                Row::Message(m) => Some(m.meta.reactions.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn reaction_add_many_event_is_applied() {
        let ch = ChannelId(100);
        let mut s = state();
        let _ = s.begin_load(ch);
        let _ = s.apply_messages(ch, MessagesPage::latest(vec![message(500)], 50));
        assert!(chips(&s, ch).is_empty());

        // DM reactions arrive grouped: two users and the account on one emoji, one
        // user on another.
        let many = p::Dispatch::parse(
            "MESSAGE_REACTION_ADD_MANY",
            json!({
                "channel_id": "100",
                "message_id": "500",
                "reactions": [
                    {"user_id": "7", "emoji": {"name": "\u{1F44D}"}},
                    {"user_id": "8", "emoji": {"name": "\u{1F44D}"}},
                    {"user_id": "10", "emoji": {"name": "\u{1F44D}"}},
                    {"user_id": "7", "emoji": {"id": "77", "name": "blob"}}
                ]
            }),
        );
        let changes = s.apply(&many);
        assert!(changes.messages.contains(&ch), "the row is redrawn");
        let got = chips(&s, ch);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].count, got[0].me), (3, true));
        assert_eq!((got[1].count, got[1].me), (1, false));
        assert_eq!(label(&got[1].emoji), EmojiLabel::Text(":blob:".into()));
        assert_eq!(label(&got[0].emoji), EmojiLabel::Glyph("\u{1F44D}".into()));

        // The account's own click, shown at once, then the gateway's echo: no double
        // count. Taking it back removes the account only.
        let me = s.me_id();
        let _ = s.apply(&own_reaction_event(true, "100", "500", me, &got[1].emoji));
        let _ = s.apply(&own_reaction_event(true, "100", "500", me, &got[1].emoji));
        assert_eq!((chips(&s, ch)[1].count, chips(&s, ch)[1].me), (2, true));
        let _ = s.apply(&own_reaction_event(false, "100", "500", me, &thumbs()));
        assert_eq!((chips(&s, ch)[0].count, chips(&s, ch)[0].me), (2, false));
    }

    #[test]
    fn odd_labels_are_short_text() {
        assert_eq!(
            label(&p::Emoji::unicode("**not an emoji at all**")),
            EmojiLabel::Text("**not an\u{2026}".into())
        );
        let long = p::Emoji::custom("1", "x".repeat(100), false);
        match label(&long) {
            EmojiLabel::Text(t) => assert!(t.chars().count() <= MAX_LABEL_CHARS + 3),
            other => panic!("{other:?}"),
        }
    }
}
