// SPDX-License-Identifier: AGPL-3.0-or-later
//! What can be done with a message, and the hover bar over its top right corner.
//!
//! Which actions a message offers is worked out from the account's permissions in its
//! channel ([`Perms`], from the one account state: overwrites, `ADMINISTRATOR` and the
//! guild owner are already in them) and from the message itself ([`Facts`]). Actions
//! the account may not do are not shown, and the rights are checked again when an action
//! runs, so a menu that stayed open while a role changed cannot do more than it may.
//! Unknown rights are no rights: until the roles are loaded nothing that needs one is
//! offered (copying the text, link or ID needs none).
//!
//! The bar shows the quick reactions, "add reaction", edit (own messages) or reply,
//! forward and "more" (the context menu); the context menu lists everything.

use std::time::Instant;

use gpui::{
    div, point, prelude::*, px, svg, AnyElement, ClickEvent, Context, Hsla, Pixels, Point,
    SharedString, Svg, Window,
};
use inlet_protocol as p;
use inlet_protocol::permissions::{
    ADMINISTRATOR, MANAGE_MESSAGES, READ_MESSAGE_HISTORY, VIEW_CHANNEL,
};
use inlet_state::{AccountState, ChannelId, MessageId, Permissions, UserId};

use super::context_menu::Overlay;
use super::messages::MessageRow;
use super::reactions::{self, ReactionChip};
use crate::app::Inlet;
use crate::backend::messages::actions::{self, Kind};
use crate::backend::messages::Cmd;
use crate::theme;
use crate::ui::composer;

/// Size of a bar button (Fluxer: 28 × 28, radius 6, icon 20).
const BUTTON: f32 = 28.;
const ICON: f32 = 20.;

/// Icons of the message actions (Phosphor, MIT, fill weight).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionIcon {
    Smiley,
    Reply,
    Forward,
    Pencil,
    More,
    Trash,
    Pin,
    Copy,
    Link,
    Id,
    Bookmark,
    EyeSlash,
    Eye,
    Unread,
}

impl ActionIcon {
    fn data(self) -> &'static [u8] {
        match self {
            ActionIcon::Smiley => include_bytes!("../../../assets/icons/smiley.svg"),
            ActionIcon::Reply => include_bytes!("../../../assets/icons/arrow-bend-up-left.svg"),
            ActionIcon::Forward => {
                include_bytes!("../../../assets/icons/arrow-bend-up-right.svg")
            }
            ActionIcon::Pencil => include_bytes!("../../../assets/icons/pencil-simple.svg"),
            ActionIcon::More => include_bytes!("../../../assets/icons/dots-three.svg"),
            ActionIcon::Trash => include_bytes!("../../../assets/icons/trash.svg"),
            ActionIcon::Pin => include_bytes!("../../../assets/icons/push-pin.svg"),
            ActionIcon::Copy => include_bytes!("../../../assets/icons/copy.svg"),
            ActionIcon::Link => include_bytes!("../../../assets/icons/link.svg"),
            ActionIcon::Id => include_bytes!("../../../assets/icons/identification-card.svg"),
            ActionIcon::Bookmark => include_bytes!("../../../assets/icons/bookmark-simple.svg"),
            ActionIcon::EyeSlash => include_bytes!("../../../assets/icons/eye-slash.svg"),
            ActionIcon::Eye => include_bytes!("../../../assets/icons/eye.svg"),
            ActionIcon::Unread => include_bytes!("../../../assets/icons/chat-circle-dots.svg"),
        }
    }

    pub fn el(self, size: Pixels, color: Hsla) -> Svg {
        svg()
            .data(self.data())
            .size(size)
            .flex_none()
            .text_color(color)
    }
}

/// The account's rights in a channel, as far as message actions need them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perms {
    pub bits: Permissions,
    /// A direct message or group: every bit is set there, but nobody moderates.
    pub private: bool,
    /// The account is in a communication timeout in this guild (and is no
    /// administrator): no reactions and no edits, as the API refuses them.
    pub timed_out: bool,
}

impl Perms {
    /// The rights in `ch`. An unknown channel, or a guild whose roles are not loaded
    /// yet, has none.
    pub fn of(state: &AccountState, ch: ChannelId) -> Perms {
        let bits = state.channel_permissions(ch);
        let guild = state.channels().guild_of(ch);
        let timed_out = guild
            .and_then(|g| state.guild(g))
            .and_then(|g| g.members.me())
            .is_some_and(|m| m.is_timed_out_at(state.now_ms()))
            && !bits.has(ADMINISTRATOR);
        Perms {
            bits,
            private: state.channel(ch).is_some() && guild.is_none(),
            timed_out,
        }
    }

    /// Whether a reaction may be added, as Fluxer's API decides: a new emoji needs
    /// `ADD_REACTIONS`; joining an existing one needs that or the history; never during
    /// a timeout.
    pub fn may_react(self, exists: bool) -> bool {
        if self.timed_out {
            return false;
        }
        self.bits.can_react() || (exists && self.bits.has(VIEW_CHANNEL | READ_MESSAGE_HISTORY))
    }

    /// Whether someone else's message may be deleted or have its embeds hidden.
    pub fn moderates(self) -> bool {
        !self.private && self.bits.has(VIEW_CHANNEL | MANAGE_MESSAGES)
    }
}

/// What the rows know about a message for its actions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowMeta {
    /// Written by the account.
    pub own: bool,
    pub pinned: bool,
    /// Its embeds are hidden (`SUPPRESS_EMBEDS`).
    pub embeds_hidden: bool,
    pub reactions: Vec<ReactionChip>,
}

impl RowMeta {
    pub fn of(me: UserId, m: &p::Message) -> RowMeta {
        RowMeta {
            own: UserId::parse(&m.author.id) == Some(me),
            pinned: m.pinned,
            embeds_hidden: m.embeds_suppressed(),
            reactions: ReactionChip::of(m),
        }
    }
}

/// Everything the choice of actions depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    pub perms: Perms,
    pub own: bool,
    pub pinned: bool,
    pub has_text: bool,
    pub has_embeds: bool,
    pub embeds_hidden: bool,
    /// Its kind can be edited by the account ([`p::Message::can_edit`]: an own default
    /// message or reply, not a forward or a system message).
    pub editable: bool,
    /// Saved by the account (as far as this session knows).
    pub saved: bool,
}

/// One message action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    AddReaction,
    Reply,
    Edit,
    Forward,
    Pin,
    Unpin,
    MarkUnread,
    Save,
    Unsave,
    CopyText,
    CopyLink,
    CopyId,
    HideEmbeds,
    ShowEmbeds,
    Delete,
}

impl Action {
    pub fn icon(self) -> ActionIcon {
        match self {
            Action::AddReaction => ActionIcon::Smiley,
            Action::Reply => ActionIcon::Reply,
            Action::Edit => ActionIcon::Pencil,
            Action::Forward => ActionIcon::Forward,
            Action::Pin | Action::Unpin => ActionIcon::Pin,
            Action::MarkUnread => ActionIcon::Unread,
            Action::Save | Action::Unsave => ActionIcon::Bookmark,
            Action::CopyText => ActionIcon::Copy,
            Action::CopyLink => ActionIcon::Link,
            Action::CopyId => ActionIcon::Id,
            Action::HideEmbeds => ActionIcon::EyeSlash,
            Action::ShowEmbeds => ActionIcon::Eye,
            Action::Delete => ActionIcon::Trash,
        }
    }

    pub fn label(self) -> String {
        match self {
            Action::AddReaction => t!("action-add-reaction"),
            Action::Reply => t!("action-reply"),
            Action::Edit => t!("action-edit"),
            Action::Forward => t!("action-forward"),
            Action::Pin => t!("action-pin"),
            Action::Unpin => t!("action-unpin"),
            Action::MarkUnread => t!("action-mark-unread"),
            Action::Save => t!("action-save"),
            Action::Unsave => t!("action-unsave"),
            Action::CopyText => t!("action-copy-text"),
            Action::CopyLink => t!("action-copy-link"),
            Action::CopyId => t!("action-copy-id"),
            Action::HideEmbeds => t!("action-hide-embeds"),
            Action::ShowEmbeds => t!("action-show-embeds"),
            Action::Delete => t!("action-delete"),
        }
    }

    pub fn danger(self) -> bool {
        self == Action::Delete
    }
}

/// Whether the message may be deleted: the account's own, or someone else's with
/// `MANAGE_MESSAGES` in a guild channel (never in a direct message).
pub fn may_delete(f: &Facts) -> bool {
    f.own || f.perms.moderates()
}

/// Every action the message offers, in the context menu's order. Groups are split by
/// [`group_of`].
pub fn menu_actions(f: &Facts) -> Vec<Action> {
    let b = f.perms.bits;
    let read = b.has(VIEW_CHANNEL | READ_MESSAGE_HISTORY);
    let mut out = Vec::new();
    if f.perms.may_react(false) {
        out.push(Action::AddReaction);
    }
    // As Fluxer: editing an own message does not need SEND_MESSAGES, but the API
    // refuses it during a timeout.
    if f.editable && b.can_view() && !f.perms.timed_out {
        out.push(Action::Edit);
    }
    if b.can_send() {
        out.push(Action::Reply);
    }
    if read {
        out.push(Action::Forward);
    }
    if b.can_pin() {
        out.push(if f.pinned { Action::Unpin } else { Action::Pin });
    }
    if b.can_view() {
        out.push(Action::MarkUnread);
    }
    if read {
        out.push(if f.saved {
            Action::Unsave
        } else {
            Action::Save
        });
    }
    // A message whose embeds are hidden may have none listed (the server stops
    // fetching them), and must still be able to show them again.
    if (f.has_embeds || f.embeds_hidden) && (f.own || f.perms.moderates()) {
        out.push(if f.embeds_hidden {
            Action::ShowEmbeds
        } else {
            Action::HideEmbeds
        });
    }
    if f.has_text {
        out.push(Action::CopyText);
    }
    out.push(Action::CopyLink);
    out.push(Action::CopyId);
    if may_delete(f) {
        out.push(Action::Delete);
    }
    out
}

/// The menu's groups: a line goes between two actions of different groups.
pub fn group_of(a: Action) -> u8 {
    match a {
        Action::AddReaction | Action::Edit | Action::Reply | Action::Forward => 0,
        Action::Pin | Action::Unpin | Action::MarkUnread | Action::Save | Action::Unsave => 1,
        Action::HideEmbeds | Action::ShowEmbeds => 2,
        Action::CopyText | Action::CopyLink | Action::CopyId => 3,
        Action::Delete => 4,
    }
}

/// The bar's buttons after the quick reactions (which show when [`Action::AddReaction`]
/// does): add reaction, edit or reply, forward, and always "more".
pub fn bar_actions(f: &Facts) -> Vec<Action> {
    let menu = menu_actions(f);
    let mut out = Vec::new();
    for a in [
        Action::AddReaction,
        Action::Edit,
        Action::Reply,
        Action::Forward,
    ] {
        if !menu.contains(&a) {
            continue;
        }
        // Own messages show edit, others reply (the menu has both).
        if a == Action::Reply && menu.contains(&Action::Edit) {
            continue;
        }
        out.push(a);
    }
    out
}

impl Inlet {
    /// What the actions of message `id` in `ch` depend on, from the account state.
    /// `None` when the message is not in the open window.
    pub(crate) fn action_facts(
        &self,
        ch: ChannelId,
        id: MessageId,
        cx: &gpui::App,
    ) -> Option<Facts> {
        let state = self.account.as_ref()?.read(cx);
        let m = state.messages(ch)?.get(id)?;
        let meta = RowMeta::of(state.me_id(), m);
        let me = state.me_id().to_string();
        Some(Facts {
            editable: m.can_edit(&me),
            perms: Perms::of(state, ch),
            own: meta.own,
            pinned: meta.pinned,
            has_text: !m.content.is_empty(),
            has_embeds: !m.embeds.is_empty(),
            embeds_hidden: meta.embeds_hidden,
            saved: self.chat.actions.is_saved(id),
        })
    }

    /// The hover bar of a message row, while it is under the pointer (or its menu is
    /// open).
    pub(super) fn render_action_bar(
        &self,
        row: &MessageRow,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let id = row.id;
        if !self.chat.actions.shows_bar(id) {
            return None;
        }
        let ch = ChannelId::parse(&row.message.channel_id)?;
        let facts = self.action_facts(ch, id, cx)?;
        let mut bar = div()
            .id(SharedString::from(format!("bar-{id}")))
            .flex()
            .flex_row()
            .items_center()
            .p(px(2.))
            .gap(px(2.))
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
            // The bar sticks out over the row above: only the bar takes the pointer there.
            .occlude()
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.chat.actions.hover_bar(id, *hovered);
                cx.notify();
            }));
        let actions = bar_actions(&facts);
        if actions.contains(&Action::AddReaction) {
            for (i, glyph) in reactions::QUICK.iter().enumerate() {
                let emoji = p::Emoji::unicode(*glyph);
                bar = bar.child(
                    bar_button(format!("quick-{id}-{i}"))
                        .child(reactions::emoji_el(
                            &format!("quick-e-{id}-{i}"),
                            &emoji,
                            13.,
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_reaction(ch, id, emoji.clone(), cx)
                        })),
                );
            }
        }
        for a in actions {
            bar = bar.child(
                bar_button(format!("act-{id}-{a:?}"))
                    .child(a.icon().el(px(ICON), theme::text_tertiary()))
                    .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                        let at = e.position();
                        this.run_action(a, ch, id, e.modifiers().shift, at, window, cx)
                    })),
            );
        }
        bar = bar.child(
            bar_button(format!("more-{id}"))
                .child(ActionIcon::More.el(px(ICON), theme::text_tertiary()))
                .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                    this.open_message_menu(ch, id, e.position(), window, cx)
                })),
        );
        Some(bar.into_any_element())
    }

    /// Runs an action on message `id`, after checking the rights again. `shift` skips
    /// the delete confirmation; `at` is where a popover opens.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_action(
        &mut self,
        action: Action,
        ch: ChannelId,
        id: MessageId,
        shift: bool,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(facts) = self.action_facts(ch, id, cx) else {
            return;
        };
        if !menu_actions(&facts).contains(&action) {
            return;
        }
        let (channel_id, message_id) = (ch.to_string(), id.to_string());
        match action {
            Action::AddReaction => {
                self.open_overlay(Overlay::Picker { ch, id, at }, window, cx);
                return;
            }
            Action::Reply => {
                self.close_overlay(window, cx);
                let author = self.reply_author(ch, id, cx);
                self.composer
                    .view
                    .update(cx, |v, cx| v.start_reply(message_id, author, window, cx));
            }
            Action::Edit => {
                self.close_overlay(window, cx);
                if let Some(account) = self.account.clone() {
                    composer::edit_message(&self.composer.view, &account, ch, id, window, cx);
                }
            }
            Action::Forward => {
                self.open_overlay(Overlay::Forward { ch, id }, window, cx);
                return;
            }
            Action::Delete if !shift => {
                self.open_overlay(Overlay::ConfirmDelete { ch, id }, window, cx);
                return;
            }
            Action::Delete => {
                self.close_overlay(window, cx);
                self.delete_message(ch, id);
            }
            Action::Pin | Action::Unpin => {
                self.close_overlay(window, cx);
                let pin = action == Action::Pin;
                let kind = if pin { Kind::Pin } else { Kind::Unpin };
                self.send_once(
                    kind,
                    actions::Cmd::Pin {
                        channel_id,
                        message_id,
                        pin,
                    },
                    id,
                );
            }
            Action::Save | Action::Unsave => {
                self.close_overlay(window, cx);
                let save = action == Action::Save;
                let kind = if save { Kind::Save } else { Kind::Unsave };
                self.send_once(
                    kind,
                    actions::Cmd::Save {
                        channel_id,
                        message_id,
                        save,
                    },
                    id,
                );
            }
            Action::HideEmbeds | Action::ShowEmbeds => {
                self.close_overlay(window, cx);
                let hide = action == Action::HideEmbeds;
                let raw = self
                    .account
                    .as_ref()
                    .and_then(|a| a.read(cx).messages(ch)?.get(id).cloned());
                if let Some(message) = raw {
                    let kind = if hide {
                        Kind::HideEmbeds
                    } else {
                        Kind::ShowEmbeds
                    };
                    self.send_once(
                        kind,
                        actions::Cmd::HideEmbeds {
                            message: Box::new(message),
                            hide,
                        },
                        id,
                    );
                }
            }
            Action::MarkUnread => {
                self.close_overlay(window, cx);
                self.mark_unread_from(ch, id, cx);
            }
            Action::CopyText => {
                self.close_overlay(window, cx);
                let text = self
                    .account
                    .as_ref()
                    .and_then(|a| Some(a.read(cx).messages(ch)?.get(id)?.content.clone()));
                if let Some(text) = text {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                }
            }
            Action::CopyId => {
                self.close_overlay(window, cx);
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(message_id));
            }
            Action::CopyLink => {
                self.close_overlay(window, cx);
                let guild_id = self
                    .account
                    .as_ref()
                    .and_then(|a| a.read(cx).channels().guild_of(ch).map(|g| g.to_string()));
                self.backend.send(Cmd::Action(actions::Cmd::Link {
                    guild_id,
                    channel_id,
                    message_id,
                }));
            }
        }
        cx.notify();
    }

    /// The cleaned name of a message's author, for the reply bar.
    fn reply_author(&self, ch: ChannelId, id: MessageId, cx: &gpui::App) -> String {
        self.chat
            .view
            .rows
            .iter()
            .find_map(|r| match r {
                super::messages::Row::Message(m) if m.id == id => Some(m.message.author.clone()),
                _ => None,
            })
            .or_else(|| {
                let state = self.account.as_ref()?.read(cx);
                let m = state.messages(ch)?.get(id)?;
                Some(crate::model::clean_name(m.author.display_name()))
            })
            .unwrap_or_default()
    }

    /// Sends a delete (the confirmation was given, or skipped with Shift).
    pub(crate) fn delete_message(&mut self, ch: ChannelId, id: MessageId) {
        self.send_once(
            Kind::Delete,
            actions::Cmd::Delete {
                channel_id: ch.to_string(),
                message_id: id.to_string(),
            },
            id,
        );
    }

    /// Sends an action unless the same one for the same message is still on its way
    /// (a double click sends one request).
    pub(crate) fn send_once(&mut self, kind: Kind, cmd: actions::Cmd, id: MessageId) {
        if self.chat.actions.begin(kind, id, Instant::now()) {
            self.backend.send(Cmd::Action(cmd));
        }
    }
}

/// A 28 × 28 bar button.
fn bar_button(id: String) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .size(px(BUTTON))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::floating_hover()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlet_protocol::permissions as bits;
    use inlet_state::AccountState;
    use serde_json::json;

    /// A guild channel `100` where @everyone has `everyone` and the account has role
    /// `30` with `mine`; and a DM `200`.
    fn state(everyone: u64, mine: u64) -> AccountState {
        state_with(everyone, mine, json!({}))
    }

    /// [`state`] with more fields on the account's member (a timeout).
    fn state_with(everyone: u64, mine: u64, member: serde_json::Value) -> AccountState {
        let mut me =
            json!({"user": {"id": "10"}, "roles": ["30"], "joined_at": "2026-01-01T00:00:00Z"});
        if let (Some(obj), Some(more)) = (me.as_object_mut(), member.as_object()) {
            for (k, v) in more {
                obj.insert(k.clone(), v.clone());
            }
        }
        let ready: p::Ready = serde_json::from_value(json!({
            "session_id": "s",
            "user": {"id": "10", "username": "me"},
            "guilds": [{
                "id": "20",
                "properties": {"id": "20", "name": "G", "owner_id": "99"},
                "channels": [{"id": "100", "type": 0, "guild_id": "20", "name": "general"}],
                "roles": [
                    {"id": "20", "name": "@everyone", "permissions": everyone.to_string(), "position": 0},
                    {"id": "30", "name": "Mods", "permissions": mine.to_string(), "position": 1}
                ],
                "members": [me]
            }],
            "private_channels": [{"id": "200", "type": 1, "recipients": [{"id": "7", "username": "ada"}]}],
        }))
        .unwrap();
        AccountState::from_ready_at(&ready, 1)
    }

    fn facts(state: &AccountState, ch: u64, own: bool) -> Facts {
        Facts {
            perms: Perms::of(state, ChannelId(ch)),
            own,
            pinned: false,
            has_text: true,
            has_embeds: true,
            embeds_hidden: false,
            editable: own,
            saved: false,
        }
    }

    const READER: u64 = bits::VIEW_CHANNEL | bits::READ_MESSAGE_HISTORY;
    const MEMBER: u64 = READER | bits::SEND_MESSAGES | bits::ADD_REACTIONS;

    #[test]
    fn delete_others_requires_manage_messages() {
        let plain = state(MEMBER, 0);
        let others = facts(&plain, 100, false);
        assert!(!may_delete(&others));
        assert!(!menu_actions(&others).contains(&Action::Delete));
        assert!(!menu_actions(&others).contains(&Action::HideEmbeds));
        // Own messages can always be deleted.
        assert!(menu_actions(&facts(&plain, 100, true)).contains(&Action::Delete));

        // A role with MANAGE_MESSAGES may delete others' messages and hide their embeds.
        let moderator = state(MEMBER, bits::MANAGE_MESSAGES);
        let others = facts(&moderator, 100, false);
        assert!(may_delete(&others));
        assert!(menu_actions(&others).contains(&Action::Delete));
        assert!(menu_actions(&others).contains(&Action::HideEmbeds));

        // A channel overwrite that takes MANAGE_MESSAGES away wins over the role.
        let ready_overwrite = {
            let mut s = state(MEMBER, bits::MANAGE_MESSAGES);
            let update = p::Dispatch::parse(
                "CHANNEL_UPDATE",
                json!({"id": "100", "type": 0, "guild_id": "20", "name": "general",
                       "permission_overwrites": [{"id": "30", "type": 0, "allow": "0",
                       "deny": bits::MANAGE_MESSAGES.to_string()}]}),
            );
            let _ = s.apply(&update);
            s
        };
        assert!(!may_delete(&facts(&ready_overwrite, 100, false)));

        // In a direct message every bit is set, but nobody deletes the other's messages.
        let dm = facts(&plain, 200, false);
        assert!(dm.perms.private);
        assert!(!may_delete(&dm));
        assert!(may_delete(&facts(&plain, 200, true)));

        // An unknown channel fails closed.
        assert!(!may_delete(&facts(&plain, 999, false)));
    }

    #[test]
    fn actions_hidden_without_permission() {
        // Only allowed to read: no reactions, reply, edit, pin, delete or embeds.
        let reader = state(READER, 0);
        let f = facts(&reader, 100, false);
        let menu = menu_actions(&f);
        for hidden in [
            Action::AddReaction,
            Action::Reply,
            Action::Edit,
            Action::Pin,
            Action::Delete,
            Action::HideEmbeds,
        ] {
            assert!(!menu.contains(&hidden), "{hidden:?}");
        }
        assert!(menu.contains(&Action::CopyLink));
        assert!(menu.contains(&Action::Forward));
        assert_eq!(bar_actions(&f), vec![Action::Forward]);
        // The own message can be edited without the right to send, as in Fluxer.
        assert!(menu_actions(&facts(&reader, 100, true)).contains(&Action::Edit));
        // Joining an existing reaction needs only the history; a new one needs the right.
        assert!(f.perms.may_react(true));
        assert!(!f.perms.may_react(false));
        // With ADD_REACTIONS but no history, both are allowed (the API takes either).
        let reactor = state(bits::VIEW_CHANNEL | bits::ADD_REACTIONS, 0);
        let p = Perms::of(&reactor, ChannelId(100));
        assert!(p.may_react(true) && p.may_react(false));

        // Not even allowed to see the channel (or the roles are not loaded): nothing
        // but copying.
        let none = state(0, 0);
        let f = facts(&none, 100, false);
        assert_eq!(
            menu_actions(&f),
            vec![Action::CopyText, Action::CopyLink, Action::CopyId]
        );
        assert!(!f.perms.may_react(true));
        assert!(bar_actions(&f).is_empty());

        // A member gets the usual set; pinning needs PIN_MESSAGES.
        let member = state(MEMBER, 0);
        let menu = menu_actions(&facts(&member, 100, false));
        assert!(menu.contains(&Action::AddReaction) && menu.contains(&Action::Reply));
        assert!(!menu.contains(&Action::Pin));
        let pinner = state(MEMBER | bits::PIN_MESSAGES, 0);
        assert!(menu_actions(&facts(&pinner, 100, false)).contains(&Action::Pin));
        // Own messages show edit in the bar, others reply.
        assert_eq!(
            bar_actions(&facts(&member, 100, true)),
            vec![Action::AddReaction, Action::Edit, Action::Forward]
        );
        assert_eq!(
            bar_actions(&facts(&member, 100, false)),
            vec![Action::AddReaction, Action::Reply, Action::Forward]
        );

        // ADMINISTRATOR on a role gives everything in the guild.
        let admin = state(READER, bits::ADMINISTRATOR);
        let menu = menu_actions(&facts(&admin, 100, false));
        assert!(menu.contains(&Action::Delete) && menu.contains(&Action::Pin));

        // A timeout: no reactions (not even joining one) and no edits; reading stays.
        let timed_out = state_with(
            MEMBER,
            0,
            json!({"communication_disabled_until": "2099-01-01T00:00:00Z"}),
        );
        let f = facts(&timed_out, 100, true);
        assert!(f.perms.timed_out);
        assert!(!f.perms.may_react(true) && !f.perms.may_react(false));
        let menu = menu_actions(&f);
        assert!(!menu.contains(&Action::AddReaction) && !menu.contains(&Action::Edit));
        assert!(menu.contains(&Action::Forward) && menu.contains(&Action::MarkUnread));
    }

    #[test]
    fn only_editable_kinds_offer_edit_and_hidden_embeds_can_be_shown() {
        let member = state(MEMBER, 0);
        // An own system message or forward is not editable.
        let not_editable = Facts {
            editable: false,
            ..facts(&member, 100, true)
        };
        assert!(!menu_actions(&not_editable).contains(&Action::Edit));
        // Hidden embeds that the message no longer lists can be shown again.
        let hidden = Facts {
            has_embeds: false,
            embeds_hidden: true,
            ..facts(&member, 100, true)
        };
        assert!(menu_actions(&hidden).contains(&Action::ShowEmbeds));
        let none = Facts {
            has_embeds: false,
            ..facts(&member, 100, true)
        };
        assert!(!menu_actions(&none).contains(&Action::HideEmbeds));

        let me = "10";
        let own = |extra: serde_json::Value| -> p::Message {
            let mut v = json!({"id": "5", "channel_id": "100", "author": {"id": me, "username": "me"},
                               "type": 0, "content": "x", "timestamp": "2026-10-01T12:00:00Z"});
            for (k, val) in extra.as_object().unwrap() {
                v[k] = val.clone();
            }
            serde_json::from_value(v).unwrap()
        };
        assert!(own(json!({})).can_edit(me));
        assert!(!own(json!({"type": 6})).can_edit(me), "a pin notice");
        assert!(
            !own(json!({"message_reference": {"type": 1, "message_id": "1", "channel_id": "2"}}))
                .can_edit(me),
            "a forward"
        );
    }

    #[test]
    fn row_meta_reads_the_message() {
        let me = UserId(10);
        let m: p::Message = serde_json::from_value(json!({
            "id": "5", "channel_id": "100", "author": {"id": "10", "username": "me"},
            "type": 0, "content": "x", "timestamp": "2026-10-01T12:00:00Z",
            "pinned": true, "flags": 4,
            "reactions": [{"emoji": {"name": "\u{1F44D}"}, "count": 2, "me": true}]
        }))
        .unwrap();
        let meta = RowMeta::of(me, &m);
        assert!(meta.own && meta.pinned && meta.embeds_hidden);
        assert_eq!(meta.reactions.len(), 1);
    }
}
