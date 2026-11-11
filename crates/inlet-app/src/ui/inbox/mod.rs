// SPDX-License-Identifier: AGPL-3.0-or-later
//! The panels over the chat and jumping to a message.
//!
//! - The pinned messages of the open channel (`ui/chat/pins.rs`), behind the pin in the
//!   channel header.
//! - The inbox, behind the tray in the channel header, as Fluxer's: unread channels
//!   (`unreads`), saved messages (`saved`) and recent mentions (`mentions`).
//! - Jumping to a message (from a panel): the channel opens, and when the message is not
//!   in its window the page around it is loaded and replaces the window, which then
//!   misses its newest messages ("detached") until the person goes back to the present.
//!   The message is highlighted for a moment (`ui::chat::messages::JUMP_HIGHLIGHT_MS`).
//!
//! A panel is a layer over the chat (`desktop::OpenLayers`): it takes the keyboard,
//! Escape closes it first, and nothing is read while it is open or while a jump is on
//! its way (`Inlet::chat_is_visible`). The lists load as described in
//! `backend/messages/inbox.rs`: one request per list at a time and at most one a second.

pub mod mentions;
pub mod saved;
pub mod unreads;

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, svg, AnyElement, Context, Div, FontWeight, Hsla, MouseDownEvent,
    SharedString, Stateful, Svg, Task, Window,
};
use inlet_protocol as p;
use inlet_state::{ChannelId, GuildId, MessageId};

use crate::app::Inlet;
use crate::backend::messages::inbox::{Cmd, Ev, Item};
use crate::backend::messages::Cmd as MessagesCmd;
use crate::desktop::Layer;
use crate::model::Message;
use crate::theme;
use crate::ui::chat::pins::Pins;
use crate::ui::markdown::{self, RenderCtx};
use crate::ui::HEADER_HEIGHT;

#[cfg(test)]
use crate::ui::chat::messages::JUMP_HIGHLIGHT_MS;

/// Shortest time between two requests for the same list.
pub const LIST_INTERVAL_MS: i64 = 1_000;
/// A jump that has not found its message by then is given up (the channel could not be
/// opened, or the person went elsewhere), so reading never stays held back.
pub const JUMP_TIMEOUT_MS: i64 = 10_000;
/// Mentions and saved messages older than this load again when their tab is shown.
pub const LIST_FRESH_MS: i64 = 60_000;
/// A click on a header button right after a click outside closed its panel does not
/// open it again (the click outside was on the button).
const REOPEN_GUARD_MS: i64 = 300;

/// The panels' sizes, as Fluxer's popouts open.
pub const PINS_WIDTH: f32 = 480.;
pub const PINS_HEIGHT: f32 = 640.;
pub const INBOX_WIDTH: f32 = 600.;
pub const INBOX_HEIGHT: f32 = 900.;
/// The inbox's tab column.
const TAB_COLUMN: f32 = 46.;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Icons of this domain (Phosphor, MIT).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxIcon {
    /// `push-pin` (fill).
    PushPin,
    /// `tray` (fill): the inbox.
    Tray,
    /// `at` (bold): mentions.
    At,
    /// `bookmark-simple` (fill): saved messages.
    Bookmark,
    /// `bell` (fill): unread channels.
    Bell,
    /// `check` (bold): mark as read.
    Check,
    /// `x` (bold): remove.
    Close,
}

impl InboxIcon {
    fn data(self) -> &'static [u8] {
        match self {
            InboxIcon::PushPin => include_bytes!("../../../assets/icons/push-pin.svg"),
            InboxIcon::Tray => include_bytes!("../../../assets/icons/tray.svg"),
            InboxIcon::At => include_bytes!("../../../assets/icons/at.svg"),
            InboxIcon::Bookmark => include_bytes!("../../../assets/icons/bookmark-simple.svg"),
            InboxIcon::Bell => include_bytes!("../../../assets/icons/bell.svg"),
            InboxIcon::Check => include_bytes!("../../../assets/icons/check.svg"),
            InboxIcon::Close => include_bytes!("../../../assets/icons/x.svg"),
        }
    }

    pub fn el(self, size: f32, color: Hsla) -> Svg {
        svg()
            .data(self.data())
            .size(px(size))
            .flex_none()
            .text_color(color)
    }
}

/// Which panel is open over the chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Pins,
    Inbox,
}

/// The inbox's tabs, in Fluxer's order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Unreads,
    Saved,
    Mentions,
}

/// A list loaded page by page: what it holds, and its one request on the way.
pub struct Paged<T> {
    pub items: Vec<T>,
    /// The server has more after the last item.
    pub more: bool,
    /// The first page landed.
    pub loaded: bool,
    /// The last load failed; shown with "Try again", never retried by itself.
    pub error: Option<String>,
    /// The list changed on the server since its first page was asked for.
    pub stale: bool,
    /// The request on its way, and whether it asks for the first page.
    in_flight: Option<(u64, bool)>,
    last_request_ms: Option<i64>,
    loaded_ms: i64,
    cap: usize,
}

impl<T> Paged<T> {
    /// An empty list that keeps at most `cap` items.
    pub fn new(cap: usize) -> Self {
        Paged {
            items: Vec::new(),
            more: false,
            loaded: false,
            error: None,
            stale: false,
            in_flight: None,
            last_request_ms: None,
            loaded_ms: 0,
            cap,
        }
    }

    pub fn busy(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Whether the first page is to be loaded: never loaded, or changed since, and no
    /// request on its way or failure waiting for "Try again".
    pub fn wants_first(&self) -> bool {
        !self.busy() && self.error.is_none() && (!self.loaded || self.stale)
    }

    /// Whether a request may go at `now`: none on its way and the last one at least
    /// [`LIST_INTERVAL_MS`] ago.
    pub fn may_start(&self, now: i64) -> bool {
        !self.busy() && self.wait_ms(now) == 0
    }

    /// How long until the next request may go.
    pub fn wait_ms(&self, now: i64) -> i64 {
        self.last_request_ms
            .map_or(0, |last| (last + LIST_INTERVAL_MS - now).max(0))
    }

    /// Request `request` went out, for the first page (`first`) or the next one.
    pub fn start(&mut self, request: u64, first: bool, now: i64) {
        self.in_flight = Some((request, first));
        self.last_request_ms = Some(now);
        if first {
            self.stale = false;
        }
    }

    /// The answer to `request`: whether it was the request on its way (an answer to a
    /// request the list no longer waits for is dropped).
    pub fn land(&mut self, request: u64, items: Vec<T>, more: bool, now: i64) -> bool {
        let Some((r, first)) = self.in_flight else {
            return false;
        };
        if r != request {
            return false;
        }
        self.in_flight = None;
        if first {
            self.items = items;
            self.loaded_ms = now;
        } else {
            self.items.extend(items);
        }
        self.more = more;
        self.loaded = true;
        self.error = None;
        if self.items.len() >= self.cap {
            self.items.truncate(self.cap);
            self.more = false;
        }
        true
    }

    /// Request `request` failed. Whether it was the one on its way. A failed first page
    /// is still wanted after "Try again".
    pub fn fail(&mut self, request: u64, reason: String) -> bool {
        let Some((r, first)) = self.in_flight else {
            return false;
        };
        if r != request {
            return false;
        }
        self.in_flight = None;
        self.stale |= first;
        self.error = Some(reason);
        true
    }

    /// Loaded more than `max_age` ago (or never).
    pub fn is_old(&self, now: i64, max_age: i64) -> bool {
        !self.loaded || now.saturating_sub(self.loaded_ms) > max_age
    }

    /// Forgets everything; an answer on its way is dropped when it comes.
    pub fn reset(&mut self) {
        *self = Paged::new(self.cap);
    }
}

/// A message to jump to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub channel: ChannelId,
    pub message: MessageId,
}

/// What a jump does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JumpStep {
    /// Nothing to do yet (the channel or its newest page is on its way).
    Wait,
    /// The message is in the list: scroll to it and highlight it.
    Show(MessageId),
    /// Load the page around the message.
    Fetch(Target),
    /// The jump was given up.
    GiveUp,
}

/// A jump to a message: waits for its channel to open and its newest page to land, then
/// shows the message, or loads the page around it first. One page request at a time; a
/// newer jump replaces an older one.
#[derive(Default)]
pub struct Jumper {
    /// The jump waiting for its channel, when it started and which channel was open then.
    target: Option<(Target, i64, Option<ChannelId>)>,
    /// The page around a message on its way.
    in_flight: Option<(u64, Target)>,
    /// Asked again once already, after a page came while the window was loading.
    retried: bool,
}

impl Jumper {
    pub fn start(&mut self, target: Target, open: Option<ChannelId>, now: i64) {
        self.target = Some((target, now, open));
        self.retried = false;
    }

    /// Whether a jump is under way: reading waits until it is done.
    pub fn is_active(&self, now: i64) -> bool {
        self.in_flight.is_some()
            || self
                .target
                .is_some_and(|(_, since, _)| now.saturating_sub(since) < JUMP_TIMEOUT_MS)
    }

    /// What to do now. `open` is the open channel; `ready` says its newest page landed
    /// and its window is not loading; `shown` whether a message is in the list.
    pub fn step(
        &mut self,
        open: Option<ChannelId>,
        ready: bool,
        shown: impl Fn(MessageId) -> bool,
        now: i64,
    ) -> JumpStep {
        let Some((t, since, from)) = self.target else {
            return JumpStep::Wait;
        };
        // Too slow, or the person opened yet another channel meanwhile.
        let elsewhere = open.is_some_and(|o| o != t.channel && Some(o) != from);
        if now.saturating_sub(since) >= JUMP_TIMEOUT_MS || elsewhere {
            self.target = None;
            return JumpStep::GiveUp;
        }
        if open != Some(t.channel) || !ready {
            return JumpStep::Wait;
        }
        if shown(t.message) {
            self.target = None;
            return JumpStep::Show(t.message);
        }
        if self.in_flight.is_some() {
            return JumpStep::Wait;
        }
        self.target = None;
        JumpStep::Fetch(t)
    }

    /// The page around `target` was asked for with `request`.
    pub fn fetching(&mut self, request: u64, target: Target) {
        self.in_flight = Some((request, target));
    }

    /// The page for `request` came (or failed): the jump it belongs to, if it is still
    /// wanted. A newer jump waiting makes it unwanted.
    pub fn answered(&mut self, request: u64) -> Option<Target> {
        let (r, t) = self.in_flight?;
        if r != request {
            return None;
        }
        self.in_flight = None;
        self.target.is_none().then_some(t)
    }

    /// The page came while the window loaded its newest page: try once more after it.
    pub fn again(&mut self, target: Target, now: i64) -> bool {
        if self.retried {
            return false;
        }
        self.retried = true;
        self.target = Some((target, now, Some(target.channel)));
        true
    }
}

/// This domain's view state.
#[derive(Default)]
pub struct State {
    pub open: Option<Panel>,
    pub tab: Tab,
    pub pins: Pins,
    pub mentions: mentions::Mentions,
    pub saved: saved::Saved,
    pub jump: Jumper,
    next_request: u64,
    /// The panel a click outside just closed, and when (see [`REOPEN_GUARD_MS`]).
    closed_outside: Option<(Panel, i64)>,
    /// Wakes the lists when a request had to wait for the interval.
    timer: Option<Task<()>>,
    /// Demo scenes: the scene's panel was opened.
    #[cfg(feature = "demo")]
    demo_done: bool,
}

impl State {
    fn request_id(&mut self) -> u64 {
        self.next_request += 1;
        self.next_request
    }

    /// Signed out: nothing of the account stays. Request numbers go on counting, so a
    /// late answer never fits a new request.
    pub fn clear(&mut self) {
        let next_request = self.next_request;
        *self = State {
            next_request,
            ..State::default()
        };
    }
}

/// A 24 px header button with the panel's icon; brighter while its panel is open.
fn header_icon_button(id: &'static str, icon: InboxIcon, active: bool, dot: bool) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .flex_none()
        .p(px(6.))
        .rounded(px(6.))
        .cursor_pointer()
        .when(active, |d| d.bg(theme::selected_bg()))
        .hover(|s| s.bg(theme::hover_bg()))
        .child(icon.el(
            24.,
            if active {
                theme::text_strong()
            } else {
                theme::text_muted()
            },
        ))
        .when(dot, |d| {
            d.child(
                div()
                    .absolute()
                    .bottom(px(5.))
                    .right(px(5.))
                    .size(px(8.))
                    .rounded_full()
                    .bg(theme::danger()),
            )
        })
}

/// The pinned messages button in the channel header (see `ui/chat/header_buttons.rs`).
/// A dot shows pins the account has not looked at.
pub fn pinned_header_button(this: &Inlet, cx: &mut Context<Inlet>) -> Option<AnyElement> {
    let ch = this.open_channel_id()?;
    let state = this.account.as_ref()?.read(cx);
    // Fail closed: no button for a channel the account cannot see.
    if !state.channel_permissions(ch).can_view() {
        return None;
    }
    let unseen = state.read_state(ch).is_some_and(|r| r.has_unread_pins());
    Some(
        header_icon_button(
            "header-pins",
            InboxIcon::PushPin,
            this.inbox.open == Some(Panel::Pins),
            unseen,
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_panel(Panel::Pins, cx)))
        .into_any_element(),
    )
}

/// The inbox button in the channel header (see `ui/chat/header_buttons.rs`).
pub fn header_button(this: &Inlet, cx: &mut Context<Inlet>) -> Option<AnyElement> {
    this.account.as_ref()?;
    Some(
        header_icon_button(
            "header-inbox",
            InboxIcon::Tray,
            this.inbox.open == Some(Panel::Inbox),
            false,
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_panel(Panel::Inbox, cx)))
        .into_any_element(),
    )
}

/// A small square button in a card header (Fluxer's `headerIconButton`, 28 px).
pub(crate) fn card_button(id: SharedString, icon: InboxIcon) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(28.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(theme::bg_app())
        .cursor_pointer()
        .hover(|s| s.bg(theme::hover_bg()))
        .child(icon.el(16., theme::text_muted()))
}

/// A text button in a panel ("Jump", "Load more", "Try again").
pub(crate) fn text_button(id: SharedString, label: String) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .px(px(8.))
        .py(px(3.))
        .rounded(px(6.))
        .bg(theme::bg_app())
        .cursor_pointer()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::text_secondary())
        .hover(|s| s.bg(theme::hover_bg()).text_color(theme::text_strong()))
        .child(label)
}

/// A line of muted text in an empty or loading list.
pub(crate) fn list_note(text: String) -> Div {
    div()
        .py(px(24.))
        .px(px(16.))
        .flex()
        .justify_center()
        .text_size(px(14.))
        .text_color(theme::text_muted())
        .child(text)
}

impl Inlet {
    /// Opens a panel, or closes it when it is open.
    pub(crate) fn toggle_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        let now = now_ms();
        let just_closed = self
            .inbox
            .closed_outside
            .take()
            .is_some_and(|(p, at)| p == panel && now.saturating_sub(at) < REOPEN_GUARD_MS);
        if self.inbox.open == Some(panel) || just_closed {
            self.inbox.open = None;
        } else {
            self.open_panel(panel, cx);
        }
        cx.notify();
    }

    fn open_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        match panel {
            Panel::Pins => {
                let Some(ch) = self.open_channel_id() else {
                    return;
                };
                self.inbox.pins.open(ch);
                // Looking at the pins marks them seen: once, when there are unseen ones.
                if let Some(account) = self.account.clone() {
                    let unseen = account
                        .read(cx)
                        .read_state(ch)
                        .is_some_and(|r| r.has_unread_pins());
                    if unseen {
                        let changes = account.update(cx, |s, _| s.ack_pins(ch));
                        self.backend.send(MessagesCmd::Inbox(Cmd::AckPins {
                            channel_id: ch.to_string(),
                        }));
                        self.after_changes(changes, false, cx);
                    }
                }
            }
            Panel::Inbox => self.inbox_tab_shown(self.inbox.tab),
        }
        self.inbox.open = Some(panel);
        self.pump_panels(cx);
    }

    /// A tab comes into view: a list that failed may try again, an old one reloads.
    fn inbox_tab_shown(&mut self, tab: Tab) {
        let now = now_ms();
        match tab {
            Tab::Mentions => {
                let list = &mut self.inbox.mentions.list;
                list.error = None;
                if list.loaded && list.is_old(now, LIST_FRESH_MS) {
                    list.stale = true;
                }
            }
            Tab::Saved => {
                let list = &mut self.inbox.saved.list;
                list.error = None;
                if list.loaded && list.is_old(now, LIST_FRESH_MS) {
                    list.stale = true;
                }
            }
            Tab::Unreads => {}
        }
    }

    pub(crate) fn select_inbox_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.inbox.tab = tab;
        self.inbox_tab_shown(tab);
        self.pump_panels(cx);
        cx.notify();
    }

    /// A click outside the open panel closes it.
    fn panel_clicked_outside(&mut self, panel: Panel, cx: &mut Context<Self>) {
        if self.inbox.open == Some(panel) {
            self.inbox.open = None;
            self.inbox.closed_outside = Some((panel, now_ms()));
            cx.notify();
        }
    }

    /// Sends the first page the open panel's list needs, within the budget. A list that
    /// has to wait for the interval is woken by a timer.
    pub(crate) fn pump_panels(&mut self, cx: &mut Context<Self>) {
        let now = now_ms();
        let wait = match self.inbox.open {
            Some(Panel::Pins) => self.pump_pins(now),
            Some(Panel::Inbox) => match self.inbox.tab {
                Tab::Mentions => self.pump_mentions(now, false),
                Tab::Saved => self.pump_saved(now, false),
                Tab::Unreads => None,
            },
            None => None,
        };
        if let Some(ms) = wait {
            if self.inbox.timer.is_none() {
                let ms = u64::try_from(ms.max(1)).unwrap_or(1);
                self.inbox.timer = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(ms))
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        this.inbox.timer = None;
                        this.pump_panels(cx);
                    });
                }));
            }
        }
    }

    /// The pins' first page when it is needed; `Some(wait)` when it has to wait.
    fn pump_pins(&mut self, now: i64) -> Option<i64> {
        let ch = self.inbox.pins.channel?;
        if !self.inbox.pins.list.wants_first() {
            return None;
        }
        if !self.inbox.pins.list.may_start(now) {
            return Some(self.inbox.pins.list.wait_ms(now));
        }
        let request = self.inbox.request_id();
        self.inbox.pins.list.start(request, true, now);
        self.backend.send(MessagesCmd::Inbox(Cmd::Pins {
            channel_id: ch.to_string(),
            before: None,
            request,
        }));
        None
    }

    /// The mentions' first page (or the next one, `more`) when it is needed.
    fn pump_mentions(&mut self, now: i64, more: bool) -> Option<i64> {
        let m = &mut self.inbox.mentions;
        let first = !more;
        if first && !m.list.wants_first() {
            return None;
        }
        if more && (!m.list.more || m.list.busy()) {
            return None;
        }
        if !m.list.may_start(now) {
            return Some(m.list.wait_ms(now));
        }
        let before = if more {
            m.list.items.last().map(|i| i.message.id.clone())
        } else {
            None
        };
        let (everyone, roles) = (m.everyone, m.roles);
        let request = self.inbox.request_id();
        self.inbox.mentions.list.start(request, first, now);
        self.backend.send(MessagesCmd::Inbox(Cmd::Mentions {
            before,
            everyone,
            roles,
            request,
        }));
        None
    }

    /// The saved messages' first page (or the next one, `more`) when it is needed.
    fn pump_saved(&mut self, now: i64, more: bool) -> Option<i64> {
        let list = &mut self.inbox.saved.list;
        let first = !more;
        if first && !list.wants_first() {
            return None;
        }
        if more && (!list.more || list.busy()) {
            return None;
        }
        if !list.may_start(now) {
            return Some(list.wait_ms(now));
        }
        let before = if more {
            list.items.last().map(|e| e.message_id().to_string())
        } else {
            None
        };
        let request = self.inbox.request_id();
        self.inbox.saved.list.start(request, first, now);
        self.backend
            .send(MessagesCmd::Inbox(Cmd::Saved { before, request }));
        None
    }

    /// "Load more" at the end of a list (ignored while a request is on its way or the
    /// last one was under a second ago).
    pub(crate) fn load_more(&mut self, cx: &mut Context<Self>) {
        let now = now_ms();
        match (self.inbox.open, self.inbox.tab) {
            (Some(Panel::Pins), _) => {
                let pins = &mut self.inbox.pins;
                let (Some(ch), Some(before)) = (pins.channel, pins.next.clone()) else {
                    return;
                };
                if !pins.list.more || !pins.list.may_start(now) {
                    return;
                }
                let request = self.inbox.request_id();
                self.inbox.pins.list.start(request, false, now);
                self.backend.send(MessagesCmd::Inbox(Cmd::Pins {
                    channel_id: ch.to_string(),
                    before: Some(before),
                    request,
                }));
            }
            (Some(Panel::Inbox), Tab::Mentions) => {
                self.pump_mentions(now, true);
            }
            (Some(Panel::Inbox), Tab::Saved) => {
                self.pump_saved(now, true);
            }
            _ => {}
        }
        cx.notify();
    }

    /// "Try again" after a failed load.
    pub(crate) fn retry_list(&mut self, cx: &mut Context<Self>) {
        match (self.inbox.open, self.inbox.tab) {
            (Some(Panel::Pins), _) => self.inbox.pins.list.error = None,
            (Some(Panel::Inbox), Tab::Mentions) => self.inbox.mentions.list.error = None,
            (Some(Panel::Inbox), Tab::Saved) => self.inbox.saved.list.error = None,
            _ => {}
        }
        self.pump_panels(cx);
        cx.notify();
    }

    /// Answers for the panels and jumps.
    pub(crate) fn on_inbox_event(&mut self, ev: Ev, cx: &mut Context<Self>) {
        let now = now_ms();
        match ev {
            Ev::Pins {
                channel_id,
                request,
                items,
                next,
            } => {
                let pins = &mut self.inbox.pins;
                if pins.channel.map(|c| c.to_string()).as_deref() == Some(channel_id.as_str())
                    && pins.list.land(request, items, next.is_some(), now)
                {
                    pins.next = next;
                }
            }
            Ev::Mentions {
                request,
                items,
                more,
            } => {
                self.inbox.mentions.landed(request, items, more, now);
            }
            Ev::Saved {
                request,
                entries,
                more,
            } => {
                if self.inbox.saved.list.land(request, entries, more, now) {
                    // What the message actions know as saved (their "Save"/"Remove").
                    for e in &self.inbox.saved.list.items {
                        if let Some(id) = MessageId::parse(e.message_id()) {
                            self.chat.actions.set_saved(id, true);
                        }
                    }
                }
            }
            Ev::Around {
                channel_id,
                request,
                messages,
                rendered,
            } => self.on_around(&channel_id, request, messages, rendered, cx),
            Ev::Failed { request, reason } => {
                let reason = reason.unwrap_or_else(|| t!("inbox-load-failed"));
                let jumped = self.inbox.jump.answered(request).is_some();
                if jumped {
                    self.chat.error = Some(t!("inbox-jump-failed", reason = reason.clone()));
                }
                let lists = [
                    self.inbox.pins.list.fail(request, reason.clone()),
                    self.inbox.mentions.list.fail(request, reason.clone()),
                    self.inbox.saved.list.fail(request, reason),
                ];
                if !jumped && !lists.contains(&true) {
                    return;
                }
            }
            Ev::MentionDeleteFailed { message_id, reason } => {
                if self.inbox.mentions.delete_failed(&message_id) {
                    self.chat.error = Some(t!("inbox-remove-mention-failed", reason = reason));
                }
            }
        }
        // A list that changed on the server meanwhile loads again.
        self.pump_panels(cx);
        cx.notify();
    }

    /// Gateway events that change what the panels show, after the account state took
    /// them (`rendered`: the messages the event carries, converted).
    pub(crate) fn inbox_on_dispatch(
        &mut self,
        d: &p::Dispatch,
        rendered: &[Message],
        cx: &mut Context<Self>,
    ) {
        if let p::Dispatch::Ready(_) = d {
            // A new session: what the lists hold may have changed meanwhile.
            let inbox = &mut self.inbox;
            inbox.pins.list.stale |= inbox.pins.list.loaded;
            inbox.mentions.list.stale |= inbox.mentions.list.loaded;
            inbox.saved.list.stale |= inbox.saved.list.loaded;
            self.pump_panels(cx);
            return;
        }
        let mut changed = self.inbox.pins.on_dispatch(d, rendered);
        if let Some(account) = &self.account {
            changed |= self
                .inbox
                .mentions
                .on_dispatch(d, account.read(cx), rendered);
        }
        changed |= self.inbox.saved.on_dispatch(d, rendered);
        if changed {
            self.pump_panels(cx);
            cx.notify();
        }
    }

    /// The open channel's newest page landed: a jump waiting for it goes on.
    pub(crate) fn inbox_after_newest_page(&mut self, cx: &mut Context<Self>) {
        if self.inbox.jump.is_active(now_ms()) {
            self.step_jump(cx);
        }
    }

    /// Jumps to a message: closes the panel, opens the message's channel if needed, and
    /// shows the message (loading the page around it when it is not in the list). A
    /// channel the account cannot see is not opened.
    pub(crate) fn jump_to(
        &mut self,
        channel_id: &str,
        message_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(ch), Some(id), Some(account)) = (
            ChannelId::parse(channel_id),
            MessageId::parse(message_id),
            self.account.clone(),
        ) else {
            return;
        };
        let guild = {
            let state = account.read(cx);
            if !state.channel_permissions(ch).can_view() {
                self.chat.notice = Some(t!("inbox-jump-no-access"));
                cx.notify();
                return;
            }
            state.channels().guild_of(ch).map(|g| g.to_string())
        };
        self.inbox.open = None;
        let open = self.open_channel_id();
        self.inbox.jump.start(
            Target {
                channel: ch,
                message: id,
            },
            open,
            now_ms(),
        );
        if open != Some(ch) {
            self.follow_channel_link(guild, ch.to_string(), window, cx);
        }
        // After this click is handled, never inside the message list's own events.
        let weak = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = weak.update(cx, |this, cx| this.step_jump(cx));
        });
        cx.notify();
    }

    /// Opens a channel from a panel (the unread list), without a message to jump to.
    pub(crate) fn open_from_panel(
        &mut self,
        guild: Option<GuildId>,
        ch: ChannelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.inbox.open = None;
        if self.open_channel_id() != Some(ch) {
            self.follow_channel_link(guild.map(|g| g.to_string()), ch.to_string(), window, cx);
        }
        cx.notify();
    }

    fn step_jump(&mut self, cx: &mut Context<Self>) {
        let open = self.open_channel_id();
        let ready = match (open, &self.account) {
            (Some(ch), Some(account)) => {
                self.chat.view.is_loaded()
                    && account
                        .read(cx)
                        .messages(ch)
                        .is_some_and(|w| !w.is_loading())
            }
            _ => false,
        };
        let rows = &self.chat.view.rows;
        let step = self.inbox.jump.step(
            open,
            ready,
            |id| rows.iter().any(|r| r.message_id() == Some(id)),
            now_ms(),
        );
        match step {
            JumpStep::Wait => {}
            JumpStep::GiveUp => cx.notify(),
            JumpStep::Show(id) => self.jump_to_message(id, cx),
            JumpStep::Fetch(t) => {
                let request = self.inbox.request_id();
                self.inbox.jump.fetching(request, t);
                self.backend.send(MessagesCmd::Inbox(Cmd::Around {
                    channel_id: t.channel.to_string(),
                    message_id: t.message.to_string(),
                    request,
                }));
            }
        }
    }

    /// The page around a message came: it replaces the channel's window (detached
    /// unless it reaches the newest message) and the message is shown.
    fn on_around(
        &mut self,
        channel_id: &str,
        request: u64,
        messages: Vec<p::Message>,
        rendered: Vec<Message>,
        cx: &mut Context<Self>,
    ) {
        let Some(t) = self.inbox.jump.answered(request) else {
            // A newer jump waits: it goes on now.
            self.step_jump(cx);
            return;
        };
        let (Some(account), Some(open)) = (self.account.clone(), self.open_channel_id()) else {
            return;
        };
        if open != t.channel || ChannelId::parse(channel_id) != Some(t.channel) {
            return;
        }
        let loading = account
            .read(cx)
            .messages(t.channel)
            .is_some_and(|w| w.is_loading());
        if loading {
            // The newest page on its way would replace the jump: once more after it.
            self.inbox.jump.again(t, now_ms());
            return;
        }
        let changes = account.update(cx, |s, _| s.apply_around(t.channel, &messages));
        self.chat
            .view
            .remember_page(account.read(cx), &messages, rendered);
        self.after_changes(changes, false, cx);
        if self
            .chat
            .view
            .rows
            .iter()
            .any(|r| r.message_id() == Some(t.message))
        {
            self.jump_to_message(t.message, cx);
        } else {
            self.chat.notice = Some(t!("inbox-jump-missing"));
        }
        cx.notify();
    }

    /// Mention names and callbacks for drawing a message in a panel.
    fn panel_ctx(&self, m: &Message, cx: &mut Context<Self>) -> RenderCtx {
        let weak = cx.entity().downgrade();
        RenderCtx {
            revealed: false,
            names: self.mention_names(m, cx),
            jumbo: false,
            off_thread: Some(self.markdown_off_thread(cx)),
            on_link: Rc::new(move |url, masked, _window, cx| {
                let url = url.to_string();
                let _ = weak.update(cx, |this, cx| this.open_link(&url, masked, cx));
            }),
            // Spoilers stay hidden in a panel; the message shows them after a jump.
            on_reveal: Rc::new(|_window, _cx| {}),
        }
    }

    /// A message as a panel shows it (Fluxer's preview card): avatar, name, time, the
    /// text and the attachments, in a framed card.
    pub(crate) fn message_card(
        &mut self,
        key: &str,
        item: &Item,
        header: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let m = &item.message;
        let ctx = self.panel_ctx(m, cx);
        let avatar = self.user_avatar(m.author_avatar.as_deref(), &m.author, 40., false, cx);
        let mut body = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .line_height(px(22.))
                            .text_color(theme::text_strong())
                            .child(m.author.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::timestamp())
                            .child(m.timestamp.clone()),
                    ),
            )
            .child(markdown::render(
                &format!("{key}-m{}", m.id),
                &m.blocks,
                &ctx,
            ));
        if m.truncated {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(t!("inbox-truncated")),
            );
        }
        if m.blocks.is_empty() && m.attachments.is_empty() {
            body = body.child(
                div()
                    .text_size(px(14.))
                    .text_color(theme::text_muted())
                    .child(t!("message-no-text")),
            );
        }
        let body = body.children(self.render_attachments(&m.channel_id, &m.id, &m.attachments, cx));
        div()
            .w_full()
            .flex()
            .flex_col()
            .mb(px(16.))
            .children(header)
            .child(
                div()
                    .w_full()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme::preview_card_border())
                    .bg(theme::preview_card_bg())
                    .py(px(8.))
                    .px(px(16.))
                    .flex()
                    .flex_row()
                    .gap_3()
                    .text_color(theme::text())
                    .line_height(px(22.))
                    .child(div().w(px(40.)).flex_none().child(avatar))
                    .child(body),
            )
            .into_any_element()
    }

    /// The header over a card in the inbox (Fluxer's `InboxMessageHeader`): the channel
    /// and its server, which jumps to the message, and buttons on the right.
    pub(crate) fn card_header(
        &self,
        key: &str,
        item: &Item,
        buttons: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (channel, guild) = self.channel_and_guild_names(&item.message.channel_id, cx);
        let (channel_id, message_id) = (item.message.channel_id.clone(), item.message.id.clone());
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .py(px(10.))
            .border_b_1()
            .border_color(theme::preview_card_divider())
            .child(
                div()
                    .id(SharedString::from(format!("{key}-jump-{message_id}")))
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .cursor_pointer()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_strong())
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(channel),
                    )
                    .children(guild.map(|g| {
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(g)
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.jump_to(&channel_id, &message_id, window, cx)
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .children(buttons),
            )
            .into_any_element()
    }

    /// "#channel" (or "@name" for a direct message) and the server's name.
    pub(crate) fn channel_and_guild_names(
        &self,
        channel_id: &str,
        cx: &mut Context<Self>,
    ) -> (String, Option<String>) {
        let Some(state) = self.account.as_ref().map(|a| a.read(cx)) else {
            return (String::new(), None);
        };
        let Some(ch) = ChannelId::parse(channel_id) else {
            return (String::new(), None);
        };
        let channel = state
            .channel(ch)
            .map(|c| {
                crate::ui::sidebar::channel_list::channel_label(
                    &crate::backend::messages::parse::to_channel(c),
                )
            })
            .unwrap_or_else(|| t!("inbox-unknown-channel"));
        let guild = state.channels().guild_of(ch).map(|g| {
            let id = g.to_string();
            self.guilds
                .iter()
                .find(|x| x.id == id)
                .map(|x| x.name.clone())
                .unwrap_or_else(|| t!("inbox-unknown-server"))
        });
        (channel, guild)
    }

    /// The open panel, drawn under the channel header at the right, as Fluxer's popouts.
    pub(crate) fn render_panels(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        #[cfg(feature = "demo")]
        self.demo_panels(cx);
        let panel = self.inbox.open?;
        // The pins belong to the channel they were opened for.
        if panel == Panel::Pins && self.inbox.pins.channel != self.open_channel_id() {
            self.inbox.open = None;
            return None;
        }
        let (width, height) = match panel {
            Panel::Pins => (PINS_WIDTH, PINS_HEIGHT),
            Panel::Inbox => (INBOX_WIDTH, INBOX_HEIGHT),
        };
        let top = HEADER_HEIGHT - 4.;
        let viewport_h = self.media.viewport.height.as_f32();
        let height = if viewport_h > 0. {
            height.min((viewport_h - top - 8.).max(200.))
        } else {
            height
        };
        let content = match panel {
            Panel::Pins => self.render_pins_panel(cx),
            Panel::Inbox => self.render_inbox_panel(cx),
        };
        let frame = div()
            .id("panel")
            .occlude()
            .absolute()
            .top(px(top))
            .right(px(16.))
            .w(px(width))
            .h(px(height))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(8.))
            .border_1()
            .border_color(theme::accent_border())
            .bg(theme::floating_bg())
            .shadow(vec![gpui::BoxShadow {
                color: theme::floating_shadow(),
                offset: gpui::point(px(0.), px(8.)),
                blur_radius: px(24.),
                spread_radius: px(0.),
                inset: false,
            }])
            .on_mouse_down_out(cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                this.panel_clicked_outside(panel, cx)
            }))
            .child(content);
        Some(if self.is_top_layer(Layer::Panel) {
            self.layer_keys(frame, cx).into_any_element()
        } else {
            frame.into_any_element()
        })
    }

    /// Demo scenes open their panel once the channel's messages are on screen, so the
    /// screenshots show it without a click.
    #[cfg(feature = "demo")]
    fn demo_panels(&mut self, cx: &mut Context<Self>) {
        use crate::demo::scenes::{current, Scene};
        if self.inbox.demo_done {
            return;
        }
        let (panel, tab) = match current() {
            Scene::ChatPins => (Panel::Pins, None),
            Scene::InboxUnreads => (Panel::Inbox, Some(Tab::Unreads)),
            Scene::InboxSaved => (Panel::Inbox, Some(Tab::Saved)),
            Scene::InboxMentions => (Panel::Inbox, Some(Tab::Mentions)),
            _ => {
                self.inbox.demo_done = true;
                return;
            }
        };
        if self.open_channel_id().is_none() || !self.chat.view.is_loaded() {
            return;
        }
        self.inbox.demo_done = true;
        if let Some(tab) = tab {
            self.inbox.tab = tab;
        }
        self.open_panel(panel, cx);
    }

    /// The inbox: the tab column on the left, the open tab on the right.
    fn render_inbox_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let tab = self.inbox.tab;
        let tab_button = |id: &'static str, t: Tab, icon: InboxIcon, cx: &mut Context<Self>| {
            let active = tab == t;
            div()
                .id(id)
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .when(active, |d| d.bg(theme::selected_bg()))
                .when(!active, |d| d.hover(|s| s.bg(theme::hover_bg())))
                .child(icon.el(
                    16.,
                    if active {
                        theme::text_strong()
                    } else {
                        theme::text_muted()
                    },
                ))
                .on_click(cx.listener(move |this, _, _, cx| this.select_inbox_tab(t, cx)))
        };
        let tabs = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.))
            .child(tab_button(
                "inbox-tab-unreads",
                Tab::Unreads,
                InboxIcon::Bell,
                cx,
            ))
            .child(tab_button(
                "inbox-tab-saved",
                Tab::Saved,
                InboxIcon::Bookmark,
                cx,
            ))
            .child(tab_button(
                "inbox-tab-mentions",
                Tab::Mentions,
                InboxIcon::At,
                cx,
            ));
        let footer = (tab == Tab::Unreads).then(|| {
            div()
                .id("inbox-mark-all")
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_bg()))
                .child(InboxIcon::Check.el(16., theme::text_muted()))
                .on_click(cx.listener(|this, _, _, cx| this.mark_inbox_unreads_read(cx)))
        });
        let column = div()
            .w(px(TAB_COLUMN))
            .flex_none()
            .h_full()
            .py(px(8.))
            .flex()
            .flex_col()
            .items_center()
            .justify_between()
            .border_r_1()
            .border_color(theme::accent_border())
            .child(tabs)
            .children(footer);
        let main = match tab {
            Tab::Unreads => self.render_unreads(cx),
            Tab::Saved => self.render_saved(cx),
            Tab::Mentions => self.render_mentions(cx),
        };
        div()
            .size_full()
            .flex()
            .flex_row()
            .child(column)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(main),
            )
            .into_any_element()
    }
}

/// The title row of a panel or tab: an icon, the title, and controls on the right.
pub(crate) fn panel_title(icon: InboxIcon, title: String, right: Vec<AnyElement>) -> Div {
    div()
        .flex_none()
        .min_h(px(52.))
        .px(px(16.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .child(icon.el(20., theme::text_muted()))
        .child(
            div()
                .flex_1()
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(16.))
                .text_color(theme::text_strong())
                .child(title),
        )
        .children(right)
}

/// The scrolling body of a panel.
pub(crate) fn panel_body(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .px(px(12.))
        .pb(px(12.))
        .flex()
        .flex_col()
}

/// What a list shows at its end: loading, the error with "Try again", or "Load more".
pub(crate) fn list_footer<T>(
    key: &str,
    list: &Paged<T>,
    empty: String,
    cx: &mut Context<Inlet>,
) -> Option<AnyElement> {
    if let Some(error) = &list.error {
        return Some(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.))
                .child(list_note(error.clone()))
                .child(
                    text_button(
                        SharedString::from(format!("{key}-retry")),
                        t!("inbox-retry"),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.retry_list(cx))),
                )
                .into_any_element(),
        );
    }
    if !list.loaded || (list.busy() && list.items.is_empty()) {
        return Some(list_note(t!("inbox-loading")).into_any_element());
    }
    if list.items.is_empty() {
        return Some(list_note(empty).into_any_element());
    }
    if list.more {
        let label = if list.busy() {
            t!("inbox-loading")
        } else {
            t!("inbox-load-more")
        };
        return Some(
            div()
                .flex()
                .justify_center()
                .py(px(8.))
                .child(
                    text_button(SharedString::from(format!("{key}-more")), label)
                        .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                )
                .into_any_element(),
        );
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::backend::messages::parse::SharedNames;
    use serde_json::json;

    /// A message for the panels' lists.
    pub(crate) fn raw(id: u64, channel: &str, extra: serde_json::Value) -> p::Message {
        let mut v = json!({
            "id": id.to_string(),
            "channel_id": channel,
            "author": {"id": "20", "username": "ada"},
            "content": format!("message {id}"),
            "timestamp": "2026-01-01T10:00:00Z",
        });
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        serde_json::from_value(v).expect("a valid message")
    }

    pub(crate) fn item(id: u64, channel: &str) -> Item {
        crate::backend::messages::inbox::item(&raw(id, channel, json!({})), &SharedNames::default())
    }

    #[test]
    fn a_list_asks_once_at_a_time_and_once_a_second() {
        let mut list: Paged<Item> = Paged::new(3);
        assert!(list.wants_first() && list.may_start(0));
        list.start(1, true, 0);
        assert!(
            !list.wants_first() && !list.may_start(5_000),
            "one on its way"
        );
        assert!(
            !list.land(2, Vec::new(), false, 10),
            "another request's answer"
        );
        assert!(list.land(1, vec![item(1, "5"), item(2, "5")], true, 10));
        assert!(!list.wants_first());
        assert!(!list.may_start(999) && list.may_start(1_000));
        assert_eq!(list.wait_ms(400), 600);
        // The next page is added, and the list stops at its cap.
        list.start(3, false, 1_000);
        assert!(list.land(3, vec![item(3, "5"), item(4, "5")], true, 1_010));
        assert_eq!(list.items.len(), 3);
        assert!(!list.more);
        // A failure waits for "Try again"; nothing asks again by itself.
        list.stale = true;
        list.start(4, true, 2_000);
        assert!(list.fail(4, "offline".into()));
        assert!(!list.wants_first());
        list.error = None;
        assert!(list.wants_first());
    }

    fn ids(state: &inlet_state::AccountState, ch: ChannelId) -> Vec<MessageId> {
        state
            .messages(ch)
            .map(|w| w.iter().filter_map(|m| MessageId::parse(&m.id)).collect())
            .unwrap_or_default()
    }

    #[test]
    fn jump_loads_around_and_highlights() {
        use crate::ui::chat::messages::MessageView;
        use inlet_state::{AccountState, MessagesPage};

        let general = "301";
        let ch = ChannelId(301);
        let ready: p::Ready = serde_json::from_value(json!({
            "session_id": "s",
            "user": {"id": "10", "username": "me"},
            "guilds": [],
            "private_channels": [{"id": general, "type": 1, "recipients": [{"id": "20", "username": "ada"}]}],
            "users": [],
            "read_states": [],
        }))
        .expect("ready");
        let mut state = AccountState::from_ready_at(&ready, 1);
        // The newest page of the open channel: messages 1 000 … 1 049.
        let newest: Vec<p::Message> = (1_000..1_050)
            .rev()
            .map(|i| raw(i, general, json!({})))
            .collect();
        state.begin_load(ch);
        state.apply_messages(ch, MessagesPage::latest(newest, 50));
        let mut view = MessageView::loaded_for_tests();
        let rows = view.build_rows(&state, ch);
        let shown = |rows: &[crate::ui::chat::messages::Row], id: MessageId| {
            rows.iter().any(|r| r.message_id() == Some(id))
        };

        // A jump to message 10, far back: the channel is open and its newest page is in,
        // the message is not, so the page around it is asked for, once.
        let target = Target {
            channel: ch,
            message: MessageId(10),
        };
        let mut jump = Jumper::default();
        jump.start(target, Some(ch), 0);
        assert!(jump.is_active(0));
        let step = jump.step(Some(ch), true, |id| shown(&rows, id), 10);
        assert_eq!(step, JumpStep::Fetch(target));
        jump.fetching(1, target);
        assert_eq!(
            jump.step(Some(ch), true, |id| shown(&rows, id), 20),
            JumpStep::Wait
        );
        assert!(
            jump.is_active(20),
            "reading waits while the page is on its way"
        );

        // The page around it replaces the window, which now misses its newest messages:
        // the channel is not read while it is detached.
        assert_eq!(jump.answered(1), Some(target));
        let around: Vec<p::Message> = (0..20).rev().map(|i| raw(i, general, json!({}))).collect();
        state.apply_around(ch, &around);
        assert_eq!(ids(&state, ch).first(), Some(&MessageId(0)));
        assert!(!state.messages(ch).unwrap().has_newest());
        assert!(!view.may_mark_read(&state, ch, true, true));

        // The message is highlighted for 320 ms, then no more.
        let now = now_ms();
        view.highlight(MessageId(10), now);
        let rows = view.build_rows(&state, ch);
        let jumped = |rows: &[crate::ui::chat::messages::Row]| {
            rows.iter()
                .filter_map(|r| match r {
                    crate::ui::chat::messages::Row::Message(m) if m.jumped => Some(m.id),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(jumped(&rows), vec![MessageId(10)]);
        assert_eq!(JUMP_HIGHLIGHT_MS, 320);
        view.highlight(MessageId(10), now - JUMP_HIGHLIGHT_MS);
        assert!(jumped(&view.build_rows(&state, ch)).is_empty());

        // A message already in the list is shown without a request.
        let rows = view.build_rows(&state, ch);
        jump.start(
            Target {
                channel: ch,
                message: MessageId(5),
            },
            Some(ch),
            100,
        );
        assert_eq!(
            jump.step(Some(ch), true, |id| shown(&rows, id), 110),
            JumpStep::Show(MessageId(5))
        );
        assert!(!jump.is_active(110));
    }

    #[test]
    fn a_jump_waits_for_its_channel_and_gives_up_in_time() {
        let (a, b, c) = (ChannelId(1), ChannelId(2), ChannelId(3));
        let t = Target {
            channel: b,
            message: MessageId(9),
        };
        let mut jump = Jumper::default();
        // From channel a: while b opens, a (and nothing) is still open.
        jump.start(t, Some(a), 0);
        assert_eq!(jump.step(Some(a), true, |_| false, 1), JumpStep::Wait);
        assert_eq!(jump.step(None, false, |_| false, 2), JumpStep::Wait);
        // b is open but its newest page is not in yet.
        assert_eq!(jump.step(Some(b), false, |_| false, 3), JumpStep::Wait);
        assert_eq!(jump.step(Some(b), true, |_| false, 4), JumpStep::Fetch(t));
        // The person opened c meanwhile: the jump goes.
        jump.start(t, Some(a), 10);
        assert_eq!(jump.step(Some(c), true, |_| false, 11), JumpStep::GiveUp);
        assert!(!jump.is_active(11));
        // A jump that never finds its channel ends, so reading is not held back.
        jump.start(t, Some(a), 20);
        assert!(jump.is_active(20 + JUMP_TIMEOUT_MS - 1));
        assert!(!jump.is_active(20 + JUMP_TIMEOUT_MS));
        assert_eq!(
            jump.step(Some(a), true, |_| false, 20 + JUMP_TIMEOUT_MS),
            JumpStep::GiveUp
        );
        // A newer jump makes the older page unwanted, and a page that came while the
        // window loaded is asked for once more only.
        jump.start(t, Some(b), 30);
        assert_eq!(jump.step(Some(b), true, |_| false, 31), JumpStep::Fetch(t));
        jump.fetching(5, t);
        let newer = Target {
            channel: b,
            message: MessageId(7),
        };
        jump.start(newer, Some(b), 32);
        assert_eq!(jump.answered(5), None);
        assert_eq!(
            jump.step(Some(b), true, |_| false, 33),
            JumpStep::Fetch(newer)
        );
        jump.fetching(6, newer);
        assert_eq!(jump.answered(6), Some(newer));
        assert!(jump.again(newer, 40));
        assert_eq!(
            jump.step(Some(b), true, |_| false, 41),
            JumpStep::Fetch(newer)
        );
        jump.fetching(7, newer);
        assert_eq!(jump.answered(7), Some(newer));
        assert!(!jump.again(newer, 50));
    }
}
