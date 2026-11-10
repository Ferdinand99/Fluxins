// SPDX-License-Identifier: AGPL-3.0-or-later
//! The text of the message being written: plain text plus token spans.
//!
//! All offsets are byte offsets into the display text ([`Buffer::text`]) and always sit on a
//! `char` boundary. Offsets inside a token are not valid cursor positions: the buffer snaps
//! them to the token's edge.

use std::fmt;
use std::ops::Range;

use crate::history::{History, Kind, Snapshot};
use crate::tokens::{label_for, special_at, Resolve, Special, Token, TokenSpan};

/// The text would pass `max_chars`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooLong {
    pub max_chars: usize,
}

impl fmt::Display for TooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the message cannot be longer than {} characters",
            self.max_chars
        )
    }
}

impl std::error::Error for TooLong {}

#[derive(Debug)]
pub struct Buffer {
    text: String,
    tokens: Vec<TokenSpan>,
    max_chars: usize,
    history: History,
    cursor: usize,
}

impl Buffer {
    /// An empty buffer. `max_chars` limits the length of the wire text (what Fluxer receives).
    pub fn new(max_chars: usize) -> Self {
        Self {
            text: String::new(),
            tokens: Vec::new(),
            max_chars,
            history: History::default(),
            cursor: 0,
        }
    }

    /// Changes the limit. Text already in the buffer is kept even if it is over the new limit,
    /// but no edit can make it longer.
    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars;
        self
    }

    pub fn max_chars(&self) -> usize {
        self.max_chars
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

    /// Inserts text. Fails, and changes nothing, when the result would pass `max_chars`.
    pub fn insert(&mut self, at: usize, s: &str) -> Result<(), TooLong> {
        let at = self.snap_nearest(at);
        self.replace(at..at, s).map(|_| ())
    }

    /// Replaces a range with text, as one undo step. Returns where the new text is.
    pub fn replace(&mut self, range: Range<usize>, s: &str) -> Result<Range<usize>, TooLong> {
        let range = self.expand_range(range);
        let s = normalize_newlines(s);
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
        self.apply(range, &s, None, kind, cursor_before)
    }

    /// A step of an input method composition. Steps in a row are one undo step.
    pub fn replace_composing(
        &mut self,
        range: Range<usize>,
        s: &str,
    ) -> Result<Range<usize>, TooLong> {
        let range = self.expand_range(range);
        let s = normalize_newlines(s);
        let cursor_before = range.start;
        self.apply(range, &s, None, Kind::Composition, cursor_before)
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
        let _ = self.apply(range, "", None, kind, cursor_before);
    }

    /// Inserts a token that shows as `label`. Returns false, and changes nothing, when the
    /// label is empty or the result would pass `max_chars`.
    pub fn insert_token(&mut self, at: usize, t: Token, label: &str) -> bool {
        let at = self.snap_nearest(at);
        self.replace_with_token(at..at, t, label)
    }

    /// Replaces a range (for example the text of a trigger) with a token, as one undo step.
    pub fn replace_with_token(&mut self, range: Range<usize>, t: Token, label: &str) -> bool {
        let label: String = label
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if label.is_empty() {
            return false;
        }
        let range = self.expand_range(range);
        let cursor_before = range.start;
        self.apply(range, &label, Some(t), Kind::Other, cursor_before)
            .is_ok()
    }

    /// The next edit starts a new undo step. Call it when the caret moves.
    pub fn break_undo_group(&mut self) {
        self.history.close_group();
    }

    /// An input method composition ended.
    pub fn end_composition(&mut self) {
        self.history.close_group();
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

    /// The one place the text changes. `range` must already be valid.
    fn apply(
        &mut self,
        range: Range<usize>,
        new: &str,
        token: Option<Token>,
        kind: Kind,
        cursor_before: usize,
    ) -> Result<Range<usize>, TooLong> {
        if range.is_empty() && new.is_empty() {
            return Ok(range);
        }
        let before = self.snapshot();
        let chars_before = self.len_chars();
        self.splice(&range, new, token);
        let chars_after = self.len_chars();
        if chars_after > self.max_chars && chars_after > chars_before {
            self.restore(before);
            return Err(TooLong {
                max_chars: self.max_chars,
            });
        }
        self.history.begin(kind, || before, cursor_before);
        let inserted = range.start..range.start + new.len();
        self.cursor = inserted.end;
        Ok(inserted)
    }

    fn splice(&mut self, range: &Range<usize>, new: &str, token: Option<Token>) {
        self.text.replace_range(range.clone(), new);
        let old_len = range.end - range.start;
        let mut kept = Vec::with_capacity(self.tokens.len() + 1);
        for mut t in std::mem::take(&mut self.tokens) {
            if t.range.end <= range.start {
                kept.push(t);
            } else if t.range.start >= range.end {
                t.range = t.range.start - old_len + new.len()..t.range.end - old_len + new.len();
                kept.push(t);
            }
            // Otherwise the token lay inside the replaced range, and is gone.
        }
        if let Some(token) = token {
            let span = TokenSpan {
                range: range.start..range.start + new.len(),
                token,
            };
            let at = kept.partition_point(|t| t.range.start < span.range.start);
            kept.insert(at, span);
        }
        self.tokens = kept;
    }

    // --- wire text -----------------------------------------------------------------------

    /// How many characters Fluxer would count in the message.
    pub fn len_chars(&self) -> usize {
        self.to_wire().chars().count()
    }

    /// The text to send: `<@id>`, `<@&id>`, `<#id>`, `<:name:id>`, `<a:name:id>`, `@everyone`
    /// and `@here` for tokens.
    ///
    /// Plain text that looks like one of those is escaped with a backslash, so the server
    /// never sees a mention the user did not pick. Backslashes right before such text are
    /// doubled, so they cannot cancel the escape.
    pub fn to_wire(&self) -> String {
        let mut out = String::with_capacity(self.text.len() + 16);
        let mut pos = 0;
        for span in &self.tokens {
            let plain = &self.text[pos..span.range.start];
            push_plain(&mut out, plain);
            if span.token.wire_starts_with_angle() {
                push_backslashes(&mut out, trailing_backslashes(plain));
            }
            out.push_str(&span.token.to_wire());
            pos = span.range.end;
        }
        push_plain(&mut out, &self.text[pos..]);
        out
    }

    /// Reads wire text from the server (to edit a message). Mentions and custom emoji become
    /// tokens. `names` gives the labels. The reverse of [`Buffer::to_wire`].
    /// The limit is off; set it with [`Buffer::with_max_chars`].
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

fn trailing_backslashes(s: &str) -> usize {
    s.bytes().rev().take_while(|b| *b == b'\\').count()
}

fn push_plain(out: &mut String, plain: &str) {
    for (i, ch) in plain.char_indices() {
        if ch == '<' || ch == '@' {
            match special_at(&plain[i..]) {
                Some(Special::Angle) => {
                    // The backslashes before it are already in `out`. Doubling them makes
                    // an even run, then one more escapes the `<`.
                    push_backslashes(out, trailing_backslashes(&plain[..i]));
                    out.push('\\');
                }
                Some(Special::At) => out.push('\\'),
                None => {}
            }
        }
        out.push(ch);
    }
}

/// Fluxer messages use `\n` only.
fn normalize_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_owned();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}
