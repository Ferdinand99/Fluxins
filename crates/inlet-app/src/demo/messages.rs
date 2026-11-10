// SPDX-License-Identifier: AGPL-3.0-or-later
//! Demo messages for the guild channels: markdown of every kind, mentions, replies,
//! an edited message, a date separator, attachments and embed-only bot messages.

use std::collections::HashMap;

use super::{embed, label, msg};
use crate::model::{Attachment, Message, Reply};

/// Adds the messages of the guild channels.
pub fn messages(messages: &mut HashMap<String, Vec<Message>>) {
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
    edited.parse_with(&super::demo_names());
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
}
