// SPDX-License-Identifier: AGPL-3.0-or-later
//! The message actions' overlays: the context menu (right click on a message, or "more"
//! in the bar), who reacted, the delete confirmation and the forward picker. One is open
//! at a time. "Add reaction" opens the emoji picker (`ui/composer/pickers`), a layer.
//!
//! An open overlay takes the keyboard focus, so Escape and Shift+Escape close it before
//! they can reach the app's "mark as read" (the composer, when it has focus, cancels
//! its own reply or edit first). The focus is taken back after a layer over it (the
//! link prompt) closes, and the app's own Escape handling also closes an open overlay
//! instead of marking anything read. Closing gives the focus back to where it was, or to
//! the composer. A click outside a menu closes it. Enter confirms a delete.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use gpui::{
    actions, anchored, deferred, div, point, prelude::*, px, AnyElement, App, ClickEvent, Context,
    FocusHandle, Focusable, FontWeight, KeyBinding, MouseDownEvent, Pixels, Point, SharedString,
    Window,
};
use inlet_protocol as p;
use inlet_protocol::models::channel_type;
use inlet_state::{ChannelId, MessageId};
use serde_json::json;

use super::action_bar::{group_of, menu_actions, Action, ActionIcon};
use super::messages::mentions_me;
use super::reactions::{self, ReactionClicks};
use super::reactors::{self, Reactors};
use crate::app::Inlet;
use crate::backend::messages::actions::{self, Ev, Kind};
use crate::backend::messages::Cmd;
use crate::model;
use crate::theme;
use crate::ui::overlays::modal::modal;
use crate::ui::widgets::button;

actions!(
    inlet_message_actions,
    [CloseMessageOverlay, ConfirmMessageOverlay]
);

/// The key context of the open overlay.
pub const KEY_CONTEXT: &str = "inlet_message_overlay";
/// Saved messages remembered per session (see [`ActionsState::is_saved`]).
const MAX_SAVED: usize = 500;
/// How long a second click on the same action of the same message is dropped when no
/// answer came.
const IN_FLIGHT_FOR: Duration = Duration::from_secs(30);
/// Destinations the forward picker lists.
const MAX_DESTINATIONS: usize = 40;
/// Of those, at most this many channels of the open server, so direct messages always
/// get a place.
const MAX_GUILD_DESTINATIONS: usize = 30;
/// Width of the context menu.
const MENU_WIDTH: f32 = 220.;

/// Escape closes the open overlay, Enter confirms a delete. Call it after the app-wide
/// bindings, so these come first while an overlay has the focus.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseMessageOverlay, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-escape", CloseMessageOverlay, Some(KEY_CONTEXT)),
        KeyBinding::new("enter", ConfirmMessageOverlay, Some(KEY_CONTEXT)),
    ]);
}

/// The overlay that is open.
#[derive(Clone, Debug, PartialEq)]
pub enum Overlay {
    Menu {
        ch: ChannelId,
        id: MessageId,
        at: Point<Pixels>,
    },
    Reactors {
        ch: ChannelId,
        id: MessageId,
        emoji: p::Emoji,
        at: Point<Pixels>,
    },
    ConfirmDelete {
        ch: ChannelId,
        id: MessageId,
    },
    Forward {
        ch: ChannelId,
        id: MessageId,
    },
}

impl Overlay {
    fn target(&self) -> (ChannelId, MessageId) {
        match self {
            Overlay::Menu { ch, id, .. }
            | Overlay::Reactors { ch, id, .. }
            | Overlay::ConfirmDelete { ch, id }
            | Overlay::Forward { ch, id } => (*ch, *id),
        }
    }
}

/// The message actions' view state.
#[derive(Default)]
pub struct ActionsState {
    pub overlay: Option<Overlay>,
    pub clicks: ReactionClicks,
    pub reactors: Reactors,
    /// Messages saved (or unsaved) in this session, newest last. Fluxer sends no list of
    /// saved messages with READY, so a message saved elsewhere before shows "Save".
    saved: VecDeque<(MessageId, bool)>,
    in_flight: HashMap<(Kind, MessageId), Instant>,
    /// The row and the bar under the pointer.
    hovered_row: Option<MessageId>,
    hovered_bar: Option<MessageId>,
    focus: Option<FocusHandle>,
    restore_focus: Option<FocusHandle>,
    /// An overlay has the focus (and gives it back when it closes).
    holds_focus: bool,
    /// Demo scenes: a bar shown without the pointer.
    pub shown_bar: Option<MessageId>,
    #[cfg(feature = "demo")]
    demo_done: bool,
}

impl ActionsState {
    /// Whether the bar of message `id` shows: under the pointer, or its menu is open.
    pub fn shows_bar(&self, id: MessageId) -> bool {
        self.hovered_row == Some(id)
            || self.hovered_bar == Some(id)
            || self.shown_bar == Some(id)
            || matches!(
                &self.overlay,
                Some(Overlay::Menu { id: m, .. }) if *m == id
            )
    }

    /// The pointer entered or left a row. Whether anything changed.
    pub fn hover_row(&mut self, id: MessageId, hovered: bool) -> bool {
        let before = self.hovered_row;
        if hovered {
            self.hovered_row = Some(id);
        } else if self.hovered_row == Some(id) {
            self.hovered_row = None;
        }
        before != self.hovered_row
    }

    /// The pointer entered or left a bar (it sticks out above its row).
    pub fn hover_bar(&mut self, id: MessageId, hovered: bool) {
        if hovered {
            self.hovered_bar = Some(id);
        } else if self.hovered_bar == Some(id) {
            self.hovered_bar = None;
        }
    }

    pub fn is_saved(&self, id: MessageId) -> bool {
        self.saved
            .iter()
            .rev()
            .find(|(m, _)| *m == id)
            .is_some_and(|(_, saved)| *saved)
    }

    pub(crate) fn set_saved(&mut self, id: MessageId, saved: bool) {
        self.saved.retain(|(m, _)| *m != id);
        if self.saved.len() >= MAX_SAVED {
            self.saved.pop_front();
        }
        self.saved.push_back((id, saved));
    }

    /// An action is sent unless the same one is on its way. Whether to send it.
    pub fn begin(&mut self, kind: Kind, id: MessageId, now: Instant) -> bool {
        self.in_flight
            .retain(|_, at| now.saturating_duration_since(*at) < IN_FLIGHT_FOR);
        if self.in_flight.contains_key(&(kind, id)) {
            return false;
        }
        self.in_flight.insert((kind, id), now);
        true
    }

    fn end(&mut self, kind: Kind, id: MessageId) {
        self.in_flight.remove(&(kind, id));
    }

    /// Signed out: nothing of the account stays (the focus handle is the window's).
    pub fn clear(&mut self) {
        let focus = self.focus.take();
        // Request numbers go on counting, so a late answer never fits a new request.
        let mut reactors = std::mem::take(&mut self.reactors);
        reactors.clear();
        *self = ActionsState {
            focus,
            reactors,
            ..ActionsState::default()
        };
    }
}

/// What "mark as unread" from a message sets: the read mark (the message before it in
/// the window, or the ID just below), the mentions of the account after it, and the
/// manual acknowledgement for the state. As Fluxer's `markAsUnread`. `None` without a
/// window. A channel without a read state ignores the acknowledgement locally; the
/// server still gets it.
pub(crate) struct UnreadMark {
    pub after: MessageId,
    pub mentions: u64,
    pub ack: p::Dispatch,
}

pub(crate) fn unread_mark(
    state: &inlet_state::AccountState,
    ch: ChannelId,
    id: MessageId,
) -> Option<UnreadMark> {
    let w = state.messages(ch)?;
    let after = w
        .iter()
        .filter_map(|m| MessageId::parse(&m.id))
        .filter(|m| *m < id)
        .max()
        .unwrap_or(MessageId(id.0.saturating_sub(1)));
    let me = state.me_id().to_string();
    let mentions = w
        .iter()
        .filter(|m| MessageId::parse(&m.id).is_some_and(|m| m > after))
        .filter(|m| m.author.id != me && mentions_me(state, ch, m))
        .count() as u64;
    let ack = serde_json::from_value::<p::models::MessageAck>(json!({
        "channel_id": ch.to_string(),
        "message_id": after.to_string(),
        "mention_count": mentions,
        "manual": true,
    }))
    .ok()?;
    Some(UnreadMark {
        after,
        mentions,
        ack: p::Dispatch::MessageAck(Box::new(ack)),
    })
}

/// The frame every overlay is drawn in: it holds the focus and the keys.
fn key_frame(focus: &FocusHandle) -> gpui::Div {
    div().key_context(KEY_CONTEXT).track_focus(focus)
}

impl Inlet {
    /// Right click on a message, or "more" in its bar: the context menu at `at`.
    pub(crate) fn open_message_menu(
        &mut self,
        ch: ChannelId,
        id: MessageId,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.action_facts(ch, id, cx).is_some() {
            self.open_overlay(Overlay::Menu { ch, id, at }, window, cx);
        }
    }

    /// Opens an overlay (closing any other) and gives it the focus.
    pub(crate) fn open_overlay(
        &mut self,
        overlay: Overlay,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let a = &mut self.chat.actions;
        let focus = a.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        if a.overlay.is_none() {
            a.restore_focus = window.focused(cx).filter(|f| *f != focus);
        }
        a.overlay = Some(overlay);
        a.holds_focus = true;
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Closes the open overlay and gives the focus back.
    pub(crate) fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let a = &mut self.chat.actions;
        if a.overlay.take().is_none() {
            return;
        }
        a.holds_focus = false;
        let back = a
            .restore_focus
            .take()
            .unwrap_or_else(|| self.composer.view.focus_handle(cx));
        window.focus(&back, cx);
        cx.notify();
    }

    /// Called when the window is drawn: an open overlay keeps the keyboard (it takes it
    /// back after a layer over it, such as the link prompt, closed), and one that closed
    /// by itself (its message went, another channel opened) gives it back. Layers over
    /// the chat (`desktop::layer_open`) own the keyboard while they are open.
    pub(crate) fn focus_message_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.layer_open() {
            return;
        }
        let composer = self.composer.view.focus_handle(cx);
        let a = &mut self.chat.actions;
        if a.overlay.is_some() {
            let handle = a.focus.get_or_insert_with(|| cx.focus_handle()).clone();
            if !handle.is_focused(window) {
                if !a.holds_focus {
                    a.restore_focus = window.focused(cx).filter(|f| *f != handle);
                    a.holds_focus = true;
                }
                cx.defer_in(window, move |_, window, cx| handle.focus(window, cx));
            }
        } else if a.holds_focus {
            a.holds_focus = false;
            let back = a.restore_focus.take().unwrap_or(composer);
            cx.defer_in(window, move |_, window, cx| back.focus(window, cx));
        }
    }

    /// "Mark as unread" from message `id`: the account's read mark goes to the message
    /// before it, keeping the mentions after it, here and on the server. The open
    /// channel is then not read again until it is opened again or marked as read (the
    /// state ignores automatic reads while a "mark unread" holds).
    pub(crate) fn mark_unread_from(
        &mut self,
        ch: ChannelId,
        id: MessageId,
        cx: &mut Context<Self>,
    ) {
        let Some(account) = self.account.clone() else {
            return;
        };
        let Some(UnreadMark {
            after,
            mentions,
            ack,
        }) = unread_mark(account.read(cx), ch, id)
        else {
            return;
        };
        let changes = account.update(cx, |s, _| s.apply(&ack));
        // An acknowledgement still waiting would move the mark forward again.
        self.notifications.drop_pending_ack(ch);
        self.chat.view.mark_unread_after(ch, after);
        self.backend.send(Cmd::Action(actions::Cmd::MarkUnread {
            channel_id: ch.to_string(),
            after: after.to_string(),
            mentions,
        }));
        self.after_changes(changes, false, cx);
        self.refresh_rows(cx);
    }

    /// SAVED_MESSAGE_CREATE and SAVED_MESSAGE_DELETE from another session.
    pub(crate) fn note_saved(&mut self, dispatch: &p::Dispatch) {
        match dispatch {
            p::Dispatch::SavedMessageCreate(m) => {
                if let Some(id) = MessageId::parse(&m.id) {
                    self.chat.actions.set_saved(id, true);
                }
            }
            p::Dispatch::SavedMessageDelete(d) => {
                if let Some(id) = MessageId::parse(&d.message_id) {
                    self.chat.actions.set_saved(id, false);
                }
            }
            _ => {}
        }
    }

    /// The answers of the message actions.
    pub(crate) fn on_action_event(&mut self, event: Ev, cx: &mut Context<Self>) {
        match event {
            Ev::ReactFailed {
                channel_id,
                message_id,
                emoji,
                add,
                reason,
            } => {
                self.reaction_failed(&channel_id, &message_id, &emoji, add, cx);
                self.chat.error = Some(t!("action-react-failed", reason = reason));
            }
            Ev::Done { message_id, kind } => {
                let Some(id) = MessageId::parse(&message_id) else {
                    return;
                };
                self.chat.actions.end(kind, id);
                match kind {
                    Kind::Save => self.chat.actions.set_saved(id, true),
                    Kind::Unsave => self.chat.actions.set_saved(id, false),
                    Kind::Forward => self.chat.notice = Some(t!("action-forwarded")),
                    _ => {}
                }
                if matches!(kind, Kind::Save) {
                    self.chat.notice = Some(t!("action-saved"));
                }
            }
            Ev::Failed {
                message_id,
                kind,
                reason,
            } => {
                if let Some(id) = MessageId::parse(&message_id) {
                    self.chat.actions.end(kind, id);
                }
                // The inbox's saved list hid the row already: it loads again.
                if kind == Kind::Unsave {
                    self.inbox_unsave_failed(cx);
                }
                self.chat.error = Some(match kind {
                    Kind::Delete => t!("action-delete-failed", reason = reason),
                    Kind::Pin | Kind::Unpin => t!("action-pin-failed", reason = reason),
                    Kind::Forward => t!("action-forward-failed", reason = reason),
                    Kind::Save | Kind::Unsave => t!("action-save-failed", reason = reason),
                    Kind::HideEmbeds | Kind::ShowEmbeds => {
                        t!("action-embeds-failed", reason = reason)
                    }
                });
            }
            Ev::Link(url) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(url));
            }
            Ev::Reactors {
                request,
                names,
                more,
            } => self.chat.actions.reactors.answered(
                request,
                Ok(reactors::Names { names, more }),
                Instant::now(),
            ),
            Ev::ReactorsFailed { request, reason } => {
                self.chat
                    .actions
                    .reactors
                    .answered(request, Err(reason), Instant::now())
            }
        }
        cx.notify();
    }

    /// The overlays of the message actions, for the window's overlay layer.
    pub(crate) fn render_message_overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        #[cfg(feature = "demo")]
        self.demo_overlays();
        let Some(overlay) = self.chat.actions.overlay.clone() else {
            return Vec::new();
        };
        let (ch, id) = overlay.target();
        // The message went away, or another channel is open: nothing to act on.
        if self.open_channel_id() != Some(ch) || self.action_facts(ch, id, cx).is_none() {
            // The focus goes back on the next frame (`focus_message_overlay`).
            self.chat.actions.overlay = None;
            cx.notify();
            return Vec::new();
        }
        let focus = self
            .chat
            .actions
            .focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let el = match overlay {
            Overlay::Menu { at, .. } => {
                let content = self.render_menu(ch, id, cx);
                self.popover(&focus, at, content, cx)
            }
            Overlay::Reactors { emoji, at, .. } => {
                let content = self.render_reactors(ch, id, &emoji, cx);
                self.popover(&focus, at, content, cx)
            }
            Overlay::ConfirmDelete { .. } => self.render_confirm_delete(&focus, ch, id, cx),
            Overlay::Forward { .. } => self.render_forward(&focus, ch, id, cx),
        };
        vec![el]
    }

    /// A floating box at `at` that stays inside the window and closes on a click outside.
    fn popover(
        &self,
        focus: &FocusHandle,
        at: Point<Pixels>,
        content: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        deferred(
            anchored()
                .position(at)
                .snap_to_window_with_margin(px(8.))
                .child(
                    key_frame(focus)
                        .id("message-popover")
                        .occlude()
                        .p(px(6.))
                        .rounded(px(8.))
                        .bg(theme::floating_bg())
                        .border_1()
                        .border_color(theme::accent_border())
                        .shadow(vec![gpui::BoxShadow {
                            color: theme::floating_shadow(),
                            offset: point(px(0.), px(6.)),
                            blur_radius: px(14.),
                            spread_radius: px(0.),
                            inset: false,
                        }])
                        .on_action(cx.listener(|this, _: &CloseMessageOverlay, window, cx| {
                            this.close_overlay(window, cx)
                        }))
                        .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            this.close_overlay(window, cx)
                        }))
                        .child(content),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// The context menu: quick reactions, then every action the message offers.
    fn render_menu(&mut self, ch: ChannelId, id: MessageId, cx: &mut Context<Self>) -> AnyElement {
        let Some(facts) = self.action_facts(ch, id, cx) else {
            return div().into_any_element();
        };
        let list = menu_actions(&facts);
        let mut menu = div().w(px(MENU_WIDTH)).flex().flex_col();
        if list.contains(&Action::AddReaction) {
            let mut quick = div().flex().flex_row().gap(px(4.)).pb(px(4.));
            for (i, glyph) in reactions::QUICK.iter().enumerate() {
                let emoji = p::Emoji::unicode(*glyph);
                quick = quick.child(
                    div()
                        .id(SharedString::from(format!("menu-quick-{i}")))
                        .size(px(36.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.))
                        .bg(theme::reaction_bg())
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::floating_hover()))
                        .child(reactions::emoji_el(
                            &format!("menu-quick-e-{i}"),
                            &emoji,
                            14.,
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.toggle_reaction(ch, id, emoji.clone(), cx);
                            this.close_overlay(window, cx);
                        })),
                );
            }
            menu = menu.child(quick).child(separator());
        }
        let mut last_group = None;
        for a in list {
            let group = group_of(a);
            if last_group.is_some_and(|g| g != group) {
                menu = menu.child(separator());
            }
            last_group = Some(group);
            menu = menu.child(
                menu_item(format!("menu-{a:?}"), a.label(), a.icon(), a.danger()).on_click(
                    cx.listener(move |this, e: &ClickEvent, window, cx| {
                        this.run_action(a, ch, id, e.modifiers().shift, e.position(), window, cx)
                    }),
                ),
            );
        }
        menu.into_any_element()
    }

    /// "Delete message?" with the message, Cancel and Delete. Enter deletes.
    fn render_confirm_delete(
        &mut self,
        focus: &FocusHandle,
        ch: ChannelId,
        id: MessageId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (author, snippet) = self
            .account
            .as_ref()
            .and_then(|a| {
                let m = a.read(cx).messages(ch)?.get(id)?;
                Some((
                    model::clean_name(m.author.display_name()),
                    model::snippet(&m.content, 300),
                ))
            })
            .unwrap_or_default();
        modal(
            "confirm-delete",
            440.,
            [
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_strong())
                    .child(t!("action-delete-title"))
                    .into_any_element(),
                div()
                    .text_color(theme::text_muted())
                    .child(t!("action-delete-body"))
                    .into_any_element(),
                div()
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(theme::bg_textarea())
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text_strong())
                            .child(author),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(theme::text())
                            .child(snippet),
                    )
                    .into_any_element(),
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_tertiary())
                    .child(t!("action-delete-shift-tip"))
                    .into_any_element(),
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("delete-cancel", t!("common-cancel"), false).on_click(
                            cx.listener(|this, _, window, cx| this.close_overlay(window, cx)),
                        ),
                    )
                    .child(
                        button("delete-confirm", t!("action-delete"), false)
                            .bg(theme::danger())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete(ch, id, window, cx)
                            })),
                    )
                    .into_any_element(),
            ],
        )
        .key_context(KEY_CONTEXT)
        .track_focus(focus)
        .on_action(
            cx.listener(|this, _: &CloseMessageOverlay, window, cx| this.close_overlay(window, cx)),
        )
        .on_action(
            cx.listener(move |this, _: &ConfirmMessageOverlay, window, cx| {
                this.confirm_delete(ch, id, window, cx)
            }),
        )
        .into_any_element()
    }

    fn confirm_delete(
        &mut self,
        ch: ChannelId,
        id: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_overlay(window, cx);
        // The rights are checked again: a role may have changed while the window was open.
        if self
            .action_facts(ch, id, cx)
            .is_some_and(|f| super::action_bar::may_delete(&f))
        {
            self.delete_message(ch, id);
        }
    }

    /// Where a message can be forwarded to: the open guild's channels the account may
    /// write in, then its direct messages, newest first.
    fn forward_destinations(&self, cx: &App) -> Vec<(ChannelId, String)> {
        let Some(state) = self.account.as_ref().map(|a| a.read(cx)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(g) = self
            .selected_guild
            .as_deref()
            .and_then(inlet_state::GuildId::parse)
        {
            for c in state.visible_channels(g) {
                let Some(id) = ChannelId::parse(&c.id) else {
                    continue;
                };
                let text = matches!(
                    c.kind,
                    channel_type::GUILD_TEXT | channel_type::GUILD_ANNOUNCEMENT
                );
                if text && state.channel_permissions(id).can_send() {
                    let name = model::clean_name(c.name.as_deref().unwrap_or_default());
                    out.push((id, format!("#{name}")));
                }
            }
        }
        out.truncate(MAX_GUILD_DESTINATIONS);
        for c in state.private_channels() {
            let Some(id) = ChannelId::parse(&c.id) else {
                continue;
            };
            if !matches!(c.kind, channel_type::DM | channel_type::GROUP_DM) {
                continue;
            }
            let who = c
                .name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .map(model::clean_name)
                .unwrap_or_else(|| {
                    c.recipients
                        .iter()
                        .map(|u| model::clean_name(u.display_name()))
                        .collect::<Vec<_>>()
                        .join(", ")
                });
            out.push((id, format!("@{who}")));
        }
        out.truncate(MAX_DESTINATIONS);
        out
    }

    /// "Forward message": one click on a destination sends it there.
    // Deviation: Fluxer's forward dialog searches every destination, takes several at
    // once and a comment; Inlet lists the open server's channels and the direct
    // messages, and forwards to one per click.
    fn render_forward(
        &mut self,
        focus: &FocusHandle,
        ch: ChannelId,
        id: MessageId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = div()
            .id("forward-list")
            .max_h(px(360.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.));
        let destinations = self.forward_destinations(cx);
        if destinations.is_empty() {
            list = list.child(
                div()
                    .text_color(theme::text_tertiary())
                    .child(t!("action-forward-nowhere")),
            );
        }
        for (i, (to, name)) in destinations.into_iter().enumerate() {
            list = list.child(
                div()
                    .id(SharedString::from(format!("forward-{i}")))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_color(theme::text())
                    .hover(|s| s.bg(theme::hover_bg()))
                    .child(name)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.forward_to(ch, id, to, window, cx)
                    })),
            );
        }
        modal(
            "forward",
            440.,
            [
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_strong())
                    .child(t!("action-forward-title"))
                    .into_any_element(),
                list.into_any_element(),
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .child(
                        button("forward-cancel", t!("common-cancel"), false).on_click(
                            cx.listener(|this, _, window, cx| this.close_overlay(window, cx)),
                        ),
                    )
                    .into_any_element(),
            ],
        )
        .key_context(KEY_CONTEXT)
        .track_focus(focus)
        .on_action(
            cx.listener(|this, _: &CloseMessageOverlay, window, cx| this.close_overlay(window, cx)),
        )
        .into_any_element()
    }

    fn forward_to(
        &mut self,
        ch: ChannelId,
        id: MessageId,
        to: ChannelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_overlay(window, cx);
        let Some(state) = self.account.as_ref().map(|a| a.read(cx)) else {
            return;
        };
        // Checked again: reading the message and writing in the destination.
        let allowed = self
            .action_facts(ch, id, cx)
            .is_some_and(|f| menu_actions(&f).contains(&Action::Forward))
            && state.channel_permissions(to).can_send();
        if !allowed {
            return;
        }
        let source = p::ForwardSource {
            message_id: id.to_string(),
            channel_id: ch.to_string(),
            guild_id: state.channels().guild_of(ch).map(|g| g.to_string()),
            ..Default::default()
        };
        self.send_once(
            Kind::Forward,
            actions::Cmd::Forward {
                source,
                to_channel: to.to_string(),
                nonce: p::new_nonce(),
            },
            id,
        );
    }

    /// Demo scenes open the bar, the menu or the delete confirmation on a fixed message
    /// once it is on screen, so the screenshots show them without a pointer.
    #[cfg(feature = "demo")]
    fn demo_overlays(&mut self) {
        use crate::demo::scenes::{current, Scene};
        if self.chat.actions.demo_done {
            return;
        }
        let scene = current();
        let Some((ch, id)) = crate::demo::actions::target(scene) else {
            self.chat.actions.demo_done = true;
            return;
        };
        if !self
            .chat
            .view
            .rows
            .iter()
            .any(|r| r.message_id() == Some(id))
        {
            return;
        }
        self.chat.actions.demo_done = true;
        self.chat.actions.shown_bar = Some(id);
        let at = point(px(560.), px(150.));
        self.chat.actions.overlay = match scene {
            Scene::ChatMenu => Some(Overlay::Menu { ch, id, at }),
            Scene::ChatDelete => Some(Overlay::ConfirmDelete { ch, id }),
            _ => None,
        };
    }
}

fn separator() -> gpui::Div {
    div()
        .my(px(4.))
        .mx(px(4.))
        .h(px(1.))
        .bg(theme::accent_border())
}

/// One line of the context menu: the label, and the icon at the right.
fn menu_item(
    id: String,
    label: String,
    icon: ActionIcon,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
    let color = if danger {
        theme::danger()
    } else {
        theme::text_muted()
    };
    div()
        .id(SharedString::from(id))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .h(px(32.))
        .px(px(8.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_size(px(14.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .hover(move |s| {
            if danger {
                s.bg(theme::danger()).text_color(theme::on_danger())
            } else {
                s.bg(theme::floating_hover())
                    .text_color(theme::text_strong())
            }
        })
        .child(label)
        .child(icon.el(px(18.), color))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn a_second_click_on_the_same_action_waits_for_the_answer() {
        let mut a = ActionsState::default();
        let t0 = Instant::now();
        assert!(a.begin(Kind::Delete, MessageId(5), t0));
        assert!(!a.begin(Kind::Delete, MessageId(5), t0));
        assert!(a.begin(Kind::Pin, MessageId(5), t0));
        a.end(Kind::Delete, MessageId(5));
        assert!(a.begin(Kind::Delete, MessageId(5), t0));
        // An answer that never came does not block for ever.
        assert!(a.begin(Kind::Delete, MessageId(5), t0 + IN_FLIGHT_FOR));
    }

    #[test]
    fn saved_messages_are_remembered_and_bounded() {
        let mut a = ActionsState::default();
        a.set_saved(MessageId(1), true);
        assert!(a.is_saved(MessageId(1)));
        a.set_saved(MessageId(1), false);
        assert!(!a.is_saved(MessageId(1)));
        for i in 0..(MAX_SAVED as u64 + 10) {
            a.set_saved(MessageId(100 + i), true);
        }
        assert_eq!(a.saved.len(), MAX_SAVED);
        assert!(!a.is_saved(MessageId(100)));
    }

    #[test]
    fn the_bar_shows_under_the_pointer_and_with_its_menu() {
        let mut a = ActionsState::default();
        let (m1, m2) = (MessageId(1), MessageId(2));
        assert!(a.hover_row(m1, true));
        assert!(a.shows_bar(m1) && !a.shows_bar(m2));
        // Moving to the next row: leaving the first one late does not hide the second.
        a.hover_row(m2, true);
        assert!(!a.hover_row(m1, false));
        assert!(a.shows_bar(m2));
        // The bar sticks out above its row: on the bar, it stays.
        a.hover_bar(m2, true);
        a.hover_row(m2, false);
        assert!(a.shows_bar(m2));
        a.hover_bar(m2, false);
        assert!(!a.shows_bar(m2));
        a.overlay = Some(Overlay::Menu {
            ch: ChannelId(9),
            id: m1,
            at: point(px(0.), px(0.)),
        });
        assert!(a.shows_bar(m1));
    }

    struct Host {
        focus: FocusHandle,
        closed: usize,
        marked_read: usize,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .on_action(
                    cx.listener(|h, _: &crate::ui::notifications::MarkChannelRead, _, _| {
                        h.marked_read += 1
                    }),
                )
                .on_action(
                    cx.listener(|h, _: &crate::ui::notifications::MarkGuildRead, _, _| {
                        h.marked_read += 1
                    }),
                )
                .child(
                    key_frame(&self.focus)
                        .size(px(100.))
                        .on_action(cx.listener(|h, _: &CloseMessageOverlay, _, _| h.closed += 1)),
                )
        }
    }

    #[gpui::test]
    fn escape_and_shift_escape_close_the_menu_and_mark_nothing_read(cx: &mut TestAppContext) {
        // The app's bindings in the app's order: the global Escape first, then the
        // overlay's.
        cx.update(crate::ui::notifications::bind_keys);
        cx.update(bind_keys);
        let (host, cx) = cx.add_window_view(|_, cx| Host {
            focus: cx.focus_handle(),
            closed: 0,
            marked_read: 0,
        });
        host.update_in(cx, |h, window, cx| window.focus(&h.focus, cx));
        cx.simulate_keystrokes("escape");
        assert_eq!(host.read_with(cx, |h, _| (h.closed, h.marked_read)), (1, 0));
        // Shift+Escape (mark the whole server read) closes it too, and reads nothing.
        cx.simulate_keystrokes("shift-escape");
        assert_eq!(host.read_with(cx, |h, _| (h.closed, h.marked_read)), (2, 0));
    }
}
