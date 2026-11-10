// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo data, compiled only with the `demo` cargo feature and used only when the
//! `INLET_DEMO` environment variable is set. Lets the UI be tried and screenshotted
//! without an account. It touches neither the network nor the keychain.
//!
//! One file per domain (`guilds`, `messages`, `social`); `scenes` picks what to show.

mod guilds;
mod messages;
pub mod scenes;
mod social;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{accounts, messages as msgs, Ui};
use crate::model::{Channel, ChannelKind, Embed, EmbedField, Message, Names};

pub struct MockSession {
    events: Ui,
    channels: HashMap<Option<String>, Vec<Channel>>,
    messages: Arc<Mutex<HashMap<String, Vec<Message>>>>,
    open_channel: Arc<Mutex<Option<String>>>,
    next_id: u64,
    ticker: tokio::task::JoinHandle<()>,
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
    }
}

/// Mention names for the demo. Mentions need numeric IDs, so the demo uses stand-ins.
pub(crate) fn demo_names() -> Names {
    let mut n = Names::default();
    n.channels.insert("1001".into(), "verify".into());
    n.channels.insert("1002".into(), "general".into());
    n.users.insert("2001".into(), "ada".into());
    n.roles.insert("3001".into(), "Moderators".into());
    n
}

pub(crate) fn embed(
    color: u32,
    title: &str,
    description: &str,
    fields: &[(&str, &str)],
    footer: Option<&str>,
) -> Embed {
    let names = demo_names();
    Embed {
        color: Some(color),
        author: None,
        title: Some(title.into()),
        url: None,
        description: crate::ui::markdown::parse_with(description, &names),
        fields: fields
            .iter()
            .map(|(name, value)| EmbedField {
                name: (*name).into(),
                value: crate::ui::markdown::parse_with(value, &names),
            })
            .collect(),
        footer: footer.map(Into::into),
        media: Vec::new(),
    }
}

pub(crate) fn msg(id: u64, channel_id: &str, author: &str, time: &str, content: &str) -> Message {
    let minutes = time
        .split_once(':')
        .and_then(|(h, m)| Some(h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?))
        .unwrap_or(0);
    let mut m = Message {
        id: id.to_string(),
        channel_id: channel_id.into(),
        author_id: format!("user-{author}"),
        author: author.into(),
        author_username: author.into(),
        author_bot: author == "Sylo",
        timestamp_ms: DEMO_DAY_MS + minutes * 60_000,
        timestamp: label(DEMO_DAY_MS + minutes * 60_000),
        content: content.into(),
        blocks: vec![],
        truncated: false,
        attachments: vec![],
        embeds: vec![],
        reply: None,
        mentions_me: content.contains("<@2001>"),
        edited: false,
    };
    m.parse_with(&demo_names());
    m
}

/// Midnight UTC, 10 November 2026: the demo's "today".
const DEMO_DAY_MS: i64 = 1_794_268_800_000;

pub(crate) fn label(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%d/%m/%Y, %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn live(id: u64, channel_id: &str, author: &str, content: &str) -> Message {
    let mut m = msg(id, channel_id, author, "", content);
    m.timestamp_ms = chrono::Utc::now().timestamp_millis();
    m.timestamp = now();
    m
}

fn now() -> String {
    chrono::Local::now().format("%d/%m/%Y, %H:%M").to_string()
}

impl MockSession {
    pub fn start(events: Ui) -> Self {
        let scenes::Data {
            guilds,
            channels,
            messages,
        } = scenes::data(scenes::current());
        let messages = Arc::new(Mutex::new(messages));
        let open_channel: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Pretend the gateway delivers a message now and then in the open channel.
        let ticker = {
            let events = events.clone();
            let open = open_channel.clone();
            let messages = messages.clone();
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
                    );
                    messages
                        .lock()
                        .unwrap()
                        .entry(channel_id)
                        .or_default()
                        .push(m.clone());
                    let _ = events.unbounded_send(msgs::Ev::MessageCreated(m));
                }
            })
        };

        let _ = events.unbounded_send(accounts::Ev::SignedIn {
            username: "demo".into(),
            instance: "demo".into(),
        });
        let _ = events.unbounded_send(msgs::Ev::Guilds(guilds));
        let _ = events.unbounded_send(accounts::Ev::Status("Demo mode".into()));

        Self {
            events,
            channels,
            messages,
            open_channel,
            next_id: 100,
            ticker,
        }
    }

    pub fn open_guild(&mut self, guild_id: Option<String>) {
        let channels = self.channels.get(&guild_id).cloned().unwrap_or_default();
        let _ = self
            .events
            .unbounded_send(msgs::Ev::Channels { guild_id, channels });
    }

    pub fn open_channel(&mut self, channel_id: String) {
        *self.open_channel.lock().unwrap() = Some(channel_id.clone());
        let messages = self
            .messages
            .lock()
            .unwrap()
            .get(&channel_id)
            .cloned()
            .unwrap_or_default();
        let _ = self.events.unbounded_send(msgs::Ev::Messages {
            channel_id,
            messages,
        });
    }

    pub fn send(&mut self, channel_id: String, text: String) {
        self.next_id += 1;
        let mut m = live(self.next_id, &channel_id, "demo", &text);
        m.id = format!("local-{}", self.next_id);
        self.messages
            .lock()
            .unwrap()
            .entry(channel_id)
            .or_default()
            .push(m.clone());
        let _ = self.events.unbounded_send(msgs::Ev::MessageCreated(m));
    }
}

impl Drop for MockSession {
    fn drop(&mut self) {
        self.ticker.abort();
    }
}
