// SPDX-License-Identifier: AGPL-3.0-or-later
//! The text of the message being written: plain text plus token spans.
//!
//! All offsets are byte offsets into the display text ([`Buffer::text`]) and always sit on a
//! `char` boundary. Offsets inside a token are not valid cursor positions: the buffer snaps
//! them to the token's edge.

use std::fmt;
use std::ops::Range;

use crate::fragment::Fragment;
use crate::history::{History, Kind, Snapshot};
use crate::sanitize::{clean_label, sanitize_input, sanitized_units_lower_bound};
use crate::tokens::{label_for, special_at, Resolve, Special, Token, TokenSpan};
use crate::wire::{self, AllowedMentions};

/// The text would pass the limit.
///
/// The limit is in UTF-16 code units of the wire text, as the server counts it. The `Display`
/// text is English and only for logs. The app shows [`TooLong::MESSAGE_KEY`] through its own
/// string table, with `max_len_utf16` as the argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooLong {
    pub max_len_utf16: usize,
}

impl TooLong {
    /// The key of the string for the app's string table (Fluent). It takes the argument `max`.
    pub const MESSAGE_KEY: &'static str = "composer-too-long";
}

impl fmt::Display for TooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the message cannot be longer than {} characters",
            self.max_len_utf16
        )
    }
}

impl std::error::Error for TooLong {}

#[derive(Debug)]
pub struct Buffer {
    text: String,
    tokens: Vec<TokenSpan>,
    max_len: usize,
    history: History,
    cursor: usize,
}

impl Buffer {
    /// An empty buffer. `max_len_utf16` limits the length of the wire text (what Fluxer
    /// receives) in UTF-16 code units, which is how the server counts. Take it from the
    /// instance's message length limit (2000, or 4000 with the larger limit).
    pub fn new(max_len_utf16: usize) -> Self {
        Self {
            text: String::new(),
            tokens: Vec::new(),
            max_len: max_len_utf16,
            history: History::default(),
            cursor: 0,
        }
    }

    /// Changes the limit. Text already in the buffer is kept even if it is over the new limit,
    /// but no edit can make it longer.
    pub fn with_max_len(mut self, max_len_utf16: usize) -> Self {
        self.max_len = max_len_utf16;
        self
    }

    /// The limit, in UTF-16 code units of the wire text.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// The text as the user sees it. A token shows as its label.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Length of [`Buffer::text`] in bytes.
    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn tokens(&self) -> &[TokenSpan] {
        &self.tokens
    }

    /// The token that `offset` lies strictly inside of. Offsets at a token's edge are outside.
    pub fn token_inside(&self, offset: usize) -> Option<&TokenSpan> {
        self.tokens
            .iter()
            .find(|t| t.range.start < offset && offset < t.range.end)
    }

    /// Where the caret should go after the last edit, undo or redo.
    pub fn cursor_hint(&self) -> usize {
        self.cursor
    }

    /// Moves `offset` back into the text and onto a `char` boundary.
    pub fn clamp(&self, offset: usize) -> usize {
        let mut o = offset.min(self.text.len());
        while !self.text.is_char_boundary(o) {
            o -= 1;
        }
        o
    }

    /// Like [`Buffer::clamp`], and an offset inside a token moves to the nearer edge.
    pub fn snap_nearest(&self, offset: usize) -> usize {
        let o = self.clamp(offset);
        match self.token_inside(o) {
            Some(t) if o - t.range.start <= t.range.end - o => t.range.start,
            Some(t) => t.range.end,
            None => o,
        }
    }

    /// Makes a range valid: ordered, on `char` boundaries, and covering whole tokens only.
    pub fn expand_range(&self, range: Range<usize>) -> Range<usize> {
        let mut start = self.clamp(range.start);
        let mut end = self.clamp(range.end).max(start);
        if let Some(t) = self.token_inside(start) {
            start = t.range.start;
        }
        if let Some(t) = self.token_inside(end) {
            end = t.range.end;
        }
        start..end
    }

    // --- editing -------------------------------------------------------------------------

    /// Inserts text. Fails, and changes nothing, when the result would pass the limit.
    /// Characters that must not be sent are dropped, see [`Buffer::replace`].
    pub fn insert(&mut self, at: usize, s: &str) -> Result<(), TooLong> {
        let at = self.snap_nearest(at);
        self.replace(at..at, s).map(|_| ())
    }

    /// Replaces a range with text, as one undo step. Returns where the new text is.
    ///
    /// Line breaks become `\n`. Bidi controls, U+202E, U+000C and control characters (except
    /// the line feed and the tab) are dropped: the server removes some of them, and the rest
    /// can make the composer show text in another order than it is sent.
    pub fn replace(&mut self, range: Range<usize>, s: &str) -> Result<Range<usize>, TooLong> {
        self.refuse_huge(s)?;
        let range = self.expand_range(range);
        let s = sanitize_input(s);
        let mut chars = s.chars();
        let kind = match (range.is_empty(), chars.next(), chars.next()) {
            (true, Some(c), None) => Kind::Typing {
                at: range.start,
                end: range.start + s.len(),
                space: c.is_whitespace(),
            },
            _ => Kind::Other,
        };
        let cursor_before = if range.is_empty() {
            range.start
        } else {
            range.end
        };
        self.apply(range, &s, Vec::new(), kind, cursor_before, true)
    }

    /// Replaces a range with a fragment that was copied from a buffer, tokens included, as one
    /// undo step. Text the buffer does not accept is inserted as plain text.
    pub fn insert_fragment(
        &mut self,
        range: Range<usize>,
        fragment: &Fragment,
    ) -> Result<Range<usize>, TooLong> {
        if sanitize_input(&fragment.text) != fragment.text {
            return self.replace(range, &fragment.text);
        }
        self.refuse_huge(&fragment.text)?;
        let range = self.expand_range(range);
        let cursor_before = range.start;
        self.apply(
            range,
            &fragment.text,
            fragment.tokens.clone(),
            Kind::Other,
            cursor_before,
            true,
        )
    }

    /// A step of an input method composition. Steps in a row are one undo step.
    ///
    /// The limit is not enforced while composing, because the input method has its own idea
    /// of what is on screen. The editor checks it when the composition is committed.
    pub fn replace_composing(
        &mut self,
        range: Range<usize>,
        s: &str,
    ) -> Result<Range<usize>, TooLong> {
        let range = self.expand_range(range);
        let s = sanitize_input(s);
        let cursor_before = range.start;
        self.apply(
            range,
            &s,
            Vec::new(),
            Kind::Composition,
            cursor_before,
            false,
        )
    }

    /// Deletes a range. A token the range touches is deleted whole.
    pub fn delete(&mut self, range: Range<usize>) {
        let range = self.expand_range(range);
        if range.is_empty() {
            return;
        }
        let kind = Kind::Delete {
            start: range.start,
            end: range.end,
        };
        let cursor_before = range.end;
        // A deletion never makes the text longer, so it cannot fail.
        let _ = self.apply(range, "", Vec::new(), kind, cursor_before, true);
    }

    /// Inserts a token that shows as `label`. Returns false, and changes nothing, when the
    /// label is empty or the result would pass the limit.
    pub fn insert_token(&mut self, at: usize, t: Token, label: &str) -> bool {
        let at = self.snap_nearest(at);
        self.replace_with_token(at..at, t, label)
    }

    /// Replaces a range (for example the text of a trigger) with a token, as one undo step.
    pub fn replace_with_token(&mut self, range: Range<usize>, t: Token, label: &str) -> bool {
        let label = clean_label(label);
        if label.is_empty() {
            return false;
        }
        let range = self.expand_range(range);
        let cursor_before = range.start;
        let span = TokenSpan {
            range: 0..label.len(),
            token: t,
        };
        self.apply(range, &label, vec![span], Kind::Other, cursor_before, true)
            .is_ok()
    }

    /// The text of a range, with its tokens, for the clipboard.
    pub fn fragment(&self, range: Range<usize>) -> Fragment {
        let range = self.expand_range(range);
        let tokens = self
            .tokens
            .iter()
            .filter(|t| t.range.start >= range.start && t.range.end <= range.end)
            .map(|t| TokenSpan {
                range: t.range.start - range.start..t.range.end - range.start,
                token: t.token.clone(),
            })
            .collect();
        Fragment {
            text: self.text[range].to_owned(),
            tokens,
        }
    }

    /// The next edit starts a new undo step. Call it when the caret moves.
    pub fn break_undo_group(&mut self) {
        self.history.close_group();
    }

    /// An input method composition ended.
    pub fn end_composition(&mut self) {
        self.history.close_group();
    }

    /// Goes back one undo step and forgets it: it cannot be redone. For a composition that
    /// turned out to be too long.
    pub fn discard_last_step(&mut self) -> bool {
        match self.history.pop_undo() {
            Some(entry) => {
                self.restore(entry.snapshot);
                self.cursor = self.clamp(entry.cursor);
                true
            }
            None => false,
        }
    }

    pub fn undo(&mut self) -> bool {
        let current = self.snapshot();
        match self.history.undo(current, self.cursor) {
            Some(entry) => {
                self.restore(entry.snapshot);
                self.cursor = self.clamp(entry.cursor);
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        let current = self.snapshot();
        match self.history.redo(current, self.cursor) {
            Some(entry) => {
                self.restore(entry.snapshot);
                self.cursor = self.clamp(entry.cursor);
                true
            }
            None => false,
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            tokens: self.tokens.clone(),
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.text = snapshot.text;
        self.tokens = snapshot.tokens;
    }

    /// Refuses text that is too long before any work is done on it. A paste of 100 MB must
    /// not be copied, spliced and escaped just to be refused.
    fn refuse_huge(&self, s: &str) -> Result<(), TooLong> {
        let lower_bound = sanitized_units_lower_bound(s);
        if lower_bound > self.max_len && lower_bound > self.len_utf16() {
            return Err(TooLong {
                max_len_utf16: self.max_len,
            });
        }
        Ok(())
    }

    /// The one place the text changes. `range` must already be valid, `new` already clean,
    /// and the ranges of `new_tokens` are relative to `new`.
    fn apply(
        &mut self,
        range: Range<usize>,
        new: &str,
        new_tokens: Vec<TokenSpan>,
        kind: Kind,
        cursor_before: usize,
        enforce_limit: bool,
    ) -> Result<Range<usize>, TooLong> {
        if range.is_empty() && new.is_empty() {
            return Ok(range);
        }
        let before = self.snapshot();
        self.splice(&range, new, new_tokens);
        if enforce_limit {
            let after = self.len_utf16();
            if after > self.max_len {
                // Only a longer text is refused: text that is over the limit may shrink.
                let was = wire::to_wire(&before.text, &before.tokens)
                    .encode_utf16()
                    .count();
                if after > was {
                    self.restore(before);
                    return Err(TooLong {
                        max_len_utf16: self.max_len,
                    });
                }
            }
        }
        self.history.begin(kind, || before, cursor_before);
        let inserted = range.start..range.start + new.len();
        self.cursor = inserted.end;
        Ok(inserted)
    }

    fn splice(&mut self, range: &Range<usize>, new: &str, new_tokens: Vec<TokenSpan>) {
        self.text.replace_range(range.clone(), new);
        let old_len = range.end - range.start;
        let mut kept = Vec::with_capacity(self.tokens.len() + new_tokens.len());
        let mut after = Vec::new();
        for mut t in std::mem::take(&mut self.tokens) {
            if t.range.end <= range.start {
                kept.push(t);
            } else if t.range.start >= range.end {
                t.range = t.range.start - old_len + new.len()..t.range.end - old_len + new.len();
                after.push(t);
            }
            // Otherwise the token lay inside the replaced range, and is gone.
        }
        kept.extend(new_tokens.into_iter().map(|mut t| {
            t.range = range.start + t.range.start..range.start + t.range.end;
            t
        }));
        kept.extend(after);
        self.tokens = kept;
    }

    // --- wire text -----------------------------------------------------------------------

    /// How long the message is, as the server counts: UTF-16 code units of the wire text.
    pub fn len_utf16(&self) -> usize {
        self.to_wire().encode_utf16().count()
    }

    /// The text is longer than the limit. Only possible for text that came from the server,
    /// or from an input method composition that has not been committed.
    pub fn is_over_limit(&self) -> bool {
        self.len_utf16() > self.max_len
    }

    /// The text to send: `<@id>`, `<@&id>`, `<#id>`, `<:name:id>`, `<a:name:id>`, `@everyone`
    /// and `@here` for tokens.
    ///
    /// Typed text that looks like one of those is escaped with a backslash, so the server
    /// never sees a mention the user did not pick. Backslashes right before such text are
    /// doubled, so they cannot cancel the escape. The escapes are computed on the text the
    /// server reads, after it has removed U+202E and U+000C. Text in code and in links is
    /// left alone, where Fluxer reads no mentions. The result is checked against a port of
    /// the server's mention extraction. If it finds anything but the picked mentions, every
    /// look-alike is escaped.
    ///
    /// The server does not count or send the leading and trailing white space; this text
    /// keeps it.
    pub fn to_wire(&self) -> String {
        wire::to_wire(&self.text, &self.tokens)
    }

    /// The mentions the user picked, to send as `allowed_mentions` so that the server drops
    /// every other mention. See [`AllowedMentions`] for how to send it.
    pub fn allowed_mentions(&self) -> AllowedMentions {
        AllowedMentions::from_tokens(&self.tokens)
    }

    /// Reads wire text from the server (to edit a message). Mentions and custom emoji become
    /// tokens. `names` gives the labels. The reverse of [`Buffer::to_wire`].
    /// The limit is off; set it with [`Buffer::with_max_len`].
    pub fn from_wire(s: &str, names: &dyn Resolve) -> Self {
        let mut buf = Buffer::new(usize::MAX);
        let bytes = s.as_bytes();
        let mut i = 0;
        let push_token = |buf: &mut Buffer, token: Token| {
            let label = label_for(&token, names);
            let start = buf.text.len();
            buf.text.push_str(&label);
            buf.tokens.push(TokenSpan {
                range: start..buf.text.len(),
                token,
            });
        };
        while i < s.len() {
            let rest = &s[i..];
            match bytes[i] {
                b'\\' => {
                    let m = rest.bytes().take_while(|b| *b == b'\\').count();
                    let j = i + m;
                    match special_at(&s[j..]) {
                        Some(Special::Angle) if m % 2 == 1 => {
                            push_backslashes(&mut buf.text, (m - 1) / 2);
                            buf.text.push('<');
                            i = j + 1;
                        }
                        Some(Special::Angle) => {
                            push_backslashes(&mut buf.text, m / 2);
                            match Token::parse_wire(&s[j..]) {
                                Some((token, len)) => {
                                    push_token(&mut buf, token);
                                    i = j + len;
                                }
                                None => {
                                    buf.text.push('<');
                                    i = j + 1;
                                }
                            }
                        }
                        Some(Special::At) => {
                            push_backslashes(&mut buf.text, m - 1);
                            buf.text.push('@');
                            i = j + 1;
                        }
                        None => {
                            push_backslashes(&mut buf.text, m);
                            i = j;
                        }
                    }
                }
                b'<' | b'@' => match Token::parse_wire(rest) {
                    Some((token, len)) => {
                        push_token(&mut buf, token);
                        i += len;
                    }
                    None => {
                        buf.text.push(bytes[i] as char);
                        i += 1;
                    }
                },
                _ => {
                    let ch = rest.chars().next().unwrap_or('\u{fffd}');
                    buf.text.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        buf.cursor = buf.text.len();
        buf
    }
}

fn push_backslashes(out: &mut String, n: usize) {
    out.extend(std::iter::repeat_n('\\', n));
}
