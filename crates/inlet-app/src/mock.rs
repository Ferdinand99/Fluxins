// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo data, compiled only with the `demo` cargo feature and used only when the
//! `INLET_DEMO` environment variable is set. Lets the UI be tried and screenshotted
//! without an account. It touches neither the network nor the keychain.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;

use crate::backend::Event;
use crate::model::{Attachment, Channel, ChannelKind, Guild, Message};

pub struct MockSession {
    events: UnboundedSender<Event>,
    channels: HashMap<Option<String>, Vec<Channel>>,
    messages: Arc<Mutex<HashMap<String, Vec<Message>>>>,
    open_channel: Arc<Mutex<Option<String>>>,
    next_id: u64,
    ticker: tokio::task::JoinHandle<()>,
}

fn channel(id: &str, name: &str, kind: ChannelKind, parent: Option<&str>, position: i64) -> Channel {
    Channel {
        id: id.into(),
        name: name.into(),
        kind,
        parent_id: parent.map(Into::into),
        position,
    }
}

fn msg(id: u64, channel_id: &str, author: &str, time: &str, content: &str) -> Message {
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
        timestamp_ms: 1_794_000_000_000 + minutes * 60_000,
        timestamp: format!("2026-11-10 {time}"),
        content: content.into(),
        blocks: vec![],
        truncated: false,
        attachments: vec![],
        edited: false,
    };
    m.parse();
    m
}

fn live(id: u64, channel_id: &str, author: &str, content: &str) -> Message {
    let mut m = msg(id, channel_id, author, "", content);
    m.timestamp_ms = chrono::Utc::now().timestamp_millis();
    m.timestamp = now();
    m
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

impl MockSession {
    pub fn start(events: UnboundedSender<Event>) -> Self {
        let guilds = vec![
            Guild { id: "g1".into(), name: "Inlet Dev".into() },
            Guild { id: "g2".into(), name: "Rust Norway".into() },
            Guild { id: "g3".into(), name: "Self Hosting".into() },
        ];
        let mut channels = HashMap::new();
        channels.insert(
            Some("g1".to_string()),
            vec![
                channel("c-info", "Information", ChannelKind::Category, None, 0),
                channel("c-welcome", "welcome", ChannelKind::Text, Some("c-info"), 0),
                channel("c-text", "Text channels", ChannelKind::Category, None, 1),
                channel("c-general", "general", ChannelKind::Text, Some("c-text"), 0),
                channel("c-dev", "development", ChannelKind::Text, Some("c-text"), 1),
                channel("c-voice", "Voice", ChannelKind::Category, None, 2),
                channel("c-lounge", "Lounge", ChannelKind::Voice, Some("c-voice"), 0),
            ],
        );
        channels.insert(
            Some("g2".to_string()),
            vec![channel("r-general", "general", ChannelKind::Text, None, 0)],
        );
        channels.insert(
            Some("g3".to_string()),
            vec![channel("s-general", "general", ChannelKind::Text, None, 0)],
        );
        channels.insert(
            None,
            vec![
                channel("dm-ada", "ada", ChannelKind::Dm, None, 0),
                channel("dm-group", "ada, linus, grace", ChannelKind::GroupDm, None, 1),
            ],
        );

        let mut messages: HashMap<String, Vec<Message>> = HashMap::new();
        let mut general = vec![
            msg(1, "c-general", "ada", "09:12", "Morning! Has anyone tried the **native client** yet?"),
            msg(2, "c-general", "linus", "09:13", "Yes. It is *fast*. Scrolling is ~~janky~~ smooth."),
            msg(3, "c-general", "grace", "09:15", "Inline code works: `cargo run -p inlet`"),
            msg(
                4,
                "c-general",
                "ada",
                "09:16",
                "Code block:\n```rust\nfn main() {\n    println!(\"hello, Fluxer\");\n}\n```",
            ),
            msg(5, "c-general", "linus", "09:18", "> Quotes render too\nand links: https://fluxer.app"),
            msg(6, "c-general", "grace", "09:20", "||spoiler text|| and a list:\n- one\n- two"),
            msg(8, "c-general", "ada", "09:20", "Masked link: [Confirm your account](https://fluxer-login.example/verify)"),
        ];
        general.push(Message {
            attachments: vec![Attachment {
                filename: "screenshot.png".into(),
                url: "https://example.invalid/attachments/screenshot.png".into(),
            }],
            ..msg(7, "c-general", "ada", "09:21", "Attachments show as links in v0.")
        });
        // Same display name, different account: gets its own header and shows @username.
        general.push(Message {
            author_id: "user-impostor".into(),
            author_username: "not_ada".into(),
            ..msg(9, "c-general", "ada", "09:21", "Same name, other account.")
        });
        messages.insert("c-general".into(), general);
        messages.insert(
            "c-welcome".into(),
            vec![msg(
                10,
                "c-welcome",
                "Inlet",
                "08:00",
                "# Welcome\nThis is **demo data**. Nothing is sent anywhere.",
            )],
        );
        messages.insert(
            "dm-ada".into(),
            vec![msg(20, "dm-ada", "ada", "10:01", "Hi! This is a direct message.")],
        );

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
                    let Some(channel_id) = open.lock().unwrap().clone() else { continue };
                    n += 1;
                    let m = live(1000 + n, &channel_id, "linus", &format!("Live message #{n} from the demo gateway"));
                    messages.lock().unwrap().entry(channel_id).or_default().push(m.clone());
                    let _ = events.unbounded_send(Event::MessageCreated(m));
                }
            })
        };

        let _ = events.unbounded_send(Event::SignedIn { username: "demo".into(), instance: "demo".into() });
        let _ = events.unbounded_send(Event::Guilds(guilds));
        let _ = events.unbounded_send(Event::Status("Demo mode".into()));

        Self { events, channels, messages, open_channel, next_id: 100, ticker }
    }

    pub fn open_guild(&mut self, guild_id: Option<String>) {
        let channels = self.channels.get(&guild_id).cloned().unwrap_or_default();
        let _ = self.events.unbounded_send(Event::Channels { guild_id, channels });
    }

    pub fn open_channel(&mut self, channel_id: String) {
        *self.open_channel.lock().unwrap() = Some(channel_id.clone());
        let messages = self.messages.lock().unwrap().get(&channel_id).cloned().unwrap_or_default();
        let _ = self.events.unbounded_send(Event::Messages { channel_id, messages });
    }

    pub fn send(&mut self, channel_id: String, text: String) {
        self.next_id += 1;
        let mut m = live(self.next_id, &channel_id, "demo", &text);
        m.id = format!("local-{}", self.next_id);
        self.messages.lock().unwrap().entry(channel_id).or_default().push(m.clone());
        let _ = self.events.unbounded_send(Event::MessageCreated(m));
    }
}

impl Drop for MockSession {
    fn drop(&mut self) {
        self.ticker.abort();
    }
}
