// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo threads: an active thread and archived ones under `#general`, a forum with tags
//! and a media channel. Only the thread scenes add them, so the other scenes keep their
//! channel lists.
//!
//! The demo answers the thread commands from this data: pages of a thread, a page of
//! archived threads, a page of archived forum posts, first messages, and op 28 (with
//! counts for the posts the account read before).

use std::collections::HashMap;

use inlet_protocol as p;
use serde_json::{json, Value};

use super::{at, media, message_at, snowflake, user_id, MockSession, ME};
use crate::backend::messages::threads::{Cmd, Ev, ForumPage, PostPreview};
use crate::backend::messages::{self as msgs, parse};
use crate::model::{Channel, ChannelKind};

/// The active thread under `#general`.
pub const THREAD: &str = "t-release";
/// The forum and the media channel.
pub const FORUM: &str = "c-forum";
pub const MEDIA: &str = "c-gallery";

/// Forum tags: (demo ID, name, Unicode emoji).
const TAGS: [(&str, &str, &str); 3] = [
    ("tag-bug", "Bug", "\u{1F41B}"),
    ("tag-idea", "Idea", "\u{1F4A1}"),
    ("tag-help", "Help", ""),
];

/// A post or thread: (demo ID, parent, name, tags, archived, joined, read before).
struct Item {
    id: &'static str,
    parent: &'static str,
    name: &'static str,
    tags: &'static [&'static str],
    archived: bool,
    joined: bool,
    read: bool,
    pinned: bool,
}

const ITEMS: [Item; 10] = [
    Item {
        id: THREAD,
        parent: "c-general",
        name: "Release plans for 0.1",
        tags: &[],
        archived: false,
        joined: true,
        read: false,
        pinned: false,
    },
    Item {
        id: "t-old-design",
        parent: "c-general",
        name: "Sidebar design review",
        tags: &[],
        archived: true,
        joined: false,
        read: false,
        pinned: false,
    },
    Item {
        id: "t-old-ci",
        parent: "c-general",
        name: "CI on three platforms",
        tags: &[],
        archived: true,
        joined: false,
        read: false,
        pinned: false,
    },
    Item {
        id: "p-welcome",
        parent: FORUM,
        name: "Read me first: how this forum works",
        tags: &["tag-help"],
        archived: false,
        joined: false,
        read: true,
        pinned: true,
    },
    Item {
        id: "p-crash",
        parent: FORUM,
        name: "Crash when the window is resized on Wayland",
        tags: &["tag-bug"],
        archived: false,
        joined: false,
        read: true,
        pinned: false,
    },
    Item {
        id: "p-themes",
        parent: FORUM,
        name: "Custom themes with a contrast check",
        tags: &["tag-idea"],
        archived: false,
        joined: true,
        read: true,
        pinned: false,
    },
    Item {
        id: "p-proxy",
        parent: FORUM,
        name: "Images through a SOCKS proxy?",
        tags: &["tag-help", "tag-idea"],
        archived: false,
        joined: false,
        read: false,
        pinned: false,
    },
    Item {
        id: "p-old-login",
        parent: FORUM,
        name: "Login loop after a password change",
        tags: &["tag-bug"],
        archived: true,
        joined: false,
        read: false,
        pinned: false,
    },
    Item {
        id: "m-sunset",
        parent: MEDIA,
        name: "Sunset over the hills",
        tags: &[],
        archived: false,
        joined: false,
        read: false,
        pinned: false,
    },
    Item {
        id: "m-lighthouse",
        parent: MEDIA,
        name: "Lighthouse at dusk",
        tags: &[],
        archived: false,
        joined: false,
        read: false,
        pinned: false,
    },
];

/// The pictures of the media posts: (post, file, width, height).
const PICTURES: [(&str, &str, u32, u32); 2] = [
    ("m-sunset", "weekend.png", 1280, 720),
    ("m-lighthouse", "lighthouse.png", 600, 900),
];

/// Adds the forum and the media channel to the first guild, and the messages of the
/// threads and posts.
pub fn add(
    channels: &mut HashMap<Option<String>, Vec<Channel>>,
    messages: &mut HashMap<String, Vec<p::Message>>,
) {
    if let Some(list) = channels.get_mut(&Some("g1".to_string())) {
        list.push(super::channel(
            FORUM,
            "feedback",
            ChannelKind::Forum,
            Some("c-text"),
            6,
        ));
        list.push(super::channel(
            MEDIA,
            "gallery",
            ChannelKind::Media,
            Some("c-text"),
            7,
        ));
    }
    let m = |seq, ch: &str, who: &str, time: &str, text: &str| {
        message_at(seq, ch, who, at(time), text, json!({}))
    };
    messages.insert(
        THREAD.into(),
        vec![
            m(
                1,
                THREAD,
                "ada",
                "10:02",
                "Starting a thread so #general stays readable.",
            ),
            m(
                2,
                THREAD,
                "linus",
                "10:04",
                "What is left before we can tag it?",
            ),
            m(
                3,
                THREAD,
                "grace",
                "10:06",
                "Threads, forums and the visual checks. The rest is **done**.",
            ),
            m(4, THREAD, ME, "10:07", "I will take the thread panel."),
        ],
    );
    for (id, text) in [
        ("t-old-design", "The new sidebar is in."),
        ("t-old-ci", "All three platforms are green now."),
        (
            "p-welcome",
            "Search before you post, and pick a tag so others find your post.",
        ),
        (
            "p-crash",
            "Resizing the window quickly makes it close. It started this week.",
        ),
        (
            "p-themes",
            "A theme editor that checks the contrast of every colour.",
        ),
        ("p-proxy", "Can Inlet fetch images through a SOCKS proxy?"),
        ("p-old-login", "Solved: clearing the keychain entry helped."),
    ] {
        messages.insert(id.into(), vec![m(1, id, "ada", "09:00", text)]);
    }
    for (post, file, w, h) in PICTURES {
        let attachment = media::attachment(
            post,
            9_100 + u64::from(w),
            file,
            "image/png",
            json!({"width": w, "height": h, "size": 400_000}),
        );
        messages.insert(
            post.into(),
            vec![message_at(
                1,
                post,
                "grace",
                at("08:30"),
                "",
                json!({ "attachments": [attachment] }),
            )],
        );
    }
}

fn last_message(messages: &HashMap<String, Vec<p::Message>>, id: &str) -> Option<String> {
    messages
        .get(&snowflake(id))?
        .iter()
        .filter_map(|m| inlet_state::MessageId::parse(&m.id))
        .max()
        .map(|m| m.to_string())
}

fn item_json(item: &Item, messages: &HashMap<String, Vec<p::Message>>) -> Value {
    let count = messages.get(&snowflake(item.id)).map_or(0, Vec::len);
    let mut v = json!({
        "id": snowflake(item.id),
        "type": 11,
        "guild_id": snowflake("g1"),
        "parent_id": snowflake(item.parent),
        "name": item.name,
        "owner_id": user_id("ada"),
        "last_message_id": last_message(messages, item.id),
        "message_count": count.saturating_sub(1),
        "member_count": 3,
        "applied_tags": item.tags.iter().map(|t| snowflake(t)).collect::<Vec<_>>(),
        "flags": if item.pinned { 2 } else { 0 },
        "thread_metadata": {
            "archived": item.archived,
            "locked": false,
            "auto_archive_duration": 4320,
            "archive_timestamp": format!("2026-11-0{}T12:00:00Z", if item.archived { 5 } else { 9 }),
        },
    });
    if item.joined {
        v["member"] = json!({"join_timestamp": "2026-11-01T00:00:00Z", "flags": 1});
    }
    v
}

/// Adds the active threads and posts to the first guild of the READY, the tags of the
/// forum and media channel, and read states for the posts read before.
pub fn decorate(ready: &mut p::Ready, messages: &HashMap<String, Vec<p::Message>>) {
    let g1 = snowflake("g1");
    let Some(guild) = ready.guilds.iter_mut().find(|g| g.id == g1) else {
        return;
    };
    let tags: Vec<Value> = TAGS
        .iter()
        .map(|(id, name, emoji)| {
            json!({"id": snowflake(id), "name": name,
                   "emoji_name": (!emoji.is_empty()).then_some(*emoji)})
        })
        .collect();
    for ch in &mut guild.channels {
        if ch.id == snowflake(FORUM) || ch.id == snowflake(MEDIA) {
            ch.extra.insert("available_tags".into(), json!(tags));
        }
    }
    let active: Vec<Value> = ITEMS
        .iter()
        .filter(|i| !i.archived)
        .map(|i| item_json(i, messages))
        .collect();
    guild.extra.insert("threads".into(), Value::Array(active));
    for item in ITEMS.iter().filter(|i| i.read) {
        // Read up to the post itself: its messages after that are unread.
        ready.read_states.push(json!({
            "id": snowflake(item.id),
            "last_message_id": snowflake(item.id),
        }));
    }
}

impl MockSession {
    /// Answers a thread command from the demo data.
    pub fn thread_command(&mut self, cmd: Cmd) {
        let send = |events: &crate::backend::Ui, ev: Ev| {
            let _ = events.unbounded_send(msgs::Ev::Thread(ev));
        };
        match cmd {
            Cmd::OpenThread { thread_id, request } => {
                let (page, rendered) = self.page(&thread_id, None);
                send(
                    &self.events,
                    Ev::Page {
                        thread_id,
                        request,
                        page,
                        rendered,
                    },
                );
            }
            Cmd::LoadOlder {
                thread_id,
                before,
                request,
            } => {
                let before = inlet_state::MessageId::parse(&before);
                let (page, rendered) = self.page(&thread_id, before);
                send(
                    &self.events,
                    Ev::Page {
                        thread_id,
                        request,
                        page,
                        rendered,
                    },
                );
            }
            Cmd::Archived {
                channel_id,
                request,
                ..
            } => {
                let threads = self.items(&channel_id, true);
                send(
                    &self.events,
                    Ev::Archived {
                        channel_id,
                        request,
                        result: Ok((threads, false)),
                    },
                );
            }
            Cmd::ForumPage {
                forum_id, request, ..
            } => {
                let posts = self.items(&forum_id, true);
                let ids: Vec<String> = posts.iter().map(|p| p.id.clone()).collect();
                let previews = self.previews(&ids);
                send(
                    &self.events,
                    Ev::ForumPage {
                        forum_id,
                        request,
                        result: Ok(ForumPage {
                            posts,
                            previews,
                            has_more: false,
                        }),
                    },
                );
            }
            Cmd::PostData {
                forum_id,
                thread_ids,
            } => {
                let previews = self.previews(&thread_ids);
                send(&self.events, Ev::PostData { forum_id, previews });
            }
            Cmd::ForumUnreads {
                guild_id,
                forum_id,
                posts,
            } => {
                let threads: Vec<Value> = posts
                    .iter()
                    .map(|p| json!({"thread_id": p.thread_id, "count": 2}))
                    .collect();
                let dispatch = p::Dispatch::parse(
                    "FORUM_UNREADS",
                    json!({"guild_id": guild_id, "channel_id": forum_id, "threads": threads}),
                );
                let _ = self.events.unbounded_send(msgs::Ev::Dispatch {
                    dispatch: Box::new(dispatch),
                    rendered: Vec::new(),
                });
            }
            Cmd::Subscribe { .. } => {}
        }
    }

    /// The threads of a channel (archived ones, or active ones).
    fn items(&self, parent: &str, archived: bool) -> Vec<p::Channel> {
        let messages = self.messages.lock().unwrap();
        ITEMS
            .iter()
            .filter(|i| snowflake(i.parent) == parent && i.archived == archived)
            .filter_map(|i| serde_json::from_value(item_json(i, &messages)).ok())
            .collect()
    }

    /// Previews of the first messages of posts.
    fn previews(&self, ids: &[String]) -> Vec<(String, PostPreview)> {
        let messages = self.messages.lock().unwrap();
        ids.iter()
            .filter_map(|id| {
                let first = messages.get(id)?.first()?;
                Some((
                    id.clone(),
                    PostPreview::of(&parse::to_message(first, &self.names)),
                ))
            })
            .collect()
    }
}
