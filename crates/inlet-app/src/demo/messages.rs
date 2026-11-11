// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo messages for the guild channels: markdown of every kind, mentions (of the demo
//! account and of its role), replies, an edited message, a date separator, attachments,
//! a bot, embed-only bot messages, every system message type and one Inlet does not
//! know.

use std::collections::HashMap;

use inlet_protocol as p;
use serde_json::json;

use super::{at, channel_ref, embed, message_at, msg, msg_with, reply_to, user_id, ME};

/// Adds the messages of the guild channels.
pub fn messages(messages: &mut HashMap<String, Vec<p::Message>>) {
    messages.insert("c-general".into(), general());
    messages.insert("c-joins".into(), joins());
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
    messages.insert("c-system".into(), system());
    messages.insert("c-replies".into(), replies());
}

fn general() -> Vec<p::Message> {
    let first = msg(
        1,
        "c-general",
        "ada",
        "09:12",
        "Morning! Has anyone tried the **native client** yet?",
    );
    vec![
        // An older message on another day, to show the date separator.
        message_at(
            0,
            "c-general",
            "ada",
            at("09:00") - 24 * 60 * 60 * 1000,
            "Yesterday's message.",
            json!({}),
        ),
        first.clone(),
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
        msg_with(
            7,
            "c-general",
            "ada",
            "09:21",
            "Attachments show as images.",
            json!({"attachments": [super::media::screenshot()]}),
        ),
        // Same display name, other account: its own header, and @username shows.
        msg_with(
            9,
            "c-general",
            "ada",
            "09:21",
            "Same name, other account.",
            json!({
                "author": {"id": "2099", "username": "not_ada", "global_name": "ada"},
                "member": {"roles": []}
            }),
        ),
        msg_with(
            11,
            "c-general",
            "linus",
            "09:22",
            &format!(
                "Replying: mentions work too \u{2014} <@{}>, {}, <@&3001>, <#999> and <:wave:123456789012345678>.",
                user_id("ada"),
                channel_ref("c-general")
            ),
            reply_to(&first),
        ),
        msg_with(
            12,
            "c-general",
            "grace",
            "09:23",
            "This one was edited.",
            json!({"edited_timestamp": "2026-11-10T09:25:00+00:00"}),
        ),
        msg(
            13,
            "c-general",
            "Sylo",
            "09:24",
            "Bots carry a tag next to their name.",
        ),
        msg(
            14,
            "c-general",
            "linus",
            "09:26",
            &format!("<@{}> could you look at the new build?", user_id(ME)),
        ),
        msg(15, "c-general", "linus", "09:26", "No rush."),
    ]
}

/// A bot channel like a real server's: embed-only messages and one without anything.
fn joins() -> Vec<p::Message> {
    vec![
        msg_with(
            30,
            "c-joins",
            "Sylo",
            "08:00",
            "",
            json!({"embeds": [
                embed(0xF1C40F, "Welcome to IWGamin's Pack!", "A hub for friends and people who want to play together.\n\nFind someone to team up with, hang out in voice and join in on game nights.", &[], None),
                embed(0x5865F2, "\u{1F680} Getting started", &format!("1. Read the rules below.\n2. React with \u{2705} in {} to unlock the server.\n3. Say hi in {}.", channel_ref("c-welcome"), channel_ref("c-general")), &[], None),
                embed(0xED4245, "\u{1F4CB} Server rules", "1. Be respectful.\n2. No spam.", &[("Need help?", "Send a direct message to **Sylo** to open a ticket."), ("Links", "Leaderboard \u{2014} https://example.com/leaderboard")], Some("Sylo")),
            ]}),
        ),
        msg(31, "c-joins", "Sylo", "08:01", ""),
    ]
}

/// Every system message type, and one Inlet does not know.
fn system() -> Vec<p::Message> {
    let with =
        |seq: u64, author: &str, time: &str, kind: i64, content: &str, extra: serde_json::Value| {
            let mut fields = json!({ "type": kind });
            if let (Some(obj), Some(more)) = (fields.as_object_mut(), extra.as_object()) {
                for (k, v) in more {
                    obj.insert(k.clone(), v.clone());
                }
            }
            msg_with(seq, "c-system", author, time, content, fields)
        };
    let grace = super::user("grace");
    vec![
        with(40, "linus", "10:00", 7, "", json!({})),
        with(41, "ada", "10:02", 6, "", json!({})),
        msg(
            42,
            "c-system",
            "ada",
            "10:03",
            "Pinned the release notes above.",
        ),
        with(43, "ada", "10:05", 1, "", json!({ "mentions": [grace] })),
        with(44, "grace", "10:06", 2, "", json!({})),
        with(45, "linus", "10:08", 3, "", json!({})),
        with(46, "ada", "10:10", 4, "release-planning", json!({})),
        with(47, "ada", "10:11", 5, "", json!({})),
        with(
            48,
            "Inlet",
            "10:12",
            12,
            "Fluxer News #announcements",
            json!({}),
        ),
        with(49, "grace", "10:14", 18, "Release planning", json!({})),
        with(50, "grace", "10:15", 21, "", json!({})),
        with(
            51,
            "linus",
            "10:16",
            4242,
            "Whatever a newer server sends",
            json!({}),
        ),
    ]
}

/// Replies: one whose original is loaded, one whose original was deleted, one the
/// server did not include, and a chain.
fn replies() -> Vec<p::Message> {
    let question = msg(
        60,
        "c-replies",
        "ada",
        "11:00",
        "Has anyone tried the new build on Windows?",
    );
    let answer = msg_with(
        61,
        "c-replies",
        "grace",
        "11:02",
        "Yes, it starts in under a second here.",
        reply_to(&question),
    );
    let chained = msg_with(
        62,
        "c-replies",
        "linus",
        "11:03",
        "Same on my laptop. Memory stays low too.",
        reply_to(&answer),
    );
    let deleted = msg_with(
        63,
        "c-replies",
        "ada",
        "11:05",
        "Replying to a message that is gone now.",
        json!({
            "type": 19,
            "message_reference": {"message_id": "1234567890123456789"},
            "referenced_message": null
        }),
    );
    let unknown = msg_with(
        64,
        "c-replies",
        "linus",
        "11:06",
        "And one the server left out.",
        json!({"type": 19, "message_reference": {"message_id": "1234567890123456790"}}),
    );
    let mine = msg_with(
        65,
        "c-replies",
        ME,
        "11:08",
        "Thanks, merging it then.",
        reply_to(&chained),
    );
    vec![question, answer, chained, deleted, unknown, mine]
}
