// SPDX-License-Identifier: AGPL-3.0-or-later
//! Desktop notifications and their sound: which new message notifies, what the
//! notification says, and where a click on it leads. Pure rules without gpui; the UI
//! side (`ui/notifications/alerts.rs`) feeds them every new message of every signed-in
//! account and shows what they return through `inlet_platform`.
//!
//! A message notifies when all of these hold (as in Fluxer):
//!
//! - It is new: not from the account itself, not covered by the read mark, and not seen
//!   before (a repeated MESSAGE_CREATE notifies once).
//! - Its channel is not muted: not the guild, the category, the channel or (for a
//!   thread) its parent channel.
//! - The notification level allows it. The level is the channel's own, else its parent's
//!   (category, or a thread's channel and its category), else the guild's, else the
//!   guild's default (only mentions in a large guild). Direct messages notify for every
//!   message. "Only mentions" counts what the unread state counts as a mention, so
//!   `@everyone` and role mentions follow the guild's suppression settings and a muted
//!   DM never counts.
//! - The account is not in Do Not Disturb. DND silences both the notification and the
//!   sound.
//! - It is not in the channel that is open and on screen while Inlet's window has focus.
//!
//! What a notification shows comes from the server and is untrusted: names go through
//! `clean_name`, the content is cut before it is looked at, mention and emoji tokens
//! become plain names, spoilers are hidden, and `inlet_platform::Notification` removes
//! control and bidi characters and caps the length. With the preview off, the content
//! is never read at all: the body is a fixed text.
//!
//! Background accounts get the same rules. Their gateways run passive (no active guild),
//! which on Fluxer means a large guild sends MESSAGE_CREATE only for messages that
//! mention the account; DMs and smaller guilds send every message. Mentions therefore
//! always arrive, and nothing in the gateway settings had to change.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant};

use inlet_platform::{Notification, Preview};
use inlet_protocol::models::{channel_type, message_type};
use inlet_protocol::{Dispatch, Message};
use inlet_state::guild_settings::notifications::{ALL_MESSAGES, NO_MESSAGES, ONLY_MENTIONS};
use inlet_state::{AccountState, ChannelId, GuildId, MessageId, RoleId, UserId};
use serde::{Deserialize, Serialize};

use crate::backend::accounts::AccountKey;
use crate::model::clean_name;

/// The content is cut to this many bytes before it is looked at.
const CONTENT_MAX_BYTES: usize = 2000;
/// At most this many notifications in [`BURST_WINDOW`]; more are dropped, so a busy
/// channel cannot flood the screen or the platform's queue.
const BURST: usize = 6;
const BURST_WINDOW: Duration = Duration::from_secs(10);
/// The sound plays at most once in this time.
const SOUND_GAP: Duration = Duration::from_millis(1500);
/// Notifications whose click still leads somewhere.
const TARGETS: usize = 64;

/// The desktop notification settings, kept under `"notifications"` in `settings.json`.
/// Which messages notify per guild and channel is the account's own setting on the
/// server (the bell in the channel header); these are the local, global switches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertPrefs {
    /// Show desktop notifications.
    pub enabled: bool,
    /// Show the message content in them.
    pub preview: bool,
    /// Play a sound for a message that notifies.
    pub sound: bool,
    /// Flash the taskbar button (bounce the dock icon) while the window has no focus.
    pub flash: bool,
}

impl Default for AlertPrefs {
    fn default() -> Self {
        AlertPrefs {
            enabled: true,
            preview: true,
            sound: true,
            flash: true,
        }
    }
}

const PREFS_KEY: &str = "notifications";

/// Reads the settings from `settings.json` at `path`; defaults for a missing or broken
/// file or entry.
pub fn load_prefs(path: &Path) -> AlertPrefs {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get(PREFS_KEY).cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Writes the settings into `settings.json` at `path`, keeping the file's other keys,
/// through a temporary file so a crash never leaves half a file.
pub fn save_prefs(path: &Path, prefs: &AlertPrefs) -> std::io::Result<()> {
    let mut root = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    root[PREFS_KEY] = serde_json::to_value(prefs).map_err(std::io::Error::other)?;
    let text = serde_json::to_string_pretty(&root).map_err(std::io::Error::other)?;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("settings path has no folder"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".settings.json.{}.n.tmp", std::process::id()));
    let result = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// What a channel's read state was just before a MESSAGE_CREATE was applied.
#[derive(Clone, Copy, Debug)]
pub struct Probe {
    channel: ChannelId,
    message: MessageId,
    last: Option<MessageId>,
    mentions: u32,
}

/// Takes the [`Probe`] of a MESSAGE_CREATE; `None` for any other event.
pub fn probe(state: &AccountState, dispatch: &Dispatch) -> Option<Probe> {
    let Dispatch::MessageCreate(m) = dispatch else {
        return None;
    };
    let channel = ChannelId::parse(&m.channel_id)?;
    let message = MessageId::parse(&m.id)?;
    let entry = state.read_state(channel);
    Some(Probe {
        channel,
        message,
        last: entry.and_then(|e| e.last_message_id),
        mentions: entry.map_or(0, |e| e.mention_count),
    })
}

/// Why a message notifies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    Mention,
    Message,
}

/// Whether the message of `probe`, now applied to `state`, notifies under the
/// account's mutes and levels (see the module docs). DND and focus are checked by
/// [`Engine::on_message`].
pub fn why(state: &AccountState, probe: &Probe, m: &Message) -> Option<Why> {
    let author = UserId::parse(&m.author.id);
    if author == Some(state.me_id()) {
        return None;
    }
    // As Fluxer, before anything else: a blocked author, a message sent with @silent
    // (it still counts as a mention in the unread state), and anything but an ordinary
    // message or a reply (joins, pins, calls and other system messages) never notify.
    if author.is_some_and(|a| state.is_blocked(a))
        || m.is_silent()
        || !matches!(m.kind, message_type::DEFAULT | message_type::REPLY)
    {
        return None;
    }
    // No read state: a visited guild, or a channel kind without one.
    let entry = state.read_state(probe.channel)?;
    if entry.ack_message_id.is_some_and(|ack| probe.message <= ack) {
        return None;
    }
    // The unread state counted it as a mention (it counts a message once).
    let mention = entry.mention_count > probe.mentions;
    let fresh = probe.last.is_none_or(|last| last < probe.message);
    if !mention && !fresh {
        return None;
    }
    if muted(state, probe.channel) {
        return None;
    }
    // Every message of an unmuted DM counts as a mention in the unread state; for the
    // notification's wording only a real mention is one.
    let private = state
        .channel(probe.channel)
        .is_some_and(|c| c.kind == channel_type::DM || c.kind == channel_type::GROUP_DM);
    let mention = mention
        && (!private
            || m.mentions
                .iter()
                .any(|u| UserId::parse(&u.id) == Some(state.me_id())));
    // As Fluxer, every message of an unmuted DM or group DM notifies at "all messages"
    // and at "only mentions"; "nothing" still means nothing.
    if private {
        if level(state, probe.channel) == NO_MESSAGES {
            return None;
        }
        return Some(if mention { Why::Mention } else { Why::Message });
    }
    match level(state, probe.channel) {
        NO_MESSAGES => None,
        ONLY_MENTIONS => mention.then_some(Why::Mention),
        _ if mention => Some(Why::Mention),
        _ => Some(Why::Message),
    }
}

/// Muted at the guild, category or channel, or (a thread) at its parent channel.
fn muted(state: &AccountState, ch: ChannelId) -> bool {
    state.is_muted(ch)
        || state
            .channels()
            .parent_of(ch)
            .is_some_and(|p| state.is_muted(p))
}

/// The notification level that applies to a channel: its own override, its parents',
/// the guild's, or the guild's default.
// Deviation: for a thread Fluxer stops at the thread's parent channel and uses the
// thread membership's own level; Inlet does not track thread membership yet, so a
// thread follows its channel, then the channel's category.
pub fn level(state: &AccountState, ch: ChannelId) -> i64 {
    let guild = state.channels().guild_of(ch);
    let settings = state.guild_settings().get(guild);
    let mut at = Some(ch);
    for _ in 0..3 {
        let Some(c) = at else { break };
        if let Some(o) = settings.channel_overrides.get(&c) {
            if matches!(
                o.message_notifications,
                ALL_MESSAGES | ONLY_MENTIONS | NO_MESSAGES
            ) {
                return o.message_notifications;
            }
        }
        at = state.channels().parent_of(c);
    }
    let default = guild
        .and_then(|g| state.guild(g))
        .map_or(ALL_MESSAGES, |g| g.default_notifications());
    state.guild_settings().guild_notifications(guild, default)
}

/// Where a click on a notification leads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// The account; `None` for the one on screen (demo data, before any account key).
    pub account: Option<AccountKey>,
    /// The guild, `None` for a direct message.
    pub guild: Option<String>,
    /// The channel to open: for a thread, the channel it belongs to.
    pub channel: String,
    /// The thread to open in the side panel, for a message in a thread.
    pub thread: Option<String>,
}

/// What to do for one message.
#[derive(Debug, Default)]
pub struct Delivery {
    pub notification: Option<Notification>,
    pub sound: bool,
    pub flash: bool,
}

/// One new message as the UI hands it over.
pub struct Input<'a> {
    pub account: Option<&'a AccountKey>,
    /// The account's name, when more than one account is signed in.
    pub account_label: Option<&'a str>,
    pub state: &'a AccountState,
    pub probe: Probe,
    pub message: &'a Message,
    /// The channels on screen while the window has focus: the open channel and the
    /// thread open in the side panel (only for the account on screen).
    pub focused: &'a [ChannelId],
}

/// The settings, each account's Do Not Disturb, the rate limits and the clickable
/// notifications.
#[derive(Default)]
pub struct Engine {
    pub prefs: AlertPrefs,
    dnd: HashMap<Option<AccountKey>, bool>,
    /// Account keys by the number in their tags.
    accounts: Vec<Option<AccountKey>>,
    targets: VecDeque<(String, Target)>,
    shown: VecDeque<Instant>,
    sounded: Option<Instant>,
}

impl Engine {
    pub fn new(prefs: AlertPrefs) -> Self {
        Engine {
            prefs,
            ..Engine::default()
        }
    }

    /// Follows the account's own status: READY's settings and every settings update
    /// that carries a status.
    pub fn note_status(&mut self, account: Option<&AccountKey>, dispatch: &Dispatch) {
        let status = match dispatch {
            Dispatch::Ready(r) => r.user_settings().map(|s| s.status).unwrap_or_default(),
            Dispatch::UserSettingsUpdate(s) if !s.status.is_empty() => s.status.clone(),
            _ => return,
        };
        self.dnd.insert(account.cloned(), status == "dnd");
    }

    /// Whether the account is in Do Not Disturb.
    pub fn is_dnd(&self, account: Option<&AccountKey>) -> bool {
        self.dnd.get(&account.cloned()).copied().unwrap_or(false)
    }

    /// Where the notification with this tag leads, while it is one of the last
    /// [`TARGETS`] shown.
    pub fn target(&self, tag: &str) -> Option<&Target> {
        self.targets.iter().find(|(t, _)| t == tag).map(|(_, x)| x)
    }

    /// The tag of the notification shown for a channel of an account, which is then
    /// forgotten (the channel was read); `None` when none is shown.
    pub fn forget_channel(
        &mut self,
        account: Option<&AccountKey>,
        channel: ChannelId,
    ) -> Option<String> {
        let ix = self.accounts.iter().position(|a| a.as_ref() == account)?;
        let tag = format!("m{ix}-{channel}");
        let at = self.targets.iter().position(|(t, _)| *t == tag)?;
        self.targets.remove(at);
        Some(tag)
    }

    /// A new message, already applied to the account's state: what to show and play.
    pub fn on_message(&mut self, input: Input<'_>, now: Instant) -> Option<Delivery> {
        let why = why(input.state, &input.probe, input.message)?;
        // Deviation: in the channel on screen Fluxer plays a softer sound of its own; Inlet
        // stays silent there.
        if self.is_dnd(input.account) || input.focused.contains(&input.probe.channel) {
            return None;
        }
        let show = self.prefs.enabled && self.admit(now);
        let sound = self.prefs.sound
            && self
                .sounded
                .is_none_or(|t| now.saturating_duration_since(t) >= SOUND_GAP);
        if sound {
            self.sounded = Some(now);
        }
        let notification = if show {
            self.compose(&input, why)
        } else {
            None
        };
        if notification.is_none() && !sound {
            return None;
        }
        Some(Delivery {
            flash: notification.is_some() && self.prefs.flash,
            notification,
            sound,
        })
    }

    fn admit(&mut self, now: Instant) -> bool {
        while self
            .shown
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= BURST_WINDOW)
        {
            self.shown.pop_front();
        }
        if self.shown.len() >= BURST {
            return false;
        }
        self.shown.push_back(now);
        true
    }

    /// The tag of a channel's notifications: one per account and channel, so a newer
    /// message replaces the older notification (Windows, macOS).
    fn tag(&mut self, account: Option<&AccountKey>, channel: ChannelId) -> String {
        let key = account.cloned();
        let ix = match self.accounts.iter().position(|a| *a == key) {
            Some(ix) => ix,
            None => {
                self.accounts.push(key);
                self.accounts.len() - 1
            }
        };
        format!("m{ix}-{channel}")
    }

    fn compose(&mut self, input: &Input<'_>, why: Why) -> Option<Notification> {
        let state = input.state;
        let m = input.message;
        let ch = input.probe.channel;
        let tag = self.tag(input.account, ch);
        let title = title(state, m, ch, input.account_label);
        let hidden = match why {
            Why::Mention => t!("alerts-hidden-mention"),
            Why::Message => t!("alerts-hidden-message"),
        };
        let notification = if self.prefs.preview {
            Notification::message(&tag, &title, &body(state, m), Preview::Show, &hidden)
        } else {
            // The content is not even read.
            Notification::message(&tag, &title, "", Preview::Hide, &hidden)
        };
        let notification = match notification {
            Ok(n) => n,
            Err(err) => {
                log::warn!("notification not built: {err}");
                return None;
            }
        };
        let parent = state
            .channel(ch)
            .filter(|c| c.is_thread())
            .and_then(|_| state.channels().parent_of(ch));
        let target = Target {
            account: input.account.cloned(),
            guild: state.channels().guild_of(ch).map(|g| g.to_string()),
            channel: parent.unwrap_or(ch).to_string(),
            thread: parent.map(|_| ch.to_string()),
        };
        self.targets.retain(|(t, _)| *t != tag);
        self.targets.push_back((tag, target));
        while self.targets.len() > TARGETS {
            self.targets.pop_front();
        }
        Some(notification)
    }
}

/// The author's name as the place shows it: the guild nickname, else the display name.
fn author_name(state: &AccountState, guild: Option<GuildId>, m: &Message) -> String {
    let nick = guild
        .zip(UserId::parse(&m.author.id))
        .and_then(|(g, u)| state.member(g, u))
        .and_then(|member| member.display_name())
        .map(str::to_string);
    clean_name(nick.as_deref().unwrap_or_else(|| m.author.display_name()))
}

/// "Author", "Author (Group)" or "Author (#channel, Guild)", with the account's name
/// after a dot when several accounts are signed in.
fn title(state: &AccountState, m: &Message, ch: ChannelId, account: Option<&str>) -> String {
    let guild = state.channels().guild_of(ch);
    let author = author_name(state, guild, m);
    let channel = state.channel(ch);
    let mut title = match (guild, channel) {
        (Some(g), Some(c)) => {
            let guild_name = state.guild(g).map(|x| x.name()).unwrap_or_default();
            t!(
                "alerts-title-guild",
                author = author,
                channel = clean_name(c.name.as_deref().unwrap_or_default()),
                guild = clean_name(guild_name)
            )
        }
        (None, Some(c)) if c.kind == channel_type::GROUP_DM => {
            let group = c
                .name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .map(clean_name)
                .unwrap_or_else(|| t!("alerts-group-dm"));
            t!("alerts-title-group", author = author, group = group)
        }
        _ => author,
    };
    if let Some(account) = account {
        title = t!(
            "alerts-title-account",
            title = title,
            account = clean_name(account)
        );
    }
    title
}

/// The message as plain text for the body (preview on).
fn body(state: &AccountState, m: &Message) -> String {
    let text = plain_content(state, m);
    if !text.trim().is_empty() {
        return text;
    }
    match m.attachments.len() {
        0 => {}
        1 => return t!("alerts-attachment"),
        n => return t!("alerts-attachments", count = n),
    }
    if !m.embeds.is_empty() {
        return t!("alerts-embed");
    }
    t!("alerts-hidden-message")
}

/// The content with its tokens as names: `<@id>` and `<@!id>` the user, `<@&id>` the
/// role, `<#id>` the channel, `<:name:id>` and `<a:name:id>` `:name:`, and spoilers
/// hidden. Cut to [`CONTENT_MAX_BYTES`] first; anything else stays as it is (the
/// platform cleans control characters and caps the length).
pub fn plain_content(state: &AccountState, m: &Message) -> String {
    let mut end = m.content.len().min(CONTENT_MAX_BYTES);
    while !m.content.is_char_boundary(end) {
        end -= 1;
    }
    let content = hide_spoilers(&m.content[..end], end < m.content.len());
    let guild = ChannelId::parse(&m.channel_id).and_then(|c| state.channels().guild_of(c));
    let mut out = String::with_capacity(content.len());
    let mut rest = content.as_str();
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        match after.find('>').and_then(|close| {
            token_name(state, guild, m, &after[1..close]).map(|name| (close, name))
        }) {
            Some((close, name)) => {
                out.push_str(&name);
                rest = &after[close + 1..];
            }
            None => {
                out.push('<');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `||secret||` becomes the spoiler text. An unclosed `||` stays as it is, unless the
/// text was cut (`cut`): its closing `||` may be past the cut, so the rest is hidden.
fn hide_spoilers(text: &str, cut: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("||") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("||") else {
            if cut {
                out.push_str(&rest[..open]);
                out.push_str(&t!("alerts-spoiler"));
                return out;
            }
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str(&t!("alerts-spoiler"));
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

/// The name for the inside of a `<…>` token, or `None` when it is not one.
fn token_name(
    state: &AccountState,
    guild: Option<GuildId>,
    m: &Message,
    inner: &str,
) -> Option<String> {
    if inner.len() > 120 {
        return None;
    }
    if let Some(id) = inner.strip_prefix("@&") {
        let id = RoleId::parse(id)?;
        let name = guild
            .and_then(|g| state.guild(g))
            .and_then(|g| g.roles.get(id))
            .map(|r| clean_name(&r.name))
            .unwrap_or_else(|| t!("alerts-unknown-role"));
        return Some(format!("@{name}"));
    }
    if let Some(id) = inner.strip_prefix('@') {
        let id = id.strip_prefix('!').unwrap_or(id);
        let user = UserId::parse(id)?;
        let from_message = m
            .mentions
            .iter()
            .find(|u| UserId::parse(&u.id) == Some(user))
            .map(|u| u.display_name().to_string());
        let nick = guild
            .and_then(|g| state.member(g, user))
            .and_then(|member| member.display_name())
            .map(str::to_string);
        let known = state.user(user).map(|u| u.display_name().to_string());
        let name = nick
            .or(from_message)
            .or(known)
            .map(|n| clean_name(&n))
            .unwrap_or_else(|| t!("alerts-unknown-user"));
        return Some(format!("@{name}"));
    }
    if let Some(id) = inner.strip_prefix('#') {
        let ch = ChannelId::parse(id)?;
        let name = state
            .channel(ch)
            .and_then(|c| c.name.as_deref())
            .map(clean_name)
            .unwrap_or_else(|| t!("alerts-unknown-channel"));
        return Some(format!("#{name}"));
    }
    let emoji = inner
        .strip_prefix("a:")
        .or_else(|| inner.strip_prefix(':'))?;
    let (name, id) = emoji.split_once(':')?;
    let valid = !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && id.bytes().all(|b| b.is_ascii_digit())
        && !id.is_empty();
    valid.then(|| format!(":{name}:"))
}

#[cfg(test)]
mod tests;
