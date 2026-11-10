//! [`AccountState`]: everything one account knows, and how events change it.

use std::collections::{BTreeMap, BTreeSet};

use inlet_protocol::models::{
    parse_timestamp_ms, Channel, GuildMember, GuildMemberListUpdate, GuildReady, MemberListItem,
    Message, Presence, Ready, User,
};
use inlet_protocol::{CurrentUser, Dispatch};

use crate::channels::{has_read_state, is_guild_text_based, is_private, Channels};
use crate::guild_settings::{notifications, GuildSettings};
use crate::guilds::GuildState;
use crate::ids::{ChannelId, GuildId, MessageId, RoleId, UserId};
use crate::limits::{
    MAX_CHANNELS_PER_GUILD, MAX_EXPRESSIONS_PER_GUILD, MAX_GUILDS, MAX_MEMBERS_TOTAL,
    MAX_PRIVATE_CHANNELS,
};
use crate::messages::{MessageWindow, MessageWindows, MessagesPage};
use crate::permissions::{self, Permissions};
use crate::presence::Presences;
use crate::read_state::{AckKind, ReadStateEntry, ReadStates, Unread};
use crate::relationships::Relationships;
use crate::settings::Settings;
use crate::size;
use crate::threads::Threads;
use crate::typing::Typing;
use crate::users::Users;
use crate::voice::Voice;
use crate::Changes;

/// How a member payload is stored.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MemberSource {
    /// A member event or list: replaces the cached member.
    Authoritative,
    /// The `member` on a message or typing event: fills in only a missing
    /// member, because it may be older than the cached one.
    Attached,
}

/// The state of one signed-in account.
///
/// Feed it with [`AccountState::apply`] (gateway), [`AccountState::begin_load`]
/// and [`AccountState::apply_messages`] (REST pages), [`AccountState::mark_read`]
/// (acks) and [`AccountState::tick`] (time). Every update returns [`Changes`].
///
/// Equality compares the contents, not the recency order of the caches.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountState {
    session_id: String,
    me: CurrentUser,
    me_id: UserId,
    guilds: BTreeMap<GuildId, GuildState>,
    channels: Channels,
    users: Users,
    presences: Presences,
    read_states: ReadStates,
    guild_settings: GuildSettings,
    windows: MessageWindows,
    typing: Typing,
    relationships: Relationships,
    threads: Threads,
    settings: Settings,
    voice: Voice,
    now_ms: i64,
}

impl AccountState {
    /// The state from a READY event. The clock starts at 0, so timeouts and
    /// timed mutes count as running until the first [`AccountState::tick`]
    /// (fail closed); prefer [`AccountState::from_ready_at`].
    pub fn from_ready(r: &Ready) -> Self {
        Self::from_ready_at(r, 0)
    }

    /// The state from a READY event at `now_ms` (Unix milliseconds).
    pub fn from_ready_at(r: &Ready, now_ms: i64) -> Self {
        let me_id = UserId::parse(&r.user.id).unwrap_or(UserId(0));
        let mut me = r.user.clone();
        size::cap_user(&mut me.user);
        let mut s = AccountState {
            session_id: r.session_id.clone(),
            me,
            me_id,
            guilds: BTreeMap::new(),
            channels: Channels::default(),
            users: Users::default(),
            presences: Presences::default(),
            read_states: ReadStates::from_ready(&r.parsed_read_states()),
            guild_settings: GuildSettings::from_ready(&r.user_guild_settings()),
            windows: MessageWindows::default(),
            typing: Typing::default(),
            relationships: Relationships::default(),
            threads: Threads::default(),
            settings: Settings::default(),
            voice: Voice::default(),
            now_ms,
        };
        s.users.upsert(&r.user.user);
        for u in &r.users {
            s.users.upsert(u);
        }
        let mut sink = Changes::default();
        for g in &r.guilds {
            s.ingest_guild(g, &mut sink);
        }
        for c in &r.private_channels {
            s.upsert_channel(c, &mut sink);
        }
        for p in r.presences() {
            s.presences.upsert(&p);
        }
        s
    }

    // ----- updates -----

    /// Applies one gateway event. Unknown events, and events for guilds or
    /// channels the state does not know, give an empty [`Changes`].
    pub fn apply(&mut self, d: &Dispatch) -> Changes {
        let mut c = Changes::default();
        match d {
            Dispatch::Ready(r) => {
                let windows = std::mem::take(&mut self.windows);
                *self = AccountState::from_ready_at(r, self.now_ms);
                self.windows = windows;
                let channels = &self.channels;
                self.windows.retain(|ch| channels.get(ch).is_some());
                self.windows.mark_stale();
                c.reset = true;
            }
            Dispatch::Resumed => {}
            Dispatch::GuildCreate(g) | Dispatch::GuildSync(g) => self.ingest_guild(g, &mut c),
            Dispatch::GuildDelete(g) => {
                if let Some(id) = GuildId::parse(&g.id) {
                    if g.unavailable {
                        if let Some(guild) = self.guilds.get_mut(&id) {
                            if !guild.unavailable {
                                guild.unavailable = true;
                                c.guilds = true;
                            }
                        }
                    } else {
                        self.remove_guild(id, &mut c);
                    }
                }
            }
            Dispatch::ChannelCreate(ch) | Dispatch::ChannelUpdate(ch) => {
                self.upsert_channel(ch, &mut c);
                self.recompute_for_channel(ch, &mut c);
            }
            Dispatch::ChannelUpdateBulk(b) => {
                let mut guilds = BTreeSet::new();
                for ch in &b.channels {
                    let mut ch = ch.clone();
                    if ch.guild_id.is_none() {
                        ch.guild_id.clone_from(&b.guild_id);
                    }
                    self.upsert_channel(&ch, &mut c);
                    if let Some(g) =
                        ChannelId::parse(&ch.id).and_then(|id| self.channels.guild_of(id))
                    {
                        guilds.insert(g);
                    }
                }
                for g in guilds {
                    self.recompute(g, &mut c);
                }
            }
            Dispatch::ChannelDelete(ch) => {
                if let Some(id) = ChannelId::parse(&ch.id) {
                    self.delete_channel(id, &mut c);
                }
            }
            Dispatch::ChannelPinsUpdate(p) => {
                if let Some(ch) = ChannelId::parse(&p.channel_id) {
                    let guild = self.channels.guild_of(ch);
                    if let Some(channel) = self.channels.get_mut(ch) {
                        let mut ts = p.last_pin_timestamp.clone();
                        if let Some(t) = &mut ts {
                            size::truncate(t, 64);
                        }
                        channel.extra.insert(
                            "last_pin_timestamp".into(),
                            ts.map_or(serde_json::Value::Null, Into::into),
                        );
                        let ms = p
                            .last_pin_timestamp
                            .as_deref()
                            .and_then(parse_timestamp_ms)
                            .unwrap_or(0);
                        if self.read_states.pins_updated(ch, ms) {
                            c.read(ch, guild);
                        }
                        c.channel(ch);
                    }
                }
            }
            Dispatch::MessageCreate(m) => self.message_create(m, &mut c),
            Dispatch::MessageUpdate(m) => {
                if let (Some(ch), Some(id)) =
                    (ChannelId::parse(&m.channel_id), MessageId::parse(&m.id))
                {
                    c.users |= self.users.upsert(&m.author);
                    if let (Some(g), Some(member)) = (self.channels.guild_of(ch), m.member()) {
                        self.attached_member(g, member, &m.author, &mut c);
                    }
                    if self.windows.update(ch, id, m) {
                        c.message(ch);
                    }
                }
            }
            Dispatch::MessageDelete(m) => {
                if let (Some(ch), Some(id)) =
                    (ChannelId::parse(&m.channel_id), MessageId::parse(&m.id))
                {
                    if self.windows.delete(ch, id) {
                        c.message(ch);
                    }
                }
            }
            Dispatch::MessageDeleteBulk(b) => {
                if let Some(ch) = ChannelId::parse(&b.channel_id) {
                    for id in b.ids.iter().filter_map(|i| MessageId::parse(i)) {
                        if self.windows.delete(ch, id) {
                            c.message(ch);
                        }
                    }
                }
            }
            Dispatch::TypingStart(t) => {
                let (Some(ch), Some(user)) =
                    (ChannelId::parse(&t.channel_id), UserId::parse(&t.user_id))
                else {
                    return c;
                };
                if user == self.me_id || self.channels.get(ch).is_none() {
                    return c;
                }
                if let (Some(g), Some(raw)) = (self.channels.guild_of(ch), t.member.as_ref()) {
                    if let Ok(member) = serde_json::from_value::<GuildMember>(raw.clone()) {
                        if member.user_id().and_then(UserId::parse) == Some(user) {
                            self.upsert_member(g, &member, MemberSource::Attached, &mut c);
                        }
                    }
                }
                let (changed, evicted) = self.typing.start(ch, user, self.now_ms);
                if changed {
                    c.typing(ch);
                }
                if let Some(e) = evicted {
                    c.typing(e);
                }
            }
            Dispatch::MessageAck(a) => {
                if let (Some(ch), Some(id)) = (
                    ChannelId::parse(&a.channel_id),
                    MessageId::parse(&a.message_id),
                ) {
                    if !self.untracked(ch)
                        && self
                            .read_states
                            .server_ack(ch, id, a.mention_count, a.manual)
                    {
                        c.read(ch, self.channels.guild_of(ch));
                    }
                }
            }
            Dispatch::MessageReactionAdd(r) | Dispatch::MessageReactionRemove(r) => {
                let add = matches!(d, Dispatch::MessageReactionAdd(_));
                if let (Some(ch), Some(id), Some(user)) = (
                    ChannelId::parse(&r.channel_id),
                    MessageId::parse(&r.message_id),
                    UserId::parse(&r.user_id),
                ) {
                    let me = user == self.me_id;
                    let changed = if add {
                        self.windows.add_reaction(ch, id, &r.emoji, me)
                    } else {
                        self.windows.remove_reaction(ch, id, &r.emoji, me)
                    };
                    if changed {
                        c.message(ch);
                    }
                }
            }
            Dispatch::MessageReactionAddMany(r) => {
                if let (Some(ch), Some(id)) = (
                    ChannelId::parse(&r.channel_id),
                    MessageId::parse(&r.message_id),
                ) {
                    for add in &r.reactions {
                        let me = UserId::parse(&add.user_id) == Some(self.me_id);
                        if self.windows.add_reaction(ch, id, &add.emoji, me) {
                            c.message(ch);
                        }
                    }
                }
            }
            Dispatch::MessageReactionRemoveAll(r) | Dispatch::MessageReactionRemoveEmoji(r) => {
                let emoji = match d {
                    Dispatch::MessageReactionRemoveEmoji(_) => r.emoji.as_ref(),
                    _ => None,
                };
                // REMOVE_EMOJI without an emoji is malformed: do not clear everything.
                let malformed =
                    matches!(d, Dispatch::MessageReactionRemoveEmoji(_)) && emoji.is_none();
                if let (false, Some(ch), Some(id)) = (
                    malformed,
                    ChannelId::parse(&r.channel_id),
                    MessageId::parse(&r.message_id),
                ) {
                    if self.windows.clear_reactions(ch, id, emoji) {
                        c.message(ch);
                    }
                }
            }
            Dispatch::GuildUpdate(g) => {
                if let Some(id) = GuildId::parse(&g.id) {
                    if let Some(guild) = self.guilds.get_mut(&id) {
                        let mut props = (**g).clone();
                        size::cap_guild(&mut props);
                        if guild.properties.as_ref() == Some(&props) {
                            return c;
                        }
                        guild.visiting = guild.visiting || props.visiting;
                        guild.properties = Some(props);
                        guild.refresh_cached();
                        c.guilds = true;
                        self.recompute(id, &mut c);
                    }
                }
            }
            Dispatch::GuildRoleCreate(r) | Dispatch::GuildRoleUpdate(r) => {
                if let Some(id) = GuildId::parse(&r.guild_id) {
                    if let Some(guild) = self.guilds.get_mut(&id) {
                        if guild.roles.upsert(r.role.clone()) {
                            c.guilds = true;
                            self.recompute(id, &mut c);
                        }
                    }
                }
            }
            Dispatch::GuildRoleUpdateBulk(r) => {
                if let Some(id) = GuildId::parse(&r.guild_id) {
                    if let Some(guild) = self.guilds.get_mut(&id) {
                        let mut changed = false;
                        for role in &r.roles {
                            changed |= guild.roles.upsert(role.clone());
                        }
                        if changed {
                            c.guilds = true;
                            self.recompute(id, &mut c);
                        }
                    }
                }
            }
            Dispatch::GuildRoleDelete(r) => {
                if let (Some(id), Some(role)) =
                    (GuildId::parse(&r.guild_id), RoleId::parse(&r.role_id))
                {
                    if let Some(guild) = self.guilds.get_mut(&id) {
                        if guild.roles.remove(role) {
                            c.guilds = true;
                            self.recompute(id, &mut c);
                        }
                    }
                }
            }
            Dispatch::GuildMemberAdd(m) | Dispatch::GuildMemberUpdate(m) => {
                if let Some(g) = m.guild_id().as_deref().and_then(GuildId::parse) {
                    self.upsert_member(g, m, MemberSource::Authoritative, &mut c);
                }
            }
            Dispatch::GuildMemberRemove(r) => {
                if let (Some(g), Some(user)) =
                    (GuildId::parse(&r.guild_id), UserId::parse(&r.user.id))
                {
                    let me = self.me_id;
                    if let Some(guild) = self.guilds.get_mut(&g) {
                        if guild.members.remove(user, me) {
                            c.members_of(g);
                        }
                        if user == me {
                            guild.left = true;
                            guild.refresh_cached();
                            self.recompute(g, &mut c);
                        }
                    }
                }
            }
            Dispatch::GuildMembersChunk(chunk) => {
                if let Some(g) = GuildId::parse(&chunk.guild_id) {
                    if self.guilds.contains_key(&g) {
                        for m in &chunk.members {
                            self.upsert_member(g, m, MemberSource::Authoritative, &mut c);
                        }
                        for p in &chunk.presences {
                            c.presence |= self.presences.upsert(p);
                        }
                    }
                }
            }
            Dispatch::GuildMemberListUpdate(u) => self.member_list(u, &mut c),
            Dispatch::GuildEmojisUpdate(e) => {
                if let Some(guild) =
                    GuildId::parse(&e.guild_id).and_then(|g| self.guilds.get_mut(&g))
                {
                    let emojis: Vec<_> = e
                        .emojis
                        .iter()
                        .take(MAX_EXPRESSIONS_PER_GUILD)
                        .cloned()
                        .map(|mut x| {
                            size::cap_emoji(&mut x);
                            x
                        })
                        .collect();
                    if guild.emojis != emojis {
                        guild.emojis = emojis;
                        c.guilds = true;
                    }
                }
            }
            Dispatch::GuildStickersUpdate(s) => {
                if let Some(guild) =
                    GuildId::parse(&s.guild_id).and_then(|g| self.guilds.get_mut(&g))
                {
                    let stickers: Vec<_> = s
                        .stickers
                        .iter()
                        .take(MAX_EXPRESSIONS_PER_GUILD)
                        .cloned()
                        .map(|mut x| {
                            size::cap_sticker(&mut x);
                            x
                        })
                        .collect();
                    if guild.stickers != stickers {
                        guild.stickers = stickers;
                        c.guilds = true;
                    }
                }
            }
            Dispatch::PresenceUpdate(p) => c.presence |= self.presences.upsert(p),
            Dispatch::PresenceUpdateBulk(b) => {
                for p in &b.presences {
                    c.presence |= self.presences.upsert(p);
                }
            }
            Dispatch::PassiveUpdates(p) => {
                if let Some(g) = GuildId::parse(&p.guild_id) {
                    for (ch, last) in p.last_message_ids() {
                        let (Some(ch), Some(last)) =
                            (ChannelId::parse(&ch), MessageId::parse(&last))
                        else {
                            continue;
                        };
                        if self.channels.guild_of(ch) != Some(g) {
                            continue;
                        }
                        self.bump_channel_last(ch, last);
                        if self.read_states.bump_last(ch, last) {
                            c.read(ch, Some(g));
                        }
                    }
                }
            }
            Dispatch::UserUpdate(u) => {
                let mut me = (**u).clone();
                size::cap_user(&mut me.user);
                if UserId::parse(&u.id) == Some(self.me_id) && self.me != me {
                    self.users.upsert(&me.user);
                    self.me = me;
                    c.users = true;
                    let ids: Vec<GuildId> = self.guilds.keys().copied().collect();
                    for g in ids {
                        self.recompute(g, &mut c);
                    }
                }
            }
            Dispatch::UserSettingsUpdate(_) | Dispatch::SessionsReplace(_) => {
                c.merge(self.settings.apply(d));
            }
            Dispatch::VoiceStateUpdate(_) => c.merge(self.voice.apply(d)),
            Dispatch::Other { name, data } if name == "USER_GUILD_SETTINGS_UPDATE" => {
                if self.guild_settings.update(data).is_some() {
                    c.settings = true;
                    c.read_all();
                }
            }
            Dispatch::Other { .. } => {
                c.merge(self.relationships.apply(d));
                c.merge(self.threads.apply(d));
            }
            _ => {}
        }
        c
    }

    /// Opens the channel's window before the app asks for its newest page.
    /// Until the page lands ([`AccountState::apply_messages`]) live messages
    /// are buffered and deletes remembered, so nothing that happens while the
    /// request is in flight is lost. An existing window is only touched.
    pub fn begin_load(&mut self, ch: ChannelId) -> Changes {
        let mut c = Changes::default();
        if self.channels.get(ch).is_none() {
            return c;
        }
        let existed = self.windows.get(ch).is_some();
        if let Some(evicted) = self.windows.begin_load(ch) {
            c.message(evicted);
        }
        if !existed {
            c.message(ch);
        }
        c
    }

    /// Merges a page of messages from REST into the channel's window. A page
    /// for a channel the state does not know (deleted meanwhile) is dropped.
    /// When [`MessageWindow::has_newest`] is still `false` after a newest
    /// page, too many live messages arrived meanwhile: load it again.
    pub fn apply_messages(&mut self, ch: ChannelId, page: MessagesPage) -> Changes {
        let mut c = Changes::default();
        if self.channels.get(ch).is_none() {
            return c;
        }
        let (changed, evicted) = self.windows.apply_page(ch, &page);
        if changed {
            c.message(ch);
        }
        if let Some(e) = evicted {
            c.message(e);
        }
        for m in &page.messages {
            c.users |= self.users.upsert(&m.author);
        }
        // Fluxer's handleLoadMessages: a newer message than the known newest moves it.
        let newest = page
            .messages
            .iter()
            .filter(|m| ChannelId::parse(&m.channel_id) == Some(ch))
            .filter_map(|m| MessageId::parse(&m.id))
            .max();
        if let Some(last) = newest {
            self.bump_channel_last(ch, last);
            if self.read_states.bump_last(ch, last) {
                c.read(ch, self.channels.guild_of(ch));
            }
        }
        c
    }

    /// The account read `ch` up to `message` (or up to the newest message),
    /// as Fluxer's `applyAck`: the mentions go to 0 and the ack never moves
    /// back. Use [`AckKind::Explicit`] after the user read the channel (it
    /// clears a "mark unread"), and [`AckKind::Automatic`] when the channel
    /// is open at the bottom while messages arrive (it does nothing while a
    /// "mark unread" holds). Send the same ack to the server.
    pub fn mark_read(
        &mut self,
        ch: ChannelId,
        message: Option<MessageId>,
        kind: AckKind,
    ) -> Changes {
        let mut c = Changes::default();
        if self.untracked(ch) {
            return c;
        }
        let Some(entry) = self.read_states.get(ch) else {
            return c;
        };
        let Some(target) = message.or(entry.last_message_id) else {
            return c;
        };
        if self.read_states.local_ack(ch, target, kind) {
            c.read(ch, self.channels.guild_of(ch));
        }
        c
    }

    /// The pins of `ch` were looked at.
    pub fn ack_pins(&mut self, ch: ChannelId) -> Changes {
        let mut c = Changes::default();
        if self.read_states.ack_pins(ch) {
            c.read(ch, self.channels.guild_of(ch));
        }
        c
    }

    /// Marks a channel's window as in use, so it is evicted last. Call it for
    /// the visible channel, or a burst of prefetches can evict it.
    pub fn touch_channel(&mut self, ch: ChannelId) {
        self.windows.touch(ch);
    }

    /// Drops every message window (for an account moved to the background).
    pub fn drop_message_windows(&mut self) -> Changes {
        let mut c = Changes::default();
        for ch in self.windows.channels().collect::<Vec<_>>() {
            c.message(ch);
        }
        self.windows.clear();
        c
    }

    /// Moves the clock to `now_ms`: typing entries expire, timeouts and timed
    /// mutes end. Call it about once a second.
    pub fn tick(&mut self, now_ms: i64) -> Changes {
        let mut c = Changes::default();
        let before = self.now_ms;
        self.now_ms = now_ms;
        for ch in self.typing.expire(now_ms) {
            c.typing(ch);
        }
        let flipped: Vec<GuildId> = self
            .guilds
            .values()
            .filter(|g| g.members.me().is_some_and(|m| m.is_timed_out_at(now_ms)) != g.timed_out)
            .map(|g| g.id)
            .collect();
        for g in flipped {
            self.recompute(g, &mut c);
        }
        if self
            .guild_settings
            .mute_boundary_between(before.min(now_ms), before.max(now_ms))
        {
            c.settings = true;
            c.read_all();
        }
        c
    }

    // ----- reads -----

    /// The signed-in account.
    pub fn me(&self) -> &CurrentUser {
        &self.me
    }

    /// The signed-in account's ID.
    pub fn me_id(&self) -> UserId {
        self.me_id
    }

    /// The gateway session ID from READY.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The clock as of the last [`AccountState::tick`].
    pub fn now_ms(&self) -> i64 {
        self.now_ms
    }

    /// One guild.
    pub fn guild(&self, g: GuildId) -> Option<&GuildState> {
        self.guilds.get(&g)
    }

    /// Every guild, in ID order (the sidebar order comes from the user's
    /// guild folders).
    pub fn guilds(&self) -> impl Iterator<Item = &GuildState> {
        self.guilds.values()
    }

    /// One channel.
    pub fn channel(&self, ch: ChannelId) -> Option<&Channel> {
        self.channels.get(ch)
    }

    /// Every channel.
    pub fn channels(&self) -> &Channels {
        &self.channels
    }

    /// A guild's channels by `position`, then ID (threads included).
    pub fn guild_channels(&self, g: GuildId) -> Vec<&Channel> {
        let Some(guild) = self.guilds.get(&g) else {
            return Vec::new();
        };
        let mut v: Vec<(ChannelId, &Channel)> = guild
            .channels
            .iter()
            .filter_map(|id| self.channels.get(*id).map(|c| (*id, c)))
            .collect();
        v.sort_by_key(|(id, c)| (c.position.unwrap_or(0), *id));
        v.into_iter().map(|(_, c)| c).collect()
    }

    /// The channels the sidebar shows: viewable channels, and categories with
    /// at least one viewable channel. Empty until the roles are loaded.
    pub fn visible_channels(&self, g: GuildId) -> Vec<&Channel> {
        use inlet_protocol::models::channel_type::GUILD_CATEGORY;
        let all = self.guild_channels(g);
        let id_of = |c: &Channel| ChannelId::parse(&c.id);
        let viewable = |c: &Channel| id_of(c).is_some_and(|id| self.permissions(g, id).can_view());
        // Categories with a viewable child, found in one pass.
        let open: BTreeSet<ChannelId> = all
            .iter()
            .filter(|c| c.kind != GUILD_CATEGORY && viewable(c))
            .filter_map(|c| id_of(c).and_then(|id| self.channels.parent_of(id)))
            .collect();
        all.into_iter()
            .filter(|c| {
                if c.kind == GUILD_CATEGORY {
                    id_of(c).is_some_and(|id| open.contains(&id))
                } else {
                    viewable(c)
                }
            })
            .collect()
    }

    /// DMs, group DMs and notes, newest activity first.
    pub fn private_channels(&self) -> Vec<&Channel> {
        let mut v: Vec<(Option<MessageId>, ChannelId, &Channel)> = self
            .channels
            .private()
            .map(|(id, c)| {
                let last = self
                    .read_states
                    .get(id)
                    .and_then(|e| e.last_message_id)
                    .or_else(|| MessageId::parse_opt(c.last_message_id.as_deref()));
                (last, id, c)
            })
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        v.into_iter().map(|(_, _, c)| c).collect()
    }

    /// A cached user.
    pub fn user(&self, id: UserId) -> Option<&User> {
        self.users.get(id)
    }

    /// A cached member of a guild.
    pub fn member(&self, g: GuildId, u: UserId) -> Option<&GuildMember> {
        self.guilds.get(&g)?.members.get(u, self.me_id)
    }

    /// A user's presence (`None` when offline or unknown).
    pub fn presence(&self, u: UserId) -> Option<&Presence> {
        self.presences.get(u)
    }

    /// Every presence.
    pub fn presences(&self) -> &Presences {
        &self.presences
    }

    /// The loaded messages of a channel.
    pub fn messages(&self, ch: ChannelId) -> Option<&MessageWindow> {
        self.windows.get(ch)
    }

    /// Every message window.
    pub fn message_windows(&self) -> &MessageWindows {
        &self.windows
    }

    /// Users typing in a channel (never the account itself).
    pub fn typing(&self, ch: ChannelId) -> Vec<UserId> {
        self.typing.users(ch, self.now_ms)
    }

    /// A channel's raw read state.
    pub fn read_state(&self, ch: ChannelId) -> Option<&ReadStateEntry> {
        self.read_states.get(ch)
    }

    /// Every read state.
    pub fn read_states(&self) -> &ReadStates {
        &self.read_states
    }

    /// Notification settings.
    pub fn guild_settings(&self) -> &GuildSettings {
        &self.guild_settings
    }

    /// The account's permissions in a guild channel. [`Permissions::NONE`]
    /// for a channel of another guild, an unknown channel, and until the
    /// guild's roles and the account's member are loaded.
    pub fn permissions(&self, g: GuildId, ch: ChannelId) -> Permissions {
        self.guilds
            .get(&g)
            .and_then(|guild| guild.perms.get(&ch))
            .map_or(Permissions::NONE, |b| Permissions(*b))
    }

    /// The account's permissions in any channel: every bit in DMs.
    pub fn channel_permissions(&self, ch: ChannelId) -> Permissions {
        if self.channels.get(ch).is_none() {
            return Permissions::NONE;
        }
        match self.channels.guild_of(ch) {
            Some(g) => self.permissions(g, ch),
            None => Permissions::ALL,
        }
    }

    /// Guild-level permissions (before channel overwrites).
    pub fn guild_permissions(&self, g: GuildId) -> Permissions {
        self.guilds
            .get(&g)
            .map_or(Permissions::NONE, |guild| Permissions(guild.guild_perms))
    }

    /// Whether a channel is muted (guild, its category, or itself).
    pub fn is_muted(&self, ch: ChannelId) -> bool {
        if self.channels.get(ch).is_none() {
            return false;
        }
        self.guild_settings.muted_at_any_level(
            self.channels.guild_of(ch),
            ch,
            self.channels.parent_of(ch),
            self.now_ms,
        )
    }

    /// A channel's unread state. A muted channel still reports `unread`
    /// (shown dimmed); it only stays out of the guild's pill.
    pub fn unread(&self, ch: ChannelId) -> Unread {
        let (Some(channel), Some(entry)) = (self.channels.get(ch), self.read_states.get(ch)) else {
            return Unread::default();
        };
        let guild = self.channels.guild_of(ch).and_then(|g| self.guilds.get(&g));
        if guild.is_some_and(|g| g.visiting) || !has_read_state(channel.kind) {
            return Unread::default();
        }
        let floor = guild
            .and_then(GuildState::joined_at_ms)
            .unwrap_or_else(|| ch.timestamp_ms());
        Unread {
            unread: entry.has_unread(floor),
            mentions: entry.mention_count,
            muted: self.is_muted(ch),
        }
    }

    /// A guild's pill and badge (Fluxer's `GuildReadState`): unread when an
    /// unmuted, viewable text channel is unread under the guild's
    /// notification level; mentions are summed over every viewable channel,
    /// muted or not. A visited guild is never unread. Recompute it for the
    /// guilds in [`Changes::read_state_guilds`].
    pub fn guild_unread(&self, g: GuildId) -> Unread {
        let Some(guild) = self.guilds.get(&g) else {
            return Unread::default();
        };
        let muted = self.guild_settings.guild_muted(Some(g), self.now_ms);
        if guild.visiting {
            return Unread {
                muted,
                ..Default::default()
            };
        }
        // getLockedCommunityUnreadBadgesLevel (unread badge customisation off).
        let locked = if muted {
            Some(notifications::NO_MESSAGES)
        } else {
            match self
                .guild_settings
                .guild_notifications(Some(g), guild.default_notifications())
            {
                notifications::ALL_MESSAGES => Some(notifications::ALL_MESSAGES),
                notifications::NO_MESSAGES => Some(notifications::NO_MESSAGES),
                _ => None,
            }
        };
        let mut out = Unread {
            muted,
            ..Default::default()
        };
        for id in &guild.channels {
            let Some(channel) = self.channels.get(*id) else {
                continue;
            };
            // Deviation: threads and forum posts are left out until thread
            // membership is tracked (Fluxer counts joined threads).
            if !is_guild_text_based(channel.kind) {
                continue;
            }
            // Fluxer's channel store only holds channels the gateway let the
            // account see; here hidden channels are skipped explicitly.
            if guild
                .perms
                .get(id)
                .is_none_or(|b| !Permissions(*b).can_view())
            {
                continue;
            }
            let u = self.unread(*id);
            let level = if u.muted {
                Some(notifications::NO_MESSAGES)
            } else {
                locked
            };
            let unread_allowed = u.unread
                && (level == Some(notifications::ALL_MESSAGES)
                    || (level != Some(notifications::NO_MESSAGES)
                        && level != Some(notifications::ONLY_MENTIONS)
                        && !u.muted));
            out.unread |= unread_allowed;
            out.mentions = out.mentions.saturating_add(u.mentions);
        }
        out
    }

    /// The direct messages button: unread when any DM is unread (muted DMs
    /// included, as in Fluxer), and their mentions summed.
    pub fn private_unread(&self) -> Unread {
        let mut out = Unread::default();
        for (id, _) in self.channels.private() {
            let u = self.unread(id);
            out.unread |= u.unread;
            out.mentions = out.mentions.saturating_add(u.mentions);
        }
        out
    }

    // ----- internals -----

    /// Fluxer's `supportsUnreadTracking`: no read state in visited guilds.
    fn untracked(&self, ch: ChannelId) -> bool {
        self.channels
            .guild_of(ch)
            .and_then(|g| self.guilds.get(&g))
            .is_some_and(|g| g.visiting)
    }

    fn recompute(&mut self, g: GuildId, c: &mut Changes) {
        let (me, channels, now) = (&self.me, &self.channels, self.now_ms);
        if let Some(guild) = self.guilds.get_mut(&g) {
            if permissions::recompute(guild, channels, me, now) {
                c.perms(g);
            }
        }
    }

    fn recompute_for_channel(&mut self, ch: &Channel, c: &mut Changes) {
        let Some(id) = ChannelId::parse(&ch.id) else {
            return;
        };
        if let Some(g) = self.channels.guild_of(id) {
            self.recompute_family(g, id, c);
        }
    }

    /// The guild's channels whose parent is `id` (threads and category children).
    fn children(&self, g: GuildId, id: ChannelId) -> Vec<ChannelId> {
        self.guilds.get(&g).map_or_else(Vec::new, |guild| {
            guild
                .channels
                .iter()
                .copied()
                .filter(|t| *t != id && self.channels.parent_of(*t) == Some(id))
                .collect()
        })
    }

    /// Recomputes one channel and the channels under it.
    fn recompute_family(&mut self, g: GuildId, id: ChannelId, c: &mut Changes) {
        let mut ids = vec![id];
        ids.extend(self.children(g, id));
        self.recompute_ids(g, &ids, c);
    }

    /// Recomputes the listed channels of a guild (gone ones are dropped).
    fn recompute_ids(&mut self, g: GuildId, ids: &[ChannelId], c: &mut Changes) {
        let (me, channels, now) = (&self.me, &self.channels, self.now_ms);
        let Some(guild) = self.guilds.get_mut(&g) else {
            return;
        };
        if permissions::recompute_channels(guild, channels, me, now, ids) {
            c.perms(g);
        }
    }

    /// GUILD_CREATE, GUILD_SYNC and READY guilds: the guild's full state.
    fn ingest_guild(&mut self, g: &GuildReady, c: &mut Changes) {
        let Some(id) = GuildId::parse(&g.id) else {
            return;
        };
        if !self.guilds.contains_key(&id) && self.guilds.len() >= MAX_GUILDS {
            return;
        }
        c.guilds = true;
        let guild = self.guilds.entry(id).or_insert_with(|| GuildState::new(id));
        if g.unavailable {
            guild.unavailable = true;
            return;
        }
        guild.unavailable = false;
        if let Some(p) = &g.properties {
            let mut p = p.clone();
            size::cap_guild(&mut p);
            guild.properties = Some(p);
        }
        guild.visiting = g.visiting || g.properties.as_ref().is_some_and(|p| p.visiting);
        guild.left = false;
        guild.roles.replace(g.parsed_roles());
        guild.member_count = g.member_count.or(guild.member_count);
        guild.online_count = g.online_count().or(guild.online_count);
        guild.emojis = g
            .emojis()
            .into_iter()
            .take(MAX_EXPRESSIONS_PER_GUILD)
            .map(|mut x| {
                size::cap_emoji(&mut x);
                x
            })
            .collect();
        guild.stickers = g
            .stickers()
            .into_iter()
            .take(MAX_EXPRESSIONS_PER_GUILD)
            .map(|mut x| {
                size::cap_sticker(&mut x);
                x
            })
            .collect();
        guild.refresh_cached();
        for m in g.parsed_members() {
            self.upsert_member(id, &m, MemberSource::Authoritative, c);
        }
        let old: BTreeSet<ChannelId> = self
            .guilds
            .get_mut(&id)
            .map(|g| std::mem::take(&mut g.channels))
            .unwrap_or_default();
        let mut fresh = BTreeSet::new();
        for ch in &g.channels {
            let Some(cid) = ChannelId::parse(&ch.id) else {
                continue;
            };
            // A channel of another guild is not moved here.
            if self.channels.guild_of(cid).is_some_and(|other| other != id) {
                continue;
            }
            if fresh.len() >= MAX_CHANNELS_PER_GUILD {
                break;
            }
            let mut ch = ch.clone();
            // GUILD_CREATE channels may omit guild_id; the calculator needs it.
            ch.guild_id = Some(id.to_string());
            fresh.insert(cid);
            self.store_channel(cid, ch, Some(id), c);
        }
        // Channels the guild no longer sends were hidden: keep their acks.
        for gone in old.difference(&fresh) {
            self.forget_channel(*gone, true, c);
        }
        if let Some(guild) = self.guilds.get_mut(&id) {
            guild.channels = fresh;
        }
        self.recompute(id, c);
    }

    /// Stores a channel and seeds its read state (restoring an archived ack).
    fn store_channel(
        &mut self,
        cid: ChannelId,
        mut ch: Channel,
        guild: Option<GuildId>,
        c: &mut Changes,
    ) {
        size::cap_channel(&mut ch);
        let last = MessageId::parse_opt(ch.last_message_id.as_deref());
        let pin_ms = ch
            .last_pin_timestamp()
            .and_then(parse_timestamp_ms)
            .unwrap_or(0);
        let kind = ch.kind;
        if self.channels.insert(cid, ch) {
            c.channel(cid);
        }
        if has_read_state(kind) && self.read_states.seed(cid, guild, last, pin_ms) {
            c.read(cid, guild);
        }
    }

    /// CHANNEL_CREATE / CHANNEL_UPDATE (permissions are recomputed by the caller).
    fn upsert_channel(&mut self, ch: &Channel, c: &mut Changes) {
        let Some(cid) = ChannelId::parse(&ch.id) else {
            return;
        };
        let known_guild = self.channels.guild_of(cid);
        let sent_guild = GuildId::parse_opt(ch.guild_id.as_deref());
        if ch.guild_id.is_some() && sent_guild.is_none() {
            return;
        }
        let guild = match (known_guild, sent_guild) {
            // A channel never moves between guilds.
            (Some(k), Some(s)) if k != s => return,
            (Some(k), _) => Some(k),
            // A known DM never becomes a guild channel.
            (None, Some(_)) if self.channels.get(cid).is_some() => return,
            (None, s) => s,
        };
        let mut ch = ch.clone();
        match guild {
            Some(g) => {
                let Some(gs) = self.guilds.get_mut(&g) else {
                    return;
                };
                if !gs.channels.contains(&cid) && gs.channels.len() >= MAX_CHANNELS_PER_GUILD {
                    return;
                }
                gs.channels.insert(cid);
                ch.guild_id = Some(g.to_string());
            }
            None => {
                if !is_private(ch.kind) {
                    return;
                }
                if self.channels.get(cid).is_none()
                    && self.channels.private_len() >= MAX_PRIVATE_CHANNELS
                {
                    return;
                }
                for u in &ch.recipients {
                    c.users |= self.users.upsert(u);
                }
            }
        }
        self.store_channel(cid, ch, guild, c);
    }

    /// CHANNEL_DELETE: the channel and its threads go; a guild channel's ack
    /// is archived (Fluxer sends delete and create when access is lost and
    /// regained), a DM's ack stays.
    fn delete_channel(&mut self, id: ChannelId, c: &mut Changes) {
        if self.channels.get(id).is_none() {
            return;
        }
        let guild = self.channels.guild_of(id);
        let threads: Vec<ChannelId> = match guild {
            Some(g) => self
                .children(g, id)
                .into_iter()
                .filter(|t| self.channels.get(*t).is_some_and(Channel::is_thread))
                .collect(),
            None => Vec::new(),
        };
        let gone: Vec<ChannelId> = std::iter::once(id).chain(threads).collect();
        for ch in &gone {
            self.forget_channel(*ch, true, c);
            if let Some(gs) = guild.and_then(|g| self.guilds.get_mut(&g)) {
                gs.channels.remove(ch);
            }
        }
        if let Some(g) = guild {
            self.recompute_ids(g, &gone, c);
        }
    }

    /// Removes a channel with its window and typing. A DM keeps its read
    /// state; a guild channel's read state is archived (`keep_ack`) or dropped.
    fn forget_channel(&mut self, id: ChannelId, keep_ack: bool, c: &mut Changes) {
        let guild = self.channels.guild_of(id);
        if self.channels.remove(id).is_some() {
            c.channel(id);
        }
        if self.windows.remove(id) {
            c.message(id);
        }
        if self.typing.clear_channel(id) {
            c.typing(id);
        }
        if guild.is_some() {
            let changed = if keep_ack {
                self.read_states.archive(id)
            } else {
                self.read_states.remove(id)
            };
            if changed {
                c.read(id, guild);
            }
        }
    }

    fn remove_guild(&mut self, g: GuildId, c: &mut Changes) {
        let Some(guild) = self.guilds.remove(&g) else {
            return;
        };
        for ch in &guild.channels {
            self.forget_channel(*ch, false, c);
        }
        // Read states that were only known from READY or archived.
        self.read_states.retain(|_, e| e.guild != Some(g));
        c.read_state = true;
        c.read_state_guilds.add(Some(g));
        c.guilds = true;
        c.members_of(g);
        c.perms(g);
    }

    /// Stores a member; `Attached` fills in only a missing one. The account's
    /// own member triggers a permission recompute.
    fn upsert_member(&mut self, g: GuildId, m: &GuildMember, src: MemberSource, c: &mut Changes) {
        let me = self.me_id;
        let Some(guild) = self.guilds.get_mut(&g) else {
            return;
        };
        let user = m.user_id().and_then(UserId::parse);
        let before = guild.members.others_len();
        let changed = match src {
            MemberSource::Authoritative => guild.members.upsert(m, me),
            MemberSource::Attached => guild.members.insert_if_missing(m, me),
        };
        let grew = guild.members.others_len() > before;
        if changed {
            c.members_of(g);
            if user == Some(me) {
                if src == MemberSource::Authoritative {
                    guild.left = false;
                }
                guild.refresh_cached();
                self.recompute(g, c);
            }
        }
        if let Some(u) = &m.user {
            c.users |= self.users.upsert(u);
        }
        if grew {
            self.enforce_member_cap(c);
        }
    }

    /// The `member` on a message, with the author as its user.
    fn attached_member(
        &mut self,
        g: GuildId,
        mut member: GuildMember,
        author: &User,
        c: &mut Changes,
    ) {
        match &member.user {
            None => member.user = Some(author.clone()),
            // A member for someone other than the author is not trusted.
            Some(u) if u.id != author.id => return,
            Some(_) => {}
        }
        self.upsert_member(g, &member, MemberSource::Attached, c);
    }

    /// Keeps the cached members of all guilds under [`MAX_MEMBERS_TOTAL`] by
    /// evicting from the guild that caches the most.
    fn enforce_member_cap(&mut self, c: &mut Changes) {
        let mut total: usize = self.guilds.values().map(|g| g.members.others_len()).sum();
        while total > MAX_MEMBERS_TOTAL {
            let Some(g) = self
                .guilds
                .values()
                .max_by(|a, b| {
                    a.members
                        .others_len()
                        .cmp(&b.members.others_len())
                        .then(b.id.cmp(&a.id))
                })
                .map(|g| g.id)
            else {
                break;
            };
            match self.guilds.get_mut(&g) {
                Some(guild) => {
                    if !guild.members.evict_oldest() {
                        break;
                    }
                    c.members_of(g);
                }
                None => break,
            }
            total -= 1;
        }
    }

    fn member_list(&mut self, u: &GuildMemberListUpdate, c: &mut Changes) {
        let Some(g) = GuildId::parse(&u.guild_id) else {
            return;
        };
        let Some(guild) = self.guilds.get_mut(&g) else {
            return;
        };
        if guild.member_count != Some(u.member_count) || guild.online_count != Some(u.online_count)
        {
            guild.member_count = Some(u.member_count);
            guild.online_count = Some(u.online_count);
            c.guilds = true;
            // The member count decides whether the guild is large.
            c.read_state = true;
            c.read_state_guilds.add(Some(g));
        }
        // INSERT and UPDATE ops are left to the member-list task; SYNC is enough here.
        for op in u.ops.iter().filter(|op| op.op == "SYNC") {
            for item in &op.items {
                let MemberListItem::Member(m) = item else {
                    continue;
                };
                self.upsert_member(g, m, MemberSource::Authoritative, c);
                if let (Some(id), Some(p)) = (m.user_id().and_then(UserId::parse), m.presence()) {
                    c.presence |= self.presences.upsert_for(id, &p);
                }
            }
        }
    }

    fn bump_channel_last(&mut self, ch: ChannelId, last: MessageId) {
        if let Some(channel) = self.channels.get_mut(ch) {
            let known = MessageId::parse_opt(channel.last_message_id.as_deref());
            if known < Some(last) {
                channel.last_message_id = Some(last.to_string());
            }
        }
    }

    fn message_create(&mut self, m: &Message, c: &mut Changes) {
        let (Some(ch), Some(id)) = (ChannelId::parse(&m.channel_id), MessageId::parse(&m.id))
        else {
            return;
        };
        let author = UserId::parse(&m.author.id);
        c.users |= self.users.upsert(&m.author);
        let guild = self.channels.guild_of(ch);
        if let (Some(g), Some(member)) = (guild, m.member()) {
            self.attached_member(g, member, &m.author, c);
        }
        if let Some(a) = author {
            if self.typing.stop(ch, a) {
                c.typing(ch);
            }
        }
        if self.windows.insert_live(ch, id, m) {
            c.message(ch);
        }
        let Some(kind) = self.channels.get(ch).map(|x| x.kind) else {
            return;
        };
        self.bump_channel_last(ch, id);
        if !has_read_state(kind) {
            return;
        }
        if self.untracked(ch) {
            if self.read_states.bump_last(ch, id) {
                c.read(ch, guild);
            }
            return;
        }
        let from_me = author == Some(self.me_id);
        let mention = !from_me && self.should_mention(ch, m);
        let changed = match self.read_states.ensure(ch, guild) {
            Some(_) => self.read_states.incoming(ch, id, from_me, mention),
            None => false,
        };
        if changed {
            c.read(ch, guild);
        }
    }

    /// Fluxer's `shouldMentionFor`: the account, `@everyone`/`@here`, one of
    /// its roles, or any message in an unmuted DM. Blocked authors never count.
    fn should_mention(&self, ch: ChannelId, m: &Message) -> bool {
        if UserId::parse(&m.author.id).is_some_and(|a| self.relationships.is_blocked(a)) {
            return false;
        }
        let Some(channel) = self.channels.get(ch) else {
            return false;
        };
        let guild = self.channels.guild_of(ch);
        let user = m
            .mentions
            .iter()
            .any(|u| UserId::parse(&u.id) == Some(self.me_id));
        let settings = self.guild_settings.get(guild);
        let suppress_everyone = guild.is_some() && settings.suppress_everyone;
        let suppress_roles = guild.is_some() && settings.suppress_roles;
        // A thread the account has not joined does not ping for @everyone.
        let non_member_thread =
            channel.is_thread() && !channel.extra.get("member").is_some_and(|v| v.is_object());
        let everyone = !suppress_everyone && m.mention_everyone() && !non_member_thread;
        let role = !suppress_roles
            && guild
                .and_then(|g| self.guilds.get(&g))
                .and_then(|g| g.members.me())
                .is_some_and(|me| m.mention_roles().iter().any(|r| me.roles.contains(r)));
        let private = is_private(channel.kind)
            && !self
                .guild_settings
                .guild_or_channel_muted(None, ch, self.now_ms);
        user || everyone || role || private
    }
}
