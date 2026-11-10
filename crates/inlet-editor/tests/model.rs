// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of the text model: buffer, tokens, selection, undo and triggers. No window needed.

use inlet_editor::{
    line_end, line_start, next_grapheme, next_word, prev_grapheme, prev_word, trigger_at, Buffer,
    Id, Move, NoNames, Resolve, Selection, Token, Trigger,
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

/// What a server that follows Fluxer's markdown rules would read as mentions in `wire`.
fn server_mentions(wire: &str) -> Vec<Token> {
    let b = wire.as_bytes();
    let escapable = b"[]()\\*_~`@#-|:<>";
    let mut out = Vec::new();
    let mut i = 0;
    while i < wire.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < wire.len() && escapable.contains(&b[i + 1]) {
            i += 2;
            continue;
        }
        let rest = &wire[i..];
        if c == b'<' {
            if let Some((t, len)) = Token::parse_wire(rest) {
                out.push(t);
                i += len;
                continue;
            }
        }
        if c == b'@' && !(i > 0 && b[i - 1] == b'\\') {
            if let Some((t, len)) = Token::parse_wire(rest) {
                out.push(t);
                i += len;
                continue;
            }
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    out
}

#[test]
fn the_server_only_sees_the_mentions_the_user_picked() {
    // Every way up to five of these pieces can be put together, as typed text or as tokens.
    #[derive(Clone, Copy)]
    enum Piece {
        Text(&'static str),
        User,
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
        Text("x"),
        User,
        Everyone,
    ];
    let mut checked = 0usize;
    let mut stack: Vec<Vec<usize>> = vec![vec![]];
    while let Some(seq) = stack.pop() {
        if !seq.is_empty() {
            let mut buf = Buffer::new(10_000);
            for &p in &seq {
                let at = buf.len();
                match pieces[p] {
                    Text(s) => buf.insert(at, s).unwrap(),
                    User => assert!(buf.insert_token(at, Token::User(Id(7)), "@u")),
                    Everyone => assert!(buf.insert_token(at, Token::Everyone, "@everyone")),
                }
            }
            let wire = buf.to_wire();
            // A mention token right after a backslash cannot be sent (the backslash would
            // escape it). That is the safe direction, so it is allowed to go missing.
            let lost = |t: &inlet_editor::TokenSpan| {
                matches!(t.token, Token::Everyone | Token::Here)
                    && t.range.start > 0
                    && buf.text().as_bytes()[t.range.start - 1] == b'\\'
            };
            let expected: Vec<Token> = buf
                .tokens()
                .iter()
                .filter(|t| !lost(t))
                .map(|t| t.token.clone())
                .collect();
            assert_eq!(server_mentions(&wire), expected, "text {:?}", buf.text());
            if !buf.tokens().iter().any(lost) {
                let back = Buffer::from_wire(&wire, &Names);
                assert_eq!(back.text(), buf.text(), "wire {wire:?}");
                assert_eq!(back.tokens(), buf.tokens(), "wire {wire:?}");
            }
            checked += 1;
        }
        if seq.len() < 5 {
            for p in 0..pieces.len() {
                let mut next = seq.clone();
                next.push(p);
                stack.push(next);
            }
        }
    }
    assert!(checked > 100_000);
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
    assert_eq!(buf.len_chars(), "x本<@1>語".chars().count());
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
    assert_eq!(err.max_chars, 10);
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
fn max_chars_counts_characters_not_bytes() {
    let mut buf = Buffer::new(3);
    buf.insert(0, "日本語").unwrap();
    assert!(buf.insert(9, "x").is_err());
}

#[test]
fn max_chars_counts_what_is_sent() {
    // `<@123456789012345678>` is 21 characters on the wire, although the pill shows 6.
    let mut buf = Buffer::new(25);
    assert!(buf.insert_token(0, Token::User(Id(123_456_789_012_345_678)), "@alice"));
    assert_eq!(buf.len_chars(), 21);
    assert!(buf.insert(6, "12345").is_err());
    buf.insert(6, "1234").unwrap();
    // A token that does not fit is refused too.
    assert!(!buf.insert_token(10, Token::Here, "@here"));
    assert_eq!(buf.text(), "@alice1234");
}

#[test]
fn text_over_the_limit_can_shrink_but_not_grow() {
    let mut buf = Buffer::from_wire("0123456789", &NoNames).with_max_chars(5);
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
