// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo data, compiled only with the `demo` cargo feature and used only when the
//! `INLET_DEMO` environment variable is set. Lets the UI be tried and screenshotted
//! without an account. It touches neither the network nor the keychain.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{Event, Ui};
use crate::model::{
    Attachment, Channel, ChannelKind, Embed, EmbedField, Guild, Message, Names, Reply,
};

pub struct MockSession {
    events: Ui,
    channels: HashMap<Option<String>, Vec<Channel>>,
    messages: Arc<Mutex<HashMap<String, Vec<Message>>>>,
    open_channel: Arc<Mutex<Option<String>>>,
    next_id: u64,
    ticker: tokio::task::JoinHandle<()>,
}

fn channel(
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
fn demo_names() -> Names {
    let mut n = Names::default();
    n.channels.insert("1001".into(), "verify".into());
    n.channels.insert("1002".into(), "general".into());
    n.users.insert("2001".into(), "ada".into());
    n.roles.insert("3001".into(), "Moderators".into());
    n
}

fn embed(
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
        description: crate::markdown::parse_with(description, &names),
        fields: fields
            .iter()
            .map(|(name, value)| EmbedField {
                name: (*name).into(),
                value: crate::markdown::parse_with(value, &names),
            })
            .collect(),
        footer: footer.map(Into::into),
        media: Vec::new(),
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
        author_bot: author == "Sylo",
        timestamp_ms: DEMO_DAY_MS + minutes * 60_000,
        timestamp: label(DEMO_DAY_MS + minutes * 60_000),
        content: content.into(),
        blocks: vec![],
        truncated: false,
        attachments: vec![],
        embeds: vec![],
        reply: None,
        edited: false,
    };
    m.parse_with(&demo_names());
    m
}

/// Midnight UTC, 10 November 2026: the demo's "today".
const DEMO_DAY_MS: i64 = 1_794_268_800_000;

fn label(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
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
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

impl MockSession {
    pub fn start(events: Ui) -> Self {
        let guilds = vec![
            Guild {
                id: "g1".into(),
                name: "Inlet Dev".into(),
            },
            Guild {
                id: "g2".into(),
                name: "Rust Norway".into(),
            },
            Guild {
                id: "g3".into(),
                name: "Self Hosting".into(),
            },
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
                channel("c-music", "\u{1F50A} Music", ChannelKind::Voice, Some("c-voice"), 1),
                // Names like real servers have: emoji, very long names, non-Latin text.
                channel("c-community", "Community \u{1F30D}", ChannelKind::Category, None, 3),
                channel("c-ann", "\u{1F389}-announcements", ChannelKind::Text, Some("c-community"), 0),
                channel(
                    "c-rules",
                    "\u{1F4DC}-rules-and-information-for-everyone-who-joins-the-server-please-read",
                    ChannelKind::Text,
                    Some("c-community"),
                    1,
                ),
                channel("c-ja", "\u{65E5}\u{672C}\u{8A9E}-\u{30C1}\u{30E3}\u{30C3}\u{30C8}", ChannelKind::Text, Some("c-community"), 2),
                channel("c-ru", "\u{0440}\u{0443}\u{0441}\u{0441}\u{043A}\u{0438}\u{0439}-\u{0447}\u{0430}\u{0442}", ChannelKind::Text, Some("c-community"), 3),
                channel("c-ar", "\u{0627}\u{0644}\u{0639}\u{0631}\u{0628}\u{064A}\u{0629}", ChannelKind::Text, Some("c-community"), 4),
                channel("c-memes", "memes \u{1F602}", ChannelKind::Text, Some("c-community"), 5),
                channel("c-help", "help-desk", ChannelKind::Text, Some("c-community"), 6),
                channel("c-offtopic", "off-topic", ChannelKind::Text, Some("c-community"), 7),
                channel("c-bots", "bot-commands \u{2699}\u{FE0F}", ChannelKind::Text, Some("c-community"), 8),
                channel("c-sylo", "Sylo", ChannelKind::Category, None, 4),
                channel("c-joins", "New Members", ChannelKind::Text, Some("c-sylo"), 0),
                channel("c-logs", "sylo-logs", ChannelKind::Text, Some("c-sylo"), 1),
                channel("c-mod", "moderation-log", ChannelKind::Text, Some("c-sylo"), 2),
                channel("c-tickets", "\u{1F3AB}-tickets", ChannelKind::Text, Some("c-sylo"), 3),
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
                channel(
                    "dm-group",
                    "ada, linus, grace",
                    ChannelKind::GroupDm,
                    None,
                    1,
                ),
            ],
        );

        let mut messages: HashMap<String, Vec<Message>> = HashMap::new();
        let mut general = vec![
            msg(
                1,
                "c-general",
                "ada",
                "09:12",
                "Morning! Has anyone tried the **native client** yet?",
            ),
            msg(
                2,
                "c-general",
                "linus",
                "09:13",
                "Yes. It is *fast*. Scrolling is ~~janky~~ smooth.",
            ),
            msg(
                3,
                "c-general",
                "grace",
                "09:15",
                "Inline code works: `cargo run -p inlet`",
            ),
            msg(
                4,
                "c-general",
                "ada",
                "09:16",
                "Code block:\n```rust\nfn main() {\n    println!(\"hello, Fluxer\");\n}\n```",
            ),
            msg(
                5,
                "c-general",
                "linus",
                "09:18",
                "> Quotes render too\nand links: https://fluxer.app",
            ),
            msg(
                6,
                "c-general",
                "grace",
                "09:20",
                "||spoiler text|| and a list:\n- one\n- two",
            ),
            msg(
                8,
                "c-general",
                "ada",
                "09:20",
                "Masked link: [Confirm your account](https://fluxer-login.example/verify)",
            ),
        ];
        general.push(Message {
            attachments: vec![Attachment {
                filename: "screenshot.png".into(),
                url: "https://example.invalid/attachments/screenshot.png".into(),
            }],
            ..msg(
                7,
                "c-general",
                "ada",
                "09:21",
                "Attachments show as links in v0.",
            )
        });
        // Same display name, different account: gets its own header and shows @username.
        general.push(Message {
            author_id: "user-impostor".into(),
            author_username: "not_ada".into(),
            ..msg(9, "c-general", "ada", "09:21", "Same name, other account.")
        });
        general.push(Message {
            reply: Some(Reply { author: "ada".into(), snippet: "Morning! Has anyone tried the native client yet?".into() }),
            ..msg(11, "c-general", "linus", "09:22", "Replying: mentions work too \u{2014} <@2001>, <#1002>, <@&3001>, <#999> and <:wave:123456789012345678>.")
        });
        let mut edited = msg(12, "c-general", "grace", "09:23", "This one was edited.");
        edited.edited = true;
        edited.parse_with(&demo_names());
        general.push(edited);
        // An older message on another day, to show the date separator.
        let mut older = msg(0, "c-general", "ada", "09:00", "Yesterday's message.");
        older.timestamp_ms -= 24 * 60 * 60 * 1000;
        older.timestamp = label(older.timestamp_ms);
        general.insert(0, older);
        messages.insert("c-general".into(), general);
        // A bot channel like a real server's: embed-only messages and one without anything.
        let joins = vec![
            Message {
            embeds: vec![
                embed(0xF1C40F, "Welcome to IWGamin's Pack!", "A hub for friends and people who want to play together.\n\nFind someone to team up with, hang out in voice and join in on game nights.", &[], None),
                embed(0x5865F2, "\u{1F680} Getting started", "1. Read the rules below.\n2. React with \u{2705} in <#1001> to unlock the server.\n3. Say hi in <#1002>.", &[], None),
                embed(0xED4245, "\u{1F4CB} Server rules", "1. Be respectful.\n2. No spam.", &[("Need help?", "Send a direct message to **Sylo** to open a ticket."), ("Links", "Leaderboard \u{2014} https://example.com/leaderboard")], Some("Sylo")),
            ],
            ..msg(30, "c-joins", "Sylo", "08:00", "")
            },
            msg(31, "c-joins", "Sylo", "08:01", ""),
        ];
        messages.insert("c-joins".into(), joins);
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
            vec![msg(
                20,
                "dm-ada",
                "ada",
                "10:01",
                "Hi! This is a direct message.",
            )],
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
                    let _ = events.unbounded_send(Event::MessageCreated(m));
                }
            })
        };

        let _ = events.unbounded_send(Event::SignedIn {
            username: "demo".into(),
            instance: "demo".into(),
        });
        let _ = events.unbounded_send(Event::Guilds(guilds));
        let _ = events.unbounded_send(Event::Status("Demo mode".into()));

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
            .unbounded_send(Event::Channels { guild_id, channels });
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
        let _ = self.events.unbounded_send(Event::Messages {
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
        let _ = self.events.unbounded_send(Event::MessageCreated(m));
    }
}

impl Drop for MockSession {
    fn drop(&mut self) {
        self.ticker.abort();
    }
}
