// SPDX-License-Identifier: AGPL-3.0-or-later
//! Social: presence and status (`presence`), the friends page (`friends`, `pending`,
//! `blocked`), adding a friend (`add_friend`), group DMs (`group_dm`) and personal notes
//! (`notes`). The DM list itself is in `ui/sidebar/dm_list.rs`.
//!
//! The add-friend window, the group DM window, the DM menu and the confirmations are
//! layers of their own (`desktop::Layer::{AddFriend, GroupDm, DmMenu, SocialConfirm}`):
//! the topmost has the keyboard, Escape closes it, and typing never reaches the
//! composer behind it.

pub mod add_friend;
pub mod blocked;
pub mod friends;
pub mod group_dm;
pub mod notes;
pub mod pending;
pub mod presence;

use std::collections::HashSet;

use gpui::{
    div, prelude::*, px, AnyElement, Context, FontWeight, Hsla, Pixels, Point, SharedString,
    Subscription, Svg, Window,
};
use inlet_state::{AccountState, ChannelId, UserId};

use crate::app::{Inlet, Screen};
use crate::backend;
use crate::backend::social::presence::Ev as PresenceEv;
use crate::backend::social::relationships::{self as rel, GroupDm};
use crate::theme;
use crate::ui::overlays::status_picker::Picker;

/// This domain's view state.
#[derive(Default)]
pub struct State {
    pub presence: presence::PresenceState,
    /// The status picker over the user panel, while it is open.
    pub picker: Option<Picker>,
    /// The friends page: its tab and search.
    pub friends: friends::FriendsPage,
    /// Calls running now (see `relationships::Cmd::key`): their buttons wait.
    pub busy: HashSet<String>,
    /// The last failure of a friend or DM action, until it is dismissed.
    pub notice: Option<String>,
    /// The add-friend window.
    pub add_friend: Option<add_friend::AddFriend>,
    /// The group DM window (new group, add people, group settings).
    pub group_dm: Option<group_dm::GroupDmWindow>,
    /// The menu of a DM row (right click).
    pub dm_menu: Option<DmMenu>,
    /// A question before a lasting action (remove a friend, block, leave a group).
    pub confirm: Option<Confirm>,
    /// DMs this client is closing: their deletion is not reported as a lost channel.
    pub closing: HashSet<String>,
    /// The DM last open in the DM list, which the DM list opens again (Fluxer's home
    /// button), until the Friends row is picked.
    pub last_dm: Option<String>,
    /// The DM list's rows, until something they show changes (see `dm_rows_changed`).
    pub dm_rows: Option<Vec<crate::ui::sidebar::dm_list::DmRow>>,
    /// Input in the window ends "away" at once.
    _input: Option<Subscription>,
}

/// The menu of a DM row.
#[derive(Clone, Debug, PartialEq)]
pub struct DmMenu {
    pub channel: ChannelId,
    pub at: Point<Pixels>,
}

/// A question before a lasting action.
#[derive(Clone, Debug, PartialEq)]
pub enum Confirm {
    RemoveFriend { user: UserId, name: String },
    Block { user: UserId, name: String },
    LeaveGroup { channel: ChannelId, name: String },
}

impl State {
    /// The state with the idle watch running.
    pub fn new(cx: &mut Context<Inlet>) -> Self {
        let mut state = State::default();
        state.presence.start(cx);
        state._input = Some(cx.observe_keystrokes(|this, _, _, cx| {
            if this.social.presence.idle.input() {
                this.sync_presence(cx);
                cx.notify();
            }
        }));
        state
    }

    /// Sign-out or a backend restart: the picker and every social window close, and the
    /// next session sends its presence again.
    pub fn reset(&mut self) {
        self.picker = None;
        self.presence.reset();
        self.close_windows();
        self.friends = Default::default();
    }

    /// Another account is shown: the friend and DM windows, their waiting calls and the
    /// last failure were the old account's.
    pub fn close_windows(&mut self) {
        self.closing.clear();
        self.last_dm = None;
        self.dm_rows = None;
        self.busy.clear();
        self.notice = None;
        self.add_friend = None;
        self.group_dm = None;
        self.dm_menu = None;
        self.confirm = None;
    }
}

/// Phosphor icons (MIT) of this domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocialIcon {
    Friends,
    Notes,
    Message,
    Plus,
    Close,
    Check,
    UserMinus,
    Block,
    Pin,
    Pencil,
    Leave,
}

impl SocialIcon {
    fn data(self) -> &'static [u8] {
        match self {
            SocialIcon::Friends => include_bytes!("../../../assets/icons/users-three.svg"),
            SocialIcon::Notes => include_bytes!("../../../assets/icons/notebook.svg"),
            SocialIcon::Message => include_bytes!("../../../assets/icons/chat-circle.svg"),
            SocialIcon::Plus => include_bytes!("../../../assets/icons/plus.svg"),
            SocialIcon::Close => include_bytes!("../../../assets/icons/x.svg"),
            SocialIcon::Check => include_bytes!("../../../assets/icons/check.svg"),
            SocialIcon::UserMinus => include_bytes!("../../../assets/icons/user-minus.svg"),
            SocialIcon::Block => include_bytes!("../../../assets/icons/prohibit.svg"),
            SocialIcon::Pin => include_bytes!("../../../assets/icons/push-pin.svg"),
            SocialIcon::Pencil => include_bytes!("../../../assets/icons/pencil-simple.svg"),
            SocialIcon::Leave => include_bytes!("../../../assets/icons/sign-out.svg"),
        }
    }

    /// The icon, `size` square, in `color`.
    pub fn el(self, size: f32, color: Hsla) -> Svg {
        gpui::svg()
            .data(self.data())
            .size(px(size))
            .flex_none()
            .text_color(color)
    }
}

/// How a person is named in the friends list and the DM list: the friend nickname,
/// then the display name, then the username; cleaned.
pub fn person_name(state: &AccountState, user: UserId) -> String {
    let p = state.relationships().get(user);
    if let Some(nick) = p.and_then(|p| p.nickname.as_deref()) {
        return crate::model::clean_name(nick);
    }
    let u = p.and_then(|p| p.user.as_ref()).or_else(|| state.user(user));
    match u {
        Some(u) => crate::model::clean_name(u.display_name()),
        None => user.to_string(),
    }
}

/// The username (`name` or `name#0001`) under a person's name.
pub fn person_tag(state: &AccountState, user: UserId) -> String {
    let u = state
        .relationships()
        .get(user)
        .and_then(|p| p.user.as_ref())
        .or_else(|| state.user(user));
    match u {
        Some(u) if !u.discriminator.is_empty() && u.discriminator != "0" => format!(
            "{}#{}",
            crate::model::clean_name(&u.username),
            crate::model::clean_name(&u.discriminator)
        ),
        Some(u) => crate::model::clean_name(&u.username),
        None => String::new(),
    }
}

/// The user record of a person, from the relationship or the user cache.
pub fn person_user(state: &AccountState, user: UserId) -> Option<inlet_protocol::User> {
    state
        .relationships()
        .get(user)
        .and_then(|p| p.user.clone())
        .or_else(|| state.user(user).cloned())
}

/// A person's shown status (`online`, `idle`, `dnd`, `offline`).
pub fn person_status(state: &AccountState, user: UserId) -> &'static str {
    match state.presences().status(user) {
        "online" => "online",
        "idle" => "idle",
        "dnd" => "dnd",
        _ => "offline",
    }
}

/// Fluxer's BOT and SYSTEM tags, for a user that is one.
pub fn user_tag(user: &inlet_protocol::User) -> Option<AnyElement> {
    let label = if user.system {
        t!("message-system-tag")
    } else if user.bot {
        t!("message-bot-tag")
    } else {
        return None;
    };
    Some(
        div()
            .flex_none()
            .px(px(5.6))
            .min_h(px(15.))
            .flex()
            .items_center()
            .rounded(px(4.))
            .bg(theme::brand())
            .text_size(px(10.))
            .line_height(px(15.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme::on_brand())
            .child(label)
            .into_any_element(),
    )
}

/// A round 36 px action button (Fluxer's friend row buttons).
pub fn round_action(
    id: impl Into<SharedString>,
    icon: SocialIcon,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
    let color = if danger {
        theme::danger()
    } else {
        theme::text_muted()
    };
    div()
        .id(gpui::ElementId::Name(id.into()))
        .size(px(36.))
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(theme::bg_app())
        .hover(|s| s.bg(theme::hover_bg()))
        .child(icon.el(20., color))
}

impl Inlet {
    /// Sends a friend or DM action and marks its buttons busy until it answers. A
    /// second click while it runs sends nothing.
    pub(crate) fn social_action(&mut self, cmd: rel::Cmd, cx: &mut Context<Self>) {
        let key = cmd.key();
        if !self.social.busy.insert(key) {
            return;
        }
        self.backend.send(cmd);
        cx.notify();
    }

    /// The relationship items of the user menu for `user`, as Fluxer's: message, then
    /// add, accept, cancel or remove the friend, then block or unblock. None for the
    /// account itself.
    pub(crate) fn relationship_actions(
        &self,
        user: UserId,
        cx: &gpui::App,
    ) -> Vec<(&'static str, String, RelationshipAction)> {
        use inlet_state::relationships::Kind;
        let Some(state) = self.account.as_ref().map(|a| a.read(cx)) else {
            return Vec::new();
        };
        if user == state.me_id() {
            return Vec::new();
        }
        let kind = state.relationships().kind(user);
        let mut out = Vec::new();
        if kind != Some(Kind::Blocked) {
            out.push((
                "user-menu-message",
                t!("social-message"),
                RelationshipAction::Message,
            ));
        }
        match kind {
            None => out.push((
                "user-menu-add-friend",
                t!("social-add-friend"),
                RelationshipAction::AddFriend,
            )),
            Some(Kind::Incoming) => out.push((
                "user-menu-accept",
                t!("social-accept-request"),
                RelationshipAction::Accept,
            )),
            Some(Kind::Outgoing) => out.push((
                "user-menu-cancel",
                t!("social-cancel-request"),
                RelationshipAction::CancelRequest,
            )),
            Some(Kind::Friend) => out.push((
                "user-menu-remove-friend",
                t!("social-remove-friend"),
                RelationshipAction::RemoveFriend,
            )),
            Some(Kind::Blocked) => {}
        }
        out.push(if kind == Some(Kind::Blocked) {
            (
                "user-menu-unblock",
                t!("social-unblock"),
                RelationshipAction::Unblock,
            )
        } else {
            (
                "user-menu-block",
                t!("social-block"),
                RelationshipAction::Block,
            )
        });
        out
    }

    /// Runs a relationship item of the user menu; removing a friend and blocking ask
    /// first.
    pub(crate) fn run_relationship_action(
        &mut self,
        action: RelationshipAction,
        user: UserId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self
            .account
            .as_ref()
            .map(|a| person_name(a.read(cx), user))
            .unwrap_or_default();
        let user_id = user.to_string();
        match action {
            RelationshipAction::Message => self.open_dm_with(user, window, cx),
            RelationshipAction::AddFriend => {
                self.social_action(rel::Cmd::SendRequest { user_id }, cx)
            }
            RelationshipAction::Accept => self.social_action(rel::Cmd::Accept { user_id }, cx),
            RelationshipAction::CancelRequest | RelationshipAction::Unblock => {
                self.social_action(rel::Cmd::Remove { user_id }, cx)
            }
            RelationshipAction::RemoveFriend => {
                self.social.confirm = Some(Confirm::RemoveFriend { user, name })
            }
            RelationshipAction::Block => self.social.confirm = Some(Confirm::Block { user, name }),
        }
        cx.notify();
    }

    /// The channel as the account state knows it (the server's own type and data).
    pub(crate) fn state_channel(
        &self,
        channel: ChannelId,
        cx: &gpui::App,
    ) -> Option<Box<inlet_protocol::Channel>> {
        let state = self.account.as_ref()?.read(cx);
        state.channel(channel).cloned().map(Box::new)
    }

    /// Closes a DM or leaves a group DM. Nothing is sent for a channel the state does
    /// not know; the protocol refuses anything but a DM or group DM.
    pub(crate) fn close_dm(&mut self, channel: ChannelId, cx: &mut Context<Self>) {
        let Some(ch) = self.state_channel(channel, cx) else {
            return;
        };
        // Its CHANNEL_DELETE closes it quietly (no "was deleted" line).
        self.social.closing.insert(channel.to_string());
        self.social_action(rel::Cmd::CloseDm { channel: ch }, cx);
    }

    /// Renames a group DM or changes its icon.
    pub(crate) fn edit_group_dm(
        &mut self,
        channel: ChannelId,
        name: Option<Option<String>>,
        icon: Option<rel::Icon>,
        cx: &mut Context<Self>,
    ) {
        let Some(ch) = self.state_channel(channel, cx) else {
            return;
        };
        self.social_action(
            rel::Cmd::EditGroupDm {
                channel: ch,
                name,
                icon,
            },
            cx,
        );
    }

    /// Whether the call named `key` runs.
    pub(crate) fn social_busy(&self, key: &str) -> bool {
        self.social.busy.contains(key)
    }

    /// The DM list's rows are worked out again when channels, read states (a new message
    /// moves a DM up), relationships, presences or users change.
    pub(crate) fn dm_rows_changed(&mut self, changes: &inlet_state::Changes) {
        if changes.reset
            || !changes.channels.is_empty()
            || changes.read_state
            || changes.relationships
            || changes.presence
            || changes.users
        {
            self.social.dm_rows = None;
        }
    }

    /// Whether the friends page fills the middle: the DM list is up and no DM is open.
    pub(crate) fn friends_page_shown(&self) -> bool {
        self.screen == Screen::Main
            && self.selected_guild.is_none()
            && self.selected_channel.is_none()
    }

    /// The Friends row: the friends page shows, and the open DM closes.
    pub(crate) fn show_friends(&mut self, cx: &mut Context<Self>) {
        self.social.last_dm = None;
        if self.selected_guild.is_some() {
            self.select_guild(None, cx);
            return;
        }
        if self.selected_channel.take().is_some() {
            self.threads_leave_channel(None, cx);
            self.clear_messages();
        }
        cx.notify();
    }

    /// Once per frame: the keyboard never stays in the composer while the friends page
    /// hides it; and the demo scenes open their windows.
    pub(crate) fn social_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::Focusable;
        if self.friends_page_shown()
            && !self.layer_open()
            && self.composer.view.focus_handle(cx).is_focused(window)
        {
            window.focus(&self.focus, cx);
        }
        #[cfg(feature = "demo")]
        self.demo_social(window, cx);
    }

    /// Demo screenshots (CI): the social scenes set their state once the DM list is in.
    #[cfg(feature = "demo")]
    fn demo_social(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::demo::scenes::{current, Scene};
        use std::sync::atomic::{AtomicBool, Ordering};
        static DONE: AtomicBool = AtomicBool::new(false);
        let scene = current();
        if std::env::var_os("INLET_DEMO").is_none()
            || !matches!(
                scene,
                Scene::FriendsPending | Scene::GroupDmCaptcha | Scene::DmBlocked
            )
            || self.account.is_none()
            || self.screen != Screen::Main
            || self.selected_guild.is_some()
            || self.channels.is_empty()
            || DONE.swap(true, Ordering::Relaxed)
        {
            return;
        }
        match scene {
            Scene::FriendsPending => self.social.friends.tab = friends::Tab::Pending,
            Scene::GroupDmCaptcha => {
                self.open_group_dm(group_dm::Mode::Create, window, cx);
                let picked: Vec<UserId> = ["ada", "grace"]
                    .iter()
                    .filter_map(|n| UserId::parse(&crate::demo::user_id(n)))
                    .collect();
                if let Some(w) = self.social.group_dm.as_mut() {
                    w.picked = picked;
                    w.message = Some(t!("social-group-dm-captcha"));
                }
            }
            _ => self.select_channel(crate::demo::snowflake("dm-group"), window, cx),
        }
        cx.notify();
    }

    /// Events from the backend's social domain.
    pub(crate) fn on_social_event(
        &mut self,
        event: backend::social::Ev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            backend::social::Ev::Presence(PresenceEv::SaveFailed) => self.status_save_failed(cx),
            backend::social::Ev::Presence(PresenceEv::Saved(settings)) => {
                self.status_saved(&settings, cx)
            }
            backend::social::Ev::Relationships(ev) => self.on_relationships_event(ev, window, cx),
            backend::social::Ev::Members(ev) => self.on_members_event(ev, cx),
            backend::social::Ev::Profile(ev) => self.on_profile_event(ev, cx),
        }
    }

    fn on_relationships_event(&mut self, ev: rel::Ev, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            rel::Ev::Done { key } => {
                self.social.busy.remove(&key);
                if key == "add-friend" {
                    self.add_friend_sent(cx);
                }
            }
            rel::Ev::Failed { key, reason } => {
                self.social.busy.remove(&key);
                if key == "add-friend" && self.social.add_friend.is_some() {
                    self.add_friend_failed(reason);
                } else if let Some(w) = self.social.group_dm.as_mut().filter(|w| w.key() == key) {
                    w.message = Some(reason);
                } else if self.friends_page_shown() {
                    self.social.notice = Some(reason);
                } else {
                    // A DM is open: the line over the chat says it.
                    self.chat.notice = Some(reason);
                }
            }
            rel::Ev::GroupDm { key, result } => {
                self.social.busy.remove(&key);
                self.group_dm_answer(&key, result, cx);
            }
            rel::Ev::DmOpened { key, channel } => {
                self.social.busy.remove(&key);
                if self
                    .social
                    .group_dm
                    .as_ref()
                    .is_some_and(|w| w.key() == key)
                {
                    self.social.group_dm = None;
                }
                self.open_dm_channel(&channel, window, cx);
            }
        }
        cx.notify();
    }

    /// Opens the DM with `user`: the one the account state has, or a new one (Fluxer
    /// sends nothing when the DM is already there).
    pub(crate) fn open_dm_with(
        &mut self,
        user: UserId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self.account.as_ref().and_then(|a| {
            a.read(cx)
                .dm_list()
                .into_iter()
                .find(|c| {
                    c.kind == inlet_protocol::models::channel_type::DM
                        && c.recipients.first().and_then(|u| UserId::parse(&u.id)) == Some(user)
                })
                .cloned()
        });
        match existing {
            Some(channel) => self.open_dm_channel(&channel, window, cx),
            None => self.social_action(
                rel::Cmd::OpenDm {
                    user_id: user.to_string(),
                },
                cx,
            ),
        }
    }

    /// Shows a DM the backend just opened or created: the state and the DM list learn
    /// it now (the gateway's CHANNEL_CREATE comes too, and changes nothing more).
    pub(crate) fn open_dm_channel(
        &mut self,
        channel: &inlet_protocol::Channel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(account) = self.account.clone() {
            let create = inlet_protocol::Dispatch::ChannelCreate(Box::new(channel.clone()));
            account.update(cx, |s, _| s.apply(&create));
        }
        let shown = backend::messages::parse::to_channel(channel);
        self.social.dm_rows = None;
        if self.selected_guild.is_none() {
            if !self.channels.iter().any(|c| c.id == shown.id) {
                self.channels.push(shown.clone());
            }
            self.select_channel(shown.id, window, cx);
        } else {
            self.follow_channel_link(None, shown.id, window, cx);
        }
    }

    /// A group DM call answered: a lasting line in its window, never a new try.
    fn group_dm_answer(&mut self, key: &str, result: GroupDm, cx: &mut Context<Self>) {
        let text = match &result {
            GroupDm::Done => {
                if self
                    .social
                    .group_dm
                    .as_ref()
                    .is_some_and(|w| w.key() == key)
                {
                    self.group_dm_added(cx);
                }
                return;
            }
            GroupDm::Captcha => t!("social-group-dm-captcha"),
            GroupDm::RateLimited { retry_after } => t!(
                "social-group-dm-rate-limited",
                wait = rel::wait_text(*retry_after)
            ),
            GroupDm::NotAddable(list) => {
                let names: Vec<String> = {
                    let state = self.account.as_ref().map(|a| a.read(cx));
                    list.iter()
                        .map(|(id, _)| match (state, UserId::parse(id)) {
                            (Some(s), Some(u)) => person_name(s, u),
                            _ => id.clone(),
                        })
                        .collect()
                };
                t!("social-group-dm-not-addable", names = names.join(", "))
            }
            GroupDm::Failed(reason) => reason.clone(),
        };
        match self.social.group_dm.as_mut().filter(|w| w.key() == key) {
            Some(w) => w.message = Some(text),
            None => self.social.notice = Some(text),
        }
    }

    /// The social windows, bottom first: add friend, group DM, DM menu, confirmation.
    pub(crate) fn render_social_overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if self.screen != Screen::Main {
            return out;
        }
        // The DM menu is drawn deferred: it goes when a link prompt opens over it.
        if self.overlays.pending_link.is_some() || self.overlays.link_prompt.is_some() {
            self.social.dm_menu = None;
        }
        if self.social.add_friend.is_some() {
            out.push(self.render_add_friend(cx));
        }
        if self.social.group_dm.is_some() {
            out.push(self.render_group_dm(cx));
        }
        if let Some(menu) = self.social.dm_menu.clone() {
            out.push(self.render_dm_menu(&menu, cx));
        }
        if let Some(confirm) = self.social.confirm.clone() {
            out.push(self.render_social_confirm(&confirm, cx));
        }
        out
    }

    /// Whether `part` is the topmost layer (it then has the keyboard and Escape).
    fn social_top(&self, part: SocialPart) -> bool {
        use crate::desktop::Layer;
        self.is_top_layer(match part {
            SocialPart::Confirm => Layer::SocialConfirm,
            SocialPart::Menu => Layer::DmMenu,
            SocialPart::GroupDm => Layer::GroupDm,
            SocialPart::AddFriend => Layer::AddFriend,
        })
    }

    /// The social window whose text field has the keyboard now, if one has.
    pub(crate) fn social_field_focused(
        &self,
        window: &Window,
        cx: &gpui::App,
    ) -> Option<crate::desktop::Layer> {
        use crate::desktop::Layer;
        use gpui::Focusable;
        if let Some(w) = &self.social.group_dm {
            let name = w
                .name
                .as_ref()
                .is_some_and(|n| n.focus_handle(cx).is_focused(window));
            if name || w.search.focus_handle(cx).is_focused(window) {
                return Some(Layer::GroupDm);
            }
        }
        self.social
            .add_friend
            .as_ref()
            .filter(|w| w.input.focus_handle(cx).is_focused(window))
            .map(|_| Layer::AddFriend)
    }

    /// The layer's keys on the DM menu while it is the topmost social window.
    pub(crate) fn social_menu_keys<E: InteractiveElement>(
        &self,
        el: E,
        cx: &mut Context<Self>,
    ) -> E {
        self.social_keys(SocialPart::Menu, el, cx)
    }

    /// The field the topmost social window's keyboard goes to, when it has one.
    pub(crate) fn social_field(&self, cx: &gpui::App) -> Option<gpui::FocusHandle> {
        use gpui::Focusable;
        let s = &self.social;
        if s.confirm.is_some() || s.dm_menu.is_some() {
            return None;
        }
        if s.group_dm.is_some() {
            return self.group_dm_field(cx);
        }
        s.add_friend.as_ref().map(|w| w.input.focus_handle(cx))
    }

    /// The layer's keys on the topmost social window.
    fn social_keys<E: InteractiveElement>(
        &self,
        part: SocialPart,
        el: E,
        cx: &mut Context<Self>,
    ) -> E {
        if self.social_top(part) {
            self.layer_keys(el, cx)
        } else {
            el
        }
    }

    fn render_social_confirm(&mut self, confirm: &Confirm, cx: &mut Context<Self>) -> AnyElement {
        use crate::ui::overlays::modal::modal;
        use crate::ui::widgets::button;
        let (title, body, label) = match confirm {
            Confirm::RemoveFriend { name, .. } => (
                t!("social-remove-friend-title", name = name.clone()),
                t!("social-remove-friend-body", name = name.clone()),
                t!("social-remove-friend"),
            ),
            Confirm::Block { name, .. } => (
                t!("social-block-title", name = name.clone()),
                t!("social-block-body", name = name.clone()),
                t!("social-block"),
            ),
            Confirm::LeaveGroup { name, .. } => (
                t!("social-leave-group-title", name = name.clone()),
                t!("social-leave-group-body", name = name.clone()),
                t!("social-leave-group"),
            ),
        };
        let action = confirm.clone();
        let card = modal(
            "social-confirm",
            440.,
            [
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_strong())
                    .child(title)
                    .into_any_element(),
                div()
                    .text_color(theme::text_muted())
                    .child(body)
                    .into_any_element(),
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("social-confirm-cancel", t!("common-cancel"), false).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.social.confirm = None;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        button("social-confirm-ok", label, true)
                            .bg(theme::danger())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.social.confirm = None;
                                match &action {
                                    Confirm::RemoveFriend { user, .. } => this.social_action(
                                        rel::Cmd::Remove {
                                            user_id: user.to_string(),
                                        },
                                        cx,
                                    ),
                                    Confirm::Block { user, .. } => this.social_action(
                                        rel::Cmd::Block {
                                            user_id: user.to_string(),
                                        },
                                        cx,
                                    ),
                                    Confirm::LeaveGroup { channel, .. } => {
                                        this.close_dm(*channel, cx)
                                    }
                                }
                            })),
                    )
                    .into_any_element(),
            ],
        );
        self.social_keys(SocialPart::Confirm, card, cx)
            .into_any_element()
    }
}

/// What the user menu offers about a person (see `relationship_actions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelationshipAction {
    Message,
    AddFriend,
    Accept,
    RemoveFriend,
    CancelRequest,
    Block,
    Unblock,
}

/// The social windows, for the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SocialPart {
    AddFriend,
    GroupDm,
    Menu,
    Confirm,
}
