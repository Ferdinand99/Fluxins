// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of the text model: buffer, tokens, selection, undo and triggers. No window needed.

use inlet_editor::{
    line_end, line_start, nearest_grapheme_boundary, next_grapheme, next_word,
    prev_backspace_boundary, prev_grapheme, prev_word, server_mentions, trigger_at,
    AllowedMentions, Buffer, Fragment, Id, Mentions, Move, NoNames, Resolve, Selection, Token,
    Trigger,
};

struct Names;

impl Resolve for Names {
    fn user(&self, id: Id) -> Option<String> {
        match id.0 {
            1 => Some("alice".into()),
            7 => Some("u".into()),
            _ => None,
        }
    }
    fn role(&self, id: Id) -> Option<String> {
        (id.0 == 2).then(|| "mods".into())
    }
    fn channel(&self, id: Id) -> Option<String> {
        match id.0 {
            3 => Some("general".into()),
            _ => None,
        }
    }
}

fn typed(buf: &mut Buffer, s: &str) {
    for ch in s.chars() {
        let at = buf.len();
        buf.insert(at, &ch.to_string()).unwrap();
    }
}

fn user_token(buf: &mut Buffer, at: usize, id: u64, label: &str) {
    assert!(buf.insert_token(at, Token::User(Id(id)), label));
}

// --- wire text ------------------------------------------------------------------------------

#[test]
fn wire_round_trip_keeps_every_token_kind() {
    let wire = "hi <@1> <@&2> <#3> <:smile:4> <a:dance:5> @everyone @here ok";
    let buf = Buffer::from_wire(wire, &Names);
    assert_eq!(
        buf.text(),
        "hi @alice @mods #general :smile: :dance: @everyone @here ok"
    );
    let kinds: Vec<&Token> = buf.tokens().iter().map(|t| &t.token).collect();
    assert_eq!(
        kinds,
        vec![
            &Token::User(Id(1)),
            &Token::Role(Id(2)),
            &Token::Channel(Id(3)),
            &Token::CustomEmoji {
                id: Id(4),
                name: "smile".into(),
                animated: false
            },
            &Token::CustomEmoji {
                id: Id(5),
                name: "dance".into(),
                animated: true
            },
            &Token::Everyone,
            &Token::Here,
        ]
    );
    // Each token covers exactly its label.
    for span in buf.tokens() {
        assert!(buf.text().is_char_boundary(span.range.start));
        assert!(buf.text().is_char_boundary(span.range.end));
    }
    assert_eq!(
        buf.text().get(buf.tokens()[0].range.clone()),
        Some("@alice")
    );
    assert_eq!(buf.to_wire(), wire);

    // And again from what the buffer produced.
    let again = Buffer::from_wire(&buf.to_wire(), &Names);
    assert_eq!(again.text(), buf.text());
    assert_eq!(again.tokens(), buf.tokens());
}

#[test]
fn unknown_names_get_placeholder_labels() {
    let buf = Buffer::from_wire("<@99> <#98>", &NoNames);
    assert_eq!(buf.text(), "@unknown-user #unknown-channel");
    assert_eq!(buf.to_wire(), "<@99> <#98>");
}

#[test]
fn legacy_nickname_mention_reads_as_a_user() {
    let buf = Buffer::from_wire("<@!1>", &Names);
    assert_eq!(buf.tokens()[0].token, Token::User(Id(1)));
}

#[test]
fn text_that_looks_like_a_mention_is_escaped() {
    let mut buf = Buffer::new(2000);
    typed(
        &mut buf,
        "<@123> and <#4> and <:e:5> and <a:e:5> and @everyone and @here",
    );
    assert_eq!(
        buf.to_wire(),
        "\\<@123> and \\<#4> and \\<:e:5> and \\<a:e:5> and \\@everyone and \\@here"
    );
    // Reading it back gives the same plain text, and no token.
    let back = Buffer::from_wire(&buf.to_wire(), &Names);
    assert_eq!(back.text(), buf.text());
    assert!(back.tokens().is_empty());
}

#[test]
fn ordinary_text_is_not_touched() {
    let mut buf = Buffer::new(2000);
    typed(
        &mut buf,
        "mail me at a@b.c, 1 < 2 > 0, <b>bold</b>, c:\\dir\\ and \\n",
    );
    assert_eq!(
        buf.to_wire(),
        "mail me at a@b.c, 1 < 2 > 0, <b>bold</b>, c:\\dir\\ and \\n"
    );
}

#[test]
fn backslashes_before_a_mention_cannot_cancel_the_escape() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "\\<@1>");
    // Three backslashes: a literal backslash, then an escaped `<`.
    assert_eq!(buf.to_wire(), "\\\\\\<@1>");
    let back = Buffer::from_wire(&buf.to_wire(), &Names);
    assert_eq!(back.text(), "\\<@1>");
    assert!(back.tokens().is_empty());
}

#[test]
fn a_token_after_a_backslash_still_reads_back_as_a_token() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "a\\");
    user_token(&mut buf, 2, 1, "@alice");
    assert_eq!(buf.to_wire(), "a\\\\<@1>");
    let back = Buffer::from_wire(&buf.to_wire(), &Names);
    assert_eq!(back.text(), buf.text());
    assert_eq!(back.tokens(), buf.tokens());
}

#[test]
fn a_crafted_emoji_name_cannot_open_a_mention() {
    let mut buf = Buffer::new(2000);
    let evil = Token::CustomEmoji {
        id: Id(9),
        name: "x:1><@123".into(),
        animated: false,
    };
    assert!(buf.insert_token(0, evil, ":x:"));
    let wire = buf.to_wire();
    assert!(!wire.contains("<@123>"), "{wire}");
    assert_eq!(wire, "<:x1123:9>");
}

/// What a Fluxer server reads as mentions in `wire`. This is a port of the server's own
/// extraction (`fluxer_messages::mention_extractor`), written for the test on purpose: it
/// uses the real `fluxer_markdown_parser` but not the editor's own code. The server first
/// removes U+202E and U+000C from the content and trims it (`normalizeString`).
fn fluxer_mentions(wire: &str) -> Mentions {
    use fluxer_markdown_parser::ast::{ListItem, MentionKind};
    use fluxer_markdown_parser::{EmojiContext, MarkdownParser, Node, ParserFlags};
    use linkify::{LinkFinder, LinkKind};

    fn walk(node: &Node, m: &mut Mentions) {
        let each = |nodes: &[Node], m: &mut Mentions| nodes.iter().for_each(|n| walk(n, m));
        let ids = |id: &str, set: &mut std::collections::BTreeSet<u64>| {
            if let Ok(id) = id.parse::<i64>() {
                if id != 0 {
                    set.insert(id as u64);
                }
            }
        };
        match node {
            Node::Mention { kind } => match kind {
                MentionKind::User { id } => ids(id, &mut m.users),
                MentionKind::Role { id } => ids(id, &mut m.roles),
                MentionKind::Channel { id } => ids(id, &mut m.channels),
                MentionKind::Everyone => m.everyone = true,
                MentionKind::Here => m.here = true,
                _ => {}
            },
            Node::Blockquote { children, .. }
            | Node::Strong { children }
            | Node::Emphasis { children }
            | Node::Underline { children }
            | Node::Strikethrough { children }
            | Node::Heading { children, .. }
            | Node::Subtext { children }
            | Node::Sequence { children }
            | Node::TableCell { children }
            | Node::Alert { children, .. }
            | Node::Spoiler { children, .. } => each(children, m),
            Node::List { items, .. } => items
                .iter()
                .for_each(|ListItem { children, .. }| each(children, m)),
            Node::Link { text, .. } => {
                if let Some(text) = text {
                    walk(text, m);
                }
            }
            Node::Table { header, rows, .. } => {
                walk(header, m);
                each(rows, m);
            }
            Node::TableRow { cells } => each(cells, m),
            Node::Text { .. }
            | Node::CodeBlock { .. }
            | Node::InlineCode { .. }
            | Node::Timestamp { .. }
            | Node::Emoji { .. } => {}
        }
    }

    let normalized: String = wire
        .chars()
        .filter(|c| *c != '\u{202e}' && *c != '\u{000c}')
        .collect();
    let trimmed = normalized.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let mut blanked = trimmed.to_owned();
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);
    for link in finder.links(trimmed) {
        let end = trimmed[link.end()..]
            .char_indices()
            .find_map(|(i, ch)| ch.is_whitespace().then_some(link.end() + i))
            .unwrap_or(trimmed.len());
        blanked.replace_range(link.start()..end, &" ".repeat(end - link.start()));
    }
    let mut mentions = Mentions::default();
    let mut parser = MarkdownParser::new(ParserFlags::ALL, EmojiContext::default());
    if let Ok(nodes) = parser.parse(&blanked) {
        nodes.iter().for_each(|n| walk(n, &mut mentions));
    }
    mentions
}

/// Every sequence of up to `max_len` of `count` pieces, one at a time.
fn for_each_sequence(count: usize, max_len: usize, mut f: impl FnMut(&[usize])) {
    let mut stack: Vec<Vec<usize>> = vec![vec![]];
    while let Some(seq) = stack.pop() {
        if !seq.is_empty() {
            f(&seq);
        }
        if seq.len() < max_len {
            for p in 0..count {
                let mut next = seq.clone();
                next.push(p);
                stack.push(next);
            }
        }
    }
}

/// An `@everyone` or `@here` token right after a typed backslash cannot be sent as a ping
/// (the backslash would escape it). That is the safe direction, so it may go missing.
fn lost(buf: &Buffer, t: &inlet_editor::TokenSpan) -> bool {
    // The character before the token, once the server has removed U+202E and U+000C.
    let before = buf.text()[..t.range.start]
        .chars()
        .rev()
        .find(|c| *c != '\u{202e}' && *c != '\u{000c}');
    matches!(t.token, Token::Everyone | Token::Here) && before == Some('\\')
}

fn picked_without_lost(buf: &Buffer) -> Mentions {
    let kept: Vec<_> = buf
        .tokens()
        .iter()
        .filter(|t| !lost(buf, t))
        .cloned()
        .collect();
    Mentions::picked(&kept)
}

#[test]
fn the_server_only_sees_the_mentions_the_user_picked() {
    // Typed text and picked tokens, every combination of up to four pieces. The text pieces
    // include the characters the server removes (U+202E, U+000C), other bidi controls and
    // control characters. The editor drops those on the way in, and the checks run against
    // the real markdown parser.
    #[derive(Clone, Copy)]
    enum Piece {
        Text(&'static str),
        User,
        Role,
        Everyone,
    }
    use Piece::*;
    let pieces = [
        Text("\\"),
        Text("<"),
        Text("@"),
        Text("<@"),
        Text("7>"),
        Text("everyone"),
        Text("<@7>"),
        Text("<@&8>"),
        Text("<#9>"),
        Text("x"),
        Text("\u{202e}"),
        Text("\u{000c}"),
        Text("\u{202a}"),
        Text("\u{0007}"),
        User,
        Role,
        Everyone,
    ];
    let mut checked = 0usize;
    for_each_sequence(pieces.len(), 4, |seq| {
        let mut buf = Buffer::new(10_000);
        for &p in seq {
            let at = buf.len();
            match pieces[p] {
                Text(s) => buf.insert(at, s).unwrap(),
                User => assert!(buf.insert_token(at, Token::User(Id(7)), "@u")),
                Role => assert!(buf.insert_token(at, Token::Role(Id(8)), "@r")),
                Everyone => assert!(buf.insert_token(at, Token::Everyone, "@everyone")),
            }
        }
        assert!(
            !buf.text()
                .chars()
                .any(|c| matches!(c, '\u{202a}'..='\u{202e}' | '\u{000c}' | '\u{0007}')),
            "text {:?}",
            buf.text()
        );
        let wire = buf.to_wire();
        assert_eq!(
            fluxer_mentions(&wire),
            picked_without_lost(&buf),
            "text {:?} wire {wire:?}",
            buf.text()
        );
        assert_eq!(server_mentions(&wire), fluxer_mentions(&wire));
        checked += 1;
    });
    assert!(checked > 80_000, "{checked}");
}

#[test]
fn to_wire_does_not_depend_on_what_was_filtered_on_the_way_in() {
    // The buffer also fills from server text. Here the typed text holds U+202E and U+000C
    // between a backslash and a tag: the server removes them, and a backslash that was
    // counted as separate would no longer escape. `from_wire` keeps such characters.
    let pieces = [
        "\\",
        "\\\\",
        "\\<@7>",
        "\\<@&8>",
        "\\<#9>",
        "\\@everyone",
        "<@7>",
        "<@&8>",
        "@everyone",
        "\u{202e}",
        "\u{000c}",
        "<",
        "@",
        "x",
    ];
    let mut checked = 0usize;
    for_each_sequence(pieces.len(), 4, |seq| {
        let raw: String = seq.iter().map(|&p| pieces[p]).collect();
        let buf = Buffer::from_wire(&raw, &Names);
        let wire = buf.to_wire();
        assert_eq!(
            fluxer_mentions(&wire),
            picked_without_lost(&buf),
            "raw {raw:?} text {:?} wire {wire:?}",
            buf.text()
        );
        checked += 1;
    });
    assert!(checked > 30_000, "{checked}");
}

#[test]
fn a_backslash_and_a_stripped_character_cannot_ping_a_role() {
    // The attack: backslash, U+202E, then a role tag, typed or pasted as plain text.
    let mut buf = Buffer::new(2000);
    buf.insert(0, "\\\u{202e}<@&42>").unwrap();
    assert_eq!(buf.text(), "\\<@&42>", "the override is dropped");
    assert_eq!(fluxer_mentions(&buf.to_wire()), Mentions::default());
    for stripped in ['\u{202e}', '\u{000c}'] {
        let raw = format!("\\{stripped}\\<@&42>");
        let buf = Buffer::from_wire(&raw, &Names);
        assert!(buf.tokens().is_empty());
        let wire = buf.to_wire();
        assert!(!wire.contains(stripped), "{wire:?}");
        assert_eq!(fluxer_mentions(&wire), Mentions::default(), "{wire:?}");
    }
}

#[test]
fn input_drops_controls_and_normalizes_line_breaks() {
    let mut buf = Buffer::new(2000);
    buf.insert(
        0,
        "a\u{202e}b\u{000c}c\u{200e}\u{200f}\u{061c}\u{2066}\u{2069}\u{0007}d\te\r\nf\rg\u{0085}h",
    )
    .unwrap();
    assert_eq!(buf.text(), "abcd\te\nf\ngh");
}

#[test]
fn allowed_mentions_hold_only_picked_tokens() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "hi <@99> ");
    assert_eq!(buf.allowed_mentions(), AllowedMentions::default());
    let end = buf.len();
    user_token(&mut buf, end, 7, "@u");
    let end = buf.len();
    user_token(&mut buf, end, 7, "@u");
    assert!(buf.insert_token(buf.len(), Token::Role(Id(8)), "@r"));
    assert!(buf.insert_token(buf.len(), Token::Channel(Id(3)), "#c"));
    let allowed = buf.allowed_mentions();
    assert_eq!(allowed.users, vec![Id(7)]);
    assert_eq!(allowed.roles, vec![Id(8)]);
    assert!(!allowed.everyone);
    assert!(buf.insert_token(buf.len(), Token::Here, "@here"));
    assert!(buf.allowed_mentions().everyone);
}

#[test]
fn code_and_links_are_left_alone() {
    let same = [
        "```xml\n<a:t>Hi</a:t>\n```",
        "`<#list items as x>`",
        "`<@macro/>`",
        "`@here`",
        "see https://example.com/@hereford now",
        "``<@7>``",
    ];
    for text in same {
        let mut buf = Buffer::new(2000);
        buf.insert(0, text).unwrap();
        assert_eq!(buf.to_wire(), text, "{text:?}");
        assert_eq!(fluxer_mentions(&buf.to_wire()), Mentions::default());
    }
    // The same text outside code is escaped, even when code sits next to it.
    let mut buf = Buffer::new(2000);
    buf.insert(0, "`<@7>` <@7> `@here` @here").unwrap();
    assert_eq!(buf.to_wire(), "`<@7>` \\<@7> `@here` \\@here");
    assert_eq!(fluxer_mentions(&buf.to_wire()), Mentions::default());
}

#[test]
fn code_around_pills_never_adds_a_mention() {
    #[derive(Clone, Copy)]
    enum Piece {
        Text(&'static str),
        User,
    }
    use Piece::*;
    let pieces = [
        Text("`"),
        Text("\n```\n"),
        Text("https://a.b/"),
        Text(" "),
        Text("<@7>"),
        Text("<@8>"),
        Text("<a:t>"),
        Text("@here"),
        Text("<#list>"),
        Text("x"),
        User,
    ];
    let mut checked = 0usize;
    for_each_sequence(pieces.len(), 4, |seq| {
        let mut buf = Buffer::new(10_000);
        for &p in seq {
            let at = buf.len();
            match pieces[p] {
                Text(s) => buf.insert(at, s).unwrap(),
                User => assert!(buf.insert_token(at, Token::User(Id(7)), "@u")),
            }
        }
        let wire = buf.to_wire();
        let found = fluxer_mentions(&wire);
        let picked = Mentions::picked(buf.tokens());
        // A pill inside code is not a mention, so the found set may be smaller. It is never
        // bigger: nothing the user did not pick can ping.
        assert!(
            found.users.is_subset(&picked.users)
                && found.roles.is_empty()
                && found.channels.is_empty()
                && !found.everyone
                && !found.here,
            "text {:?} wire {wire:?} found {found:?}",
            buf.text()
        );
        if buf.tokens().is_empty() {
            assert_eq!(found, Mentions::default(), "wire {wire:?}");
        }
        checked += 1;
    });
    assert!(checked > 10_000, "{checked}");
}

#[test]
fn custom_emoji_names_with_a_dash_stay_emoji() {
    let buf = Buffer::from_wire("<:a-b:5> <a:caf\u{e9}_1:6>", &NoNames);
    assert_eq!(buf.tokens().len(), 2);
    assert_eq!(buf.to_wire(), "<:a-b:5> <a:caf\u{e9}_1:6>");
}

#[test]
fn pill_labels_from_names_are_cleaned() {
    struct Dirty;
    impl Resolve for Dirty {
        fn user(&self, _: Id) -> Option<String> {
            Some("al\nice\u{202e}\t\u{0007}".into())
        }
        fn role(&self, _: Id) -> Option<String> {
            Some("\n\u{202e}".into())
        }
        fn channel(&self, _: Id) -> Option<String> {
            None
        }
    }
    let buf = Buffer::from_wire("<@1> <@&2>", &Dirty);
    assert_eq!(buf.text(), "@al ice @unknown-role");
    let mut buf = Buffer::new(2000);
    assert!(buf.insert_token(0, Token::User(Id(1)), "a\u{202e}b\nc"));
    assert_eq!(buf.text(), "ab c");
    assert!(!buf.insert_token(0, Token::User(Id(1)), "\u{202e}\n"));
}

// --- clipboard --------------------------------------------------------------------------

#[test]
fn a_copied_pill_survives_paste() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "hi !");
    user_token(&mut buf, 3, 1, "@alice");
    let fragment = buf.fragment(0..buf.len());
    assert_eq!(fragment.text, "hi @alice!");
    assert_eq!(fragment.tokens.len(), 1);
    assert_eq!(fragment.tokens[0].range, 3..9);
    let metadata = fragment.to_metadata().unwrap();
    // Other apps get plain text. Inlet gets the pill back.
    let back = Fragment::from_clipboard(&fragment.text, &metadata).unwrap();
    assert_eq!(back, fragment);
    let mut other = Buffer::new(2000);
    typed(&mut other, ">> ");
    other.insert_fragment(3..3, &back).unwrap();
    assert_eq!(other.text(), ">> hi @alice!");
    assert_eq!(other.to_wire(), ">> hi <@1>!");
    assert!(other.undo());
    assert_eq!(other.text(), ">> ");
    // A part of a pill is never copied.
    assert_eq!(buf.fragment(4..7).text, "@alice");
}

#[test]
fn clipboard_metadata_that_does_not_fit_is_ignored() {
    let ok = "inlet-editor-fragment 1\n3 9 <@1>";
    assert!(Fragment::from_clipboard("hi @alice!", ok).is_some());
    for bad in [
        "inlet-editor-fragment 2\n3 9 <@1>",
        "inlet-editor-fragment 1\n3 99 <@1>",
        "inlet-editor-fragment 1\n9 3 <@1>",
        "inlet-editor-fragment 1\n3 9 <@1> junk",
        "inlet-editor-fragment 1\n3 9 hello",
        "inlet-editor-fragment 1\n3 9 <@1>\n4 8 <@2>",
        "nonsense",
    ] {
        assert!(
            Fragment::from_clipboard("hi @alice!", bad).is_none(),
            "{bad}"
        );
    }
    // Text the editor would change is not trusted.
    assert!(Fragment::from_clipboard("hi \u{202e}@alice!", ok).is_none());
    // Without pills there is no metadata.
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "plain");
    assert!(buf.fragment(0..5).to_metadata().is_none());
}

// --- limits and the input method -----------------------------------------------------------

#[test]
fn a_huge_paste_is_refused_without_work() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "keep");
    let huge = "x".repeat(50_000_000);
    assert!(buf.insert(4, &huge).is_err());
    assert_eq!(buf.text(), "keep");
}

#[test]
fn a_composition_may_pass_the_limit_until_it_is_committed() {
    let mut buf = Buffer::new(3);
    buf.insert(0, "ab").unwrap();
    buf.break_undo_group();
    let r = buf.replace_composing(2..2, "cdef").unwrap();
    assert!(buf.is_over_limit());
    // Taking the composition back restores the text, and it cannot be redone.
    buf.end_composition();
    assert!(buf.discard_last_step());
    assert_eq!(buf.text(), "ab");
    assert!(!buf.redo());
    assert_eq!(r, 2..6);
    assert!(!buf.is_over_limit());
}

// --- movement details -------------------------------------------------------------------

#[test]
fn backspace_removes_one_code_point_in_complex_scripts_and_whole_emoji() {
    let mut buf = Buffer::new(2000);
    // Devanagari "ki": a consonant and a vowel sign form one cluster.
    buf.insert(0, "\u{0915}\u{093f}").unwrap();
    assert_eq!(prev_grapheme(&buf, 6), 0);
    assert_eq!(prev_backspace_boundary(&buf, 6), 3);
    // e with a combining acute accent: the accent goes first.
    let mut buf = Buffer::new(2000);
    buf.insert(0, "xe\u{301}").unwrap();
    assert_eq!(prev_backspace_boundary(&buf, 4), 2);
    // Emoji sequences, flags and keycaps go whole.
    for emoji in [
        "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
        "\u{1f1f3}\u{1f1f4}",
        "1\u{fe0f}\u{20e3}",
        "\u{1f44d}\u{1f3fd}",
    ] {
        let mut buf = Buffer::new(2000);
        buf.insert(0, "a").unwrap();
        buf.insert(1, emoji).unwrap();
        assert_eq!(prev_backspace_boundary(&buf, buf.len()), 1, "{emoji:?}");
    }
    // A plain letter, and a pill.
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "ab");
    assert_eq!(prev_backspace_boundary(&buf, 2), 1);
    user_token(&mut buf, 2, 1, "@alice");
    assert_eq!(prev_backspace_boundary(&buf, 8), 2);
}

#[test]
fn clicks_land_on_grapheme_boundaries() {
    let mut buf = Buffer::new(2000);
    buf.insert(0, "ae\u{301}b").unwrap();
    // Offsets: a 0..1, e 1..2, accent 2..4, b 4..5.
    assert_eq!(nearest_grapheme_boundary(&buf, 1), 1);
    assert_eq!(
        nearest_grapheme_boundary(&buf, 2),
        1,
        "between e and its accent"
    );
    assert_eq!(
        nearest_grapheme_boundary(&buf, 3),
        1,
        "inside the accent, nearer the start"
    );
    assert_eq!(nearest_grapheme_boundary(&buf, 4), 4);
    assert_eq!(nearest_grapheme_boundary(&buf, 99), 5);
    // Inside a pill, the nearer edge.
    let mut buf = Buffer::new(2000);
    user_token(&mut buf, 0, 1, "@alice");
    assert_eq!(nearest_grapheme_boundary(&buf, 2), 0);
    assert_eq!(nearest_grapheme_boundary(&buf, 5), 6);
}

// --- tokens as one unit ---------------------------------------------------------------------

#[test]
fn deleting_inside_a_token_removes_the_whole_token() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "abcd");
    user_token(&mut buf, 2, 1, "@alice");
    assert_eq!(buf.text(), "ab@alicecd");
    // One byte from the middle of the pill.
    buf.delete(4..5);
    assert_eq!(buf.text(), "abcd");
    assert!(buf.tokens().is_empty());
    assert_eq!(buf.to_wire(), "abcd");
}

#[test]
fn a_range_that_only_touches_a_token_leaves_it() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "abcd");
    user_token(&mut buf, 2, 1, "@alice");
    buf.delete(0..2);
    assert_eq!(buf.text(), "@alicecd");
    assert_eq!(buf.tokens().len(), 1);
    assert_eq!(buf.tokens()[0].range, 0..6);
    buf.delete(6..8);
    assert_eq!(buf.text(), "@alice");
    assert_eq!(buf.tokens()[0].range, 0..6);
}

#[test]
fn a_range_that_starts_in_a_token_takes_the_token_and_the_text_after_it() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "abcd");
    user_token(&mut buf, 2, 1, "@alice");
    buf.delete(3..9);
    assert_eq!(buf.text(), "abd");
    assert!(buf.tokens().is_empty());
}

#[test]
fn tokens_move_when_text_is_inserted_before_them() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "ab");
    user_token(&mut buf, 2, 1, "@alice");
    buf.insert(0, "日本").unwrap();
    assert_eq!(buf.text(), "日本ab@alice");
    assert_eq!(buf.tokens()[0].range, 8..14);
    assert_eq!(&buf.text()[buf.tokens()[0].range.clone()], "@alice");
}

#[test]
fn an_insert_inside_a_token_lands_at_its_nearer_edge() {
    let mut buf = Buffer::new(2000);
    user_token(&mut buf, 0, 1, "@alice");
    buf.insert(1, "x").unwrap();
    assert_eq!(buf.text(), "x@alice");
    let mut buf = Buffer::new(2000);
    user_token(&mut buf, 0, 1, "@alice");
    buf.insert(5, "x").unwrap();
    assert_eq!(buf.text(), "@alicex");
    assert_eq!(buf.tokens()[0].range, 0..6);
}

#[test]
fn offsets_are_bytes_and_stay_on_char_boundaries() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "日本語");
    // Byte 1 is inside the first character. The insert goes before it.
    buf.insert(1, "x").unwrap();
    assert_eq!(buf.text(), "x日本語");
    buf.delete(2..5);
    assert_eq!(buf.text(), "x本語");
    user_token(&mut buf, 4, 1, "@ålice");
    assert_eq!(buf.text(), "x本@ålice語");
    assert_eq!(buf.tokens()[0].range, 4..11);
    assert_eq!(buf.len_utf16(), "x本<@1>語".encode_utf16().count());
}

#[test]
fn carriage_returns_become_line_feeds() {
    let mut buf = Buffer::new(2000);
    buf.insert(0, "a\r\nb\rc").unwrap();
    assert_eq!(buf.text(), "a\nb\nc");
}

// --- length limit ---------------------------------------------------------------------------

#[test]
fn max_chars_rejects_paste_over_the_limit() {
    let mut buf = Buffer::new(10);
    buf.insert(0, "abcde").unwrap();
    let err = buf.insert(5, "123456").unwrap_err();
    assert_eq!(err.max_len_utf16, 10);
    assert_eq!(buf.text(), "abcde", "a rejected paste changes nothing");
    buf.insert(5, "12345").unwrap();
    assert_eq!(buf.text(), "abcde12345");
    assert!(buf.insert(10, "x").is_err());
    // Deleting is always fine, and so is undoing the paste that was accepted.
    buf.delete(0..5);
    assert!(buf.undo());
    assert_eq!(buf.text(), "abcde12345");
}

#[test]
fn the_limit_counts_utf16_code_units_as_the_server_does() {
    // The server counts JavaScript string length: a character outside the BMP counts as 2.
    let mut buf = Buffer::new(3);
    buf.insert(0, "\u{1f600}").unwrap();
    assert_eq!(buf.len_utf16(), 2);
    assert!(buf.insert(4, "\u{1f600}").is_err(), "4 units");
    buf.insert(4, "x").unwrap();
    // Characters in the BMP count as 1, whatever their length in bytes.
    let mut buf = Buffer::new(3);
    buf.insert(0, "\u{65e5}\u{672c}\u{8a9e}").unwrap();
    assert!(buf.insert(9, "x").is_err());
    // A flag is two regional indicators, 4 units. A family emoji is 8.
    let mut buf = Buffer::new(7);
    assert!(buf
        .insert(0, "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}")
        .is_err());
    buf.insert(0, "\u{1f1f3}\u{1f1f4}").unwrap();
    assert_eq!(buf.len_utf16(), 4);
}

#[test]
fn max_chars_counts_what_is_sent() {
    // `<@123456789012345678>` is 21 characters on the wire, although the pill shows 6.
    let mut buf = Buffer::new(25);
    assert!(buf.insert_token(0, Token::User(Id(123_456_789_012_345_678)), "@alice"));
    assert_eq!(buf.len_utf16(), 21);
    assert!(buf.insert(6, "12345").is_err());
    buf.insert(6, "1234").unwrap();
    // A token that does not fit is refused too.
    assert!(!buf.insert_token(10, Token::Here, "@here"));
    assert_eq!(buf.text(), "@alice1234");
}

#[test]
fn text_over_the_limit_can_shrink_but_not_grow() {
    let mut buf = Buffer::from_wire("0123456789", &NoNames).with_max_len(5);
    assert!(buf.insert(10, "x").is_err());
    buf.delete(0..2);
    assert_eq!(buf.text(), "23456789");
}

// --- undo -----------------------------------------------------------------------------------

#[test]
fn undo_groups_typing_into_words() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "hello world");
    assert!(buf.undo());
    assert_eq!(buf.text(), "hello ");
    assert!(buf.undo());
    assert_eq!(buf.text(), "");
    assert!(!buf.undo());
    assert!(buf.redo());
    assert_eq!(buf.text(), "hello ");
    assert!(buf.redo());
    assert_eq!(buf.text(), "hello world");
    assert!(!buf.redo());
}

#[test]
fn typing_after_an_undo_clears_redo() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "ab");
    assert!(buf.undo());
    typed(&mut buf, "c");
    assert!(!buf.redo());
    assert_eq!(buf.text(), "c");
}

#[test]
fn moving_the_caret_starts_a_new_undo_step() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "ab");
    buf.break_undo_group();
    typed(&mut buf, "cd");
    assert!(buf.undo());
    assert_eq!(buf.text(), "ab");
}

#[test]
fn a_run_of_backspaces_is_one_undo_step() {
    let mut buf = Buffer::new(2000);
    buf.insert(0, "hello").unwrap();
    for end in (2..=5).rev() {
        buf.delete(end - 1..end);
    }
    assert_eq!(buf.text(), "h");
    assert!(buf.undo());
    assert_eq!(buf.text(), "hello");
}

#[test]
fn undo_brings_back_tokens() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "hi ");
    user_token(&mut buf, 3, 1, "@alice");
    buf.delete(3..9);
    assert!(buf.tokens().is_empty());
    assert!(buf.undo());
    assert_eq!(buf.tokens().len(), 1);
    assert_eq!(buf.to_wire(), "hi <@1>");
    assert_eq!(buf.cursor_hint(), 9);
    assert!(buf.undo());
    assert_eq!(buf.text(), "hi ");
    assert!(buf.redo());
    assert_eq!(buf.to_wire(), "hi <@1>");
}

#[test]
fn a_composition_is_one_undo_step() {
    let mut buf = Buffer::new(2000);
    buf.insert(0, "ab").unwrap();
    buf.break_undo_group();
    let r = buf.replace_composing(2..2, "n").unwrap();
    let r = buf.replace_composing(r, "に").unwrap();
    buf.replace_composing(r, "日本").unwrap();
    buf.end_composition();
    assert_eq!(buf.text(), "ab日本");
    assert!(buf.undo());
    assert_eq!(buf.text(), "ab");
}

// --- selection and movement -----------------------------------------------------------------

#[test]
fn grapheme_movement_skips_emoji_and_combining_marks() {
    let mut buf = Buffer::new(2000);
    // a, a family emoji (three people joined by zero-width joiners), e + combining acute,
    // a flag (two regional indicators), b.
    let text = "a👨\u{200d}👩\u{200d}👧e\u{301}🇳🇴b";
    buf.insert(0, text).unwrap();
    let stops = [0, 1, 19, 22, 30, 31];
    assert_eq!(buf.len(), 31);
    for pair in stops.windows(2) {
        assert_eq!(
            next_grapheme(&buf, pair[0]),
            pair[1],
            "next from {}",
            pair[0]
        );
        assert_eq!(
            prev_grapheme(&buf, pair[1]),
            pair[0],
            "prev from {}",
            pair[1]
        );
    }
    assert_eq!(next_grapheme(&buf, 31), 31);
    assert_eq!(prev_grapheme(&buf, 0), 0);

    let caret = Selection::caret(1);
    let right = caret.moved(&buf, Move::Right, false);
    assert_eq!(right, Selection::caret(19));
    let sel = caret.moved(&buf, Move::Right, true);
    assert_eq!((sel.anchor, sel.head), (1, 19));
    // Left on a selection collapses it to its start instead of moving further.
    assert_eq!(sel.moved(&buf, Move::Left, false), Selection::caret(1));
    assert_eq!(sel.moved(&buf, Move::Right, false), Selection::caret(19));
}

#[test]
fn a_token_is_one_step_for_the_caret() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "hi !");
    user_token(&mut buf, 3, 1, "@alice");
    assert_eq!(buf.text(), "hi @alice!");
    assert_eq!(next_grapheme(&buf, 3), 9);
    assert_eq!(prev_grapheme(&buf, 9), 3);
    // Even from a bad offset inside the token.
    assert_eq!(next_grapheme(&buf, 5), 9);
    assert_eq!(prev_grapheme(&buf, 5), 3);
    // A selection that ends inside a token grows to cover it.
    let sel = Selection { anchor: 0, head: 5 }.normalized(&buf);
    assert_eq!((sel.anchor, sel.head), (0, 9));
}

#[test]
fn word_and_line_movement() {
    let mut buf = Buffer::new(2000);
    buf.insert(0, "foo bar, baz\nsecond line").unwrap();
    assert_eq!(next_word(&buf, 0), 3);
    assert_eq!(next_word(&buf, 3), 7);
    assert_eq!(prev_word(&buf, 7), 4);
    assert_eq!(prev_word(&buf, 4), 0);
    assert_eq!(next_word(&buf, 12), 19, "skips the line break");
    assert_eq!(line_start(&buf, 5), 0);
    assert_eq!(line_end(&buf, 5), 12);
    assert_eq!(line_start(&buf, 15), 13);
    assert_eq!(line_end(&buf, 15), buf.len());
}

// --- triggers -------------------------------------------------------------------------------

fn trigger(text: &str) -> Option<(std::ops::Range<usize>, Trigger)> {
    let mut buf = Buffer::new(2000);
    buf.insert(0, text).unwrap();
    trigger_at(&buf, buf.len())
}

#[test]
fn trigger_is_found_only_at_a_word_start() {
    assert_eq!(
        trigger("hello @al"),
        Some((6..9, Trigger::Mention("al".into())))
    );
    assert_eq!(trigger("@"), Some((0..1, Trigger::Mention(String::new()))));
    assert_eq!(
        trigger("see #gen"),
        Some((4..8, Trigger::Channel("gen".into())))
    );
    assert_eq!(
        trigger("a :smi"),
        Some((2..6, Trigger::Emoji("smi".into())))
    );
    assert_eq!(trigger("/he"), Some((0..3, Trigger::Command("he".into()))));
    assert_eq!(
        trigger("line\n@bo"),
        Some((5..8, Trigger::Mention("bo".into())))
    );

    // In the middle of a word, there is no trigger.
    assert_eq!(trigger("a@b"), None);
    assert_eq!(trigger("mail me at x@exam"), None);
    assert_eq!(trigger("see http://exa"), None);
    assert_eq!(trigger("c#sharp"), None);
    // `/` only starts a command at the start of the message.
    assert_eq!(trigger("x /he"), None);
    // `:` needs two letters, and only emoji name characters.
    assert_eq!(trigger("ok :)"), None);
    assert_eq!(trigger("ok :s"), None);
    assert_eq!(trigger("ok :a b"), None);
    // Nothing without a trigger character.
    assert_eq!(trigger("hello"), None);
    assert_eq!(trigger(""), None);
}

#[test]
fn trigger_follows_the_caret_inside_a_word() {
    let mut buf = Buffer::new(2000);
    buf.insert(0, "@alice").unwrap();
    assert_eq!(
        trigger_at(&buf, 3),
        Some((0..3, Trigger::Mention("al".into())))
    );
    assert_eq!(trigger_at(&buf, 99), None);
}

#[test]
fn trigger_ranges_are_byte_offsets() {
    // Each of these is more than one byte.
    let t = trigger("ö @ñá");
    assert_eq!(t, Some((3..8, Trigger::Mention("ñá".into()))));
}

#[test]
fn trigger_ignores_pills() {
    let mut buf = Buffer::new(2000);
    user_token(&mut buf, 0, 1, "@alice");
    // Right after a pill, and inside it, there is no trigger.
    assert_eq!(trigger_at(&buf, 6), None);
    assert_eq!(trigger_at(&buf, 3), None);
    // Text glued to a pill is not a new word. After a space it is.
    buf.insert(6, "@bo").unwrap();
    assert_eq!(trigger_at(&buf, buf.len()), None);
    let mut buf = Buffer::new(2000);
    user_token(&mut buf, 0, 1, "@alice");
    buf.insert(6, " @bo").unwrap();
    assert_eq!(
        trigger_at(&buf, buf.len()),
        Some((7..10, Trigger::Mention("bo".into())))
    );
}

#[test]
fn a_trigger_can_be_replaced_with_a_token_in_one_step() {
    let mut buf = Buffer::new(2000);
    typed(&mut buf, "hi @al");
    let (range, _) = trigger_at(&buf, buf.len()).unwrap();
    assert!(buf.replace_with_token(range, Token::User(Id(1)), "@alice"));
    assert_eq!(buf.text(), "hi @alice");
    assert_eq!(buf.to_wire(), "hi <@1>");
    assert!(buf.undo());
    assert_eq!(buf.text(), "hi @al");
}
