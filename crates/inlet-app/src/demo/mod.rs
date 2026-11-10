// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo data, compiled only with the `demo` cargo feature and used only when the
//! `INLET_DEMO` environment variable is set. Lets the UI be tried and screenshotted
//! without an account. It touches neither the network nor the keychain.
//!
//! The demo speaks the protocol's types: a READY, pages of messages and gateway events,
//! so the UI runs through the same account state and conversions as with a real
//! instance. Demo IDs such as `c-general` are mapped to stable snowflakes
//! ([`snowflake`]), and message IDs are made from the message's time.
//!
//! One file per domain (`guilds`, `messages`, `social`); `scenes` picks what to show.

pub mod composer;
mod guilds;
mod messages;
pub mod scenes;
mod social;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use inlet_protocol as p;
use inlet_state::{MessageId, MessagesPage};
use serde_json::{json, Value};

use crate::backend::messages::parse::{self, SharedNames};
use crate::backend::messages::PAGE_SIZE;
use crate::backend::{accounts, messages as msgs, Ui};
use crate::model::{Channel, ChannelKind, Message, Names};

/// The demo's own account.
pub const ME: &str = "demo";

pub struct MockSession {
    events: Ui,
    channels: HashMap<Option<String>, Vec<Channel>>,
    messages: Arc<Mutex<HashMap<String, Vec<p::Message>>>>,
    names: SharedNames,
    open_channel: Arc<Mutex<Option<String>>>,
    next_seq: u64,
    ticker: tokio::task::JoinHandle<()>,
}

/// A stable snowflake for a demo ID (`g1`, `c-general`); a snowflake stays as it is.
pub fn snowflake(name: &str) -> String {
    if inlet_state::ChannelId::parse(name).is_some() {
        return name.to_string();
    }
    // FNV-1a, so the same name gives the same ID in every run.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    ((h >> 4) | (1 << 59)).to_string()
}

/// `<#id>` for a demo channel.
pub fn channel_ref(name: &str) -> String {
    format!("<#{}>", snowflake(name))
}

/// The user ID of a demo person.
pub fn user_id(name: &str) -> String {
    match name {
        ME => "2000".into(),
        "ada" => "2001".into(),
        "linus" => "2002".into(),
        "grace" => "2003".into(),
        "Sylo" => "2004".into(),
        "Inlet" => "2005".into(),
        other => snowflake(&format!("user-{other}")),
    }
}

/// A demo person as the API sends them.
pub fn user(name: &str) -> Value {
    json!({
        "id": user_id(name),
        "username": name.to_lowercase(),
        "global_name": name,
        "bot": name == "Sylo",
    })
}

/// Role IDs of the demo guild `g1`.
pub const ROLE_MODERATORS: &str = "3001";
pub const ROLE_CONTRIBUTORS: &str = "3002";

/// The roles a demo person has in the first guild (for name colours).
fn roles_of(name: &str) -> Vec<&'static str> {
    match name {
        "ada" => vec![ROLE_MODERATORS],
        "grace" => vec![ROLE_CONTRIBUTORS],
        ME => vec![ROLE_MODERATORS],
        _ => Vec::new(),
    }
}

pub(crate) fn channel(
    id: &str,
    name: &str,
    kind: ChannelKind,
    parent: Option<&str>,
    position: i64,
) -> Channel {
    Channel {
        id: id.into(),
        name: name.into(),
        kind,
        parent_id: parent.map(Into::into),
        position,
        topic: (id == "c-general").then(|| "General chat \u{2014} be nice, have fun".to_string()),
        // The development channel shows the slowmode countdown in the demo.
        slowmode_secs: if id == "c-dev" { 10 } else { 0 },
    }
}

/// Mention names for the demo: people, roles and every demo channel.
pub(crate) fn demo_names(channels: &HashMap<Option<String>, Vec<Channel>>) -> Names {
    let mut n = Names::default();
    for list in channels.values() {
        for c in list {
            n.channels.insert(c.id.clone(), c.name.clone());
        }
    }
    for name in [ME, "ada", "linus", "grace", "Sylo", "Inlet"] {
        n.users.insert(user_id(name), name.to_string());
    }
    n.roles.insert(ROLE_MODERATORS.into(), "Moderators".into());
    n.roles
        .insert(ROLE_CONTRIBUTORS.into(), "Contributors".into());
    n
}

/// An embed as the API sends it.
pub(crate) fn embed(
    color: u32,
    title: &str,
    description: &str,
    fields: &[(&str, &str)],
    footer: Option<&str>,
) -> Value {
    let mut e = json!({
        "color": color,
        "title": title,
        "description": description,
        "fields": fields
            .iter()
            .map(|(name, value)| json!({"name": name, "value": value, "inline": false}))
            .collect::<Vec<_>>(),
    });
    if let Some(footer) = footer {
        e["footer"] = json!({ "text": footer });
    }
    e
}

/// Midnight UTC, 10 November 2026: the demo's "today".
const DEMO_DAY_MS: i64 = 1_794_268_800_000;

/// A message ID made from a time (and a sequence number for messages in the same
/// millisecond), so IDs sort like times, as real snowflakes do.
pub fn message_id(ms: i64, seq: u64) -> String {
    let since_epoch = (ms - p::models::FLUXER_EPOCH_MS as i64).max(0) as u64;
    ((since_epoch << 22) | (seq & 0x3F_FFFF)).to_string()
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339()
}

/// The time of `HH:MM` on the demo day.
pub(crate) fn at(time: &str) -> i64 {
    let minutes = time
        .split_once(':')
        .and_then(|(h, m)| Some(h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?))
        .unwrap_or(0);
    DEMO_DAY_MS + minutes * 60_000
}

/// A message as the API sends it. `extra` adds or replaces fields.
pub(crate) fn message_at(
    seq: u64,
    channel_id: &str,
    author: &str,
    ms: i64,
    content: &str,
    extra: Value,
) -> p::Message {
    let roles = roles_of(author);
    let mut v = json!({
        "id": message_id(ms, seq),
        "channel_id": snowflake(channel_id),
        "author": user(author),
        "type": 0,
        "content": content,
        "timestamp": rfc3339(ms),
        "member": {"roles": roles},
    });
    let mentions: Vec<Value> = [ME, "ada", "linus", "grace"]
        .iter()
        .filter(|n| content.contains(&format!("<@{}>", user_id(n))))
        .map(|n| user(n))
        .collect();
    v["mentions"] = Value::Array(mentions);
    let roles: Vec<&str> = [ROLE_MODERATORS, ROLE_CONTRIBUTORS]
        .into_iter()
        .filter(|r| content.contains(&format!("<@&{r}>")))
        .collect();
    v["mention_roles"] = json!(roles);
    if let (Some(obj), Some(more)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in more {
            obj.insert(k.clone(), val.clone());
        }
    }
    serde_json::from_value(v).expect("demo messages are valid")
}

/// A message at `HH:MM` on the demo day.
pub(crate) fn msg(
    seq: u64,
    channel_id: &str,
    author: &str,
    time: &str,
    content: &str,
) -> p::Message {
    message_at(seq, channel_id, author, at(time), content, json!({}))
}

/// Like [`msg`] with more fields.
pub(crate) fn msg_with(
    seq: u64,
    channel_id: &str,
    author: &str,
    time: &str,
    content: &str,
    extra: Value,
) -> p::Message {
    message_at(seq, channel_id, author, at(time), content, extra)
}

/// The fields that make `m` a reply to `to`.
pub(crate) fn reply_to(to: &p::Message) -> Value {
    json!({
        "type": 19,
        "message_reference": {"message_id": to.id, "channel_id": to.channel_id},
        "referenced_message": serde_json::to_value(to).unwrap_or(Value::Null),
    })
}

fn live(seq: u64, channel_id: &str, author: &str, content: &str, extra: Value) -> p::Message {
    let now = chrono::Utc::now().timestamp_millis();
    let mut m = message_at(seq, channel_id, author, now, content, extra);
    m.channel_id = channel_id.to_string();
    m
}

/// The newest `PAGE_SIZE` messages older than `before`, as the API would answer.
fn page_of(list: &[p::Message], before: Option<MessageId>) -> Vec<p::Message> {
    let mut older: Vec<&p::Message> = list
        .iter()
        .filter(|m| before.is_none_or(|b| MessageId::parse(&m.id).is_some_and(|id| id < b)))
        .collect();
    older.sort_by_key(|m| std::cmp::Reverse(MessageId::parse(&m.id)));
    older
        .into_iter()
        .take(usize::from(PAGE_SIZE))
        .cloned()
        .collect()
}

impl MockSession {
    pub fn start(events: Ui) -> Self {
        let scene = scenes::current();
        let scenes::Data {
            guilds,
            channels,
            messages,
            ready,
            outgoing,
        } = scenes::data(scene);
        let names: SharedNames = Arc::new(Mutex::new(demo_names(&channels)));
        let messages = Arc::new(Mutex::new(messages));
        let open_channel: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Pretend the gateway delivers a message now and then in the open channel.
        let ticker = {
            let events = events.clone();
            let open = open_channel.clone();
            let messages = messages.clone();
            let names = names.clone();
            let interval = std::env::var("INLET_DEMO_LIVE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(12)
                .max(1);
            tokio::spawn(async move {
                let mut n = 0u64;
                loop {
                    tokio::time::sleep(Duration::from_secs(interval)).await;
                    let Some(channel_id) = open.lock().unwrap().clone() else {
                        continue;
                    };
                    n += 1;
                    let m = live(
                        1000 + n,
                        &channel_id,
                        "linus",
                        &format!("Live message #{n} from the demo gateway"),
                        json!({}),
                    );
                    messages
                        .lock()
                        .unwrap()
                        .entry(channel_id)
                        .or_default()
                        .push(m.clone());
                    let rendered = vec![parse::to_message(&m, &names)];
                    let _ = events.unbounded_send(msgs::Ev::Dispatch {
                        dispatch: Box::new(p::Dispatch::MessageCreate(Box::new(m))),
                        rendered,
                    });
                }
            })
        };

        let _ = events.unbounded_send(msgs::Ev::Dispatch {
            dispatch: Box::new(p::Dispatch::Ready(Box::new(ready))),
            rendered: Vec::new(),
        });
        let _ = events.unbounded_send(accounts::Ev::SignedIn {
            username: ME.into(),
            instance: "demo".into(),
        });
        let _ = events.unbounded_send(msgs::Ev::Guilds(guilds));
        for out in outgoing {
            let _ = events.unbounded_send(msgs::Ev::Pending(out));
        }
        let _ = events.unbounded_send(accounts::Ev::Status("Demo mode".into()));

        Self {
            events,
            channels,
            messages,
            names,
            open_channel,
            next_seq: 100,
            ticker,
        }
    }

    pub fn open_guild(&mut self, guild_id: Option<String>) {
        let channels = self.channels.get(&guild_id).cloned().unwrap_or_default();
        let _ = self
            .events
            .unbounded_send(msgs::Ev::Channels { guild_id, channels });
    }

    fn page(&self, channel_id: &str, before: Option<MessageId>) -> (MessagesPage, Vec<Message>) {
        let list = page_of(
            self.messages
                .lock()
                .unwrap()
                .get(channel_id)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            before,
        );
        let rendered = list
            .iter()
            .map(|m| parse::to_message(m, &self.names))
            .collect();
        let page = match before {
            Some(b) => MessagesPage::before(b, list, PAGE_SIZE),
            None => MessagesPage::latest(list, PAGE_SIZE),
        };
        (page, rendered)
    }

    /// The newest page of a channel, which also becomes the one the demo gateway
    /// posts live messages in.
    pub fn newest_page(&mut self, channel_id: &str) -> (MessagesPage, Vec<Message>) {
        *self.open_channel.lock().unwrap() = Some(channel_id.to_string());
        self.page(channel_id, None)
    }

    pub fn older_page(
        &mut self,
        channel_id: &str,
        before: MessageId,
    ) -> (MessagesPage, Vec<Message>) {
        self.page(channel_id, Some(before))
    }

    pub fn send(&mut self, channel_id: String, text: String) {
        self.next_seq += 1;
        let nonce = format!("demo-{}", self.next_seq);
        self.post(channel_id, nonce, text);
    }

    /// "Retry" in the demo: the message goes through this time, with its nonce.
    pub fn resend(&mut self, channel_id: String, nonce: String, text: String) {
        self.next_seq += 1;
        self.post(channel_id, nonce.clone(), text);
        let _ = self.events.unbounded_send(msgs::Ev::Sent { nonce });
    }

    fn post(&mut self, channel_id: String, nonce: String, text: String) {
        let m = live(
            self.next_seq,
            &channel_id,
            ME,
            &text,
            json!({ "nonce": nonce }),
        );
        self.messages
            .lock()
            .unwrap()
            .entry(channel_id)
            .or_default()
            .push(m.clone());
        let rendered = vec![parse::to_message(&m, &self.names)];
        let _ = self.events.unbounded_send(msgs::Ev::Dispatch {
            dispatch: Box::new(p::Dispatch::MessageCreate(Box::new(m))),
            rendered,
        });
    }
}

impl Drop for MockSession {
    fn drop(&mut self) {
        self.ticker.abort();
    }
}
