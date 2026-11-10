// SPDX-License-Identifier: AGPL-3.0-or-later
//! The selection and caret movement. A caret is a selection where `anchor == head`.
//!
//! Movement is per grapheme cluster (an emoji with a skin tone or a letter with combining
//! marks is one step), per word, or per line. A token is always one step.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::Buffer;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// Where the selection started. It stays put while the head moves.
    pub anchor: usize,
    /// The end that moves, and where the caret is drawn.
    pub head: usize,
}

/// A caret movement the text model can do on its own. Up and down need the layout, so the
/// element handles them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    WordLeft,
    WordRight,
    /// The start of the line (the text between two newlines).
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
}

impl Selection {
    pub fn caret(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn range(&self) -> Range<usize> {
        self.start()..self.end()
    }

    pub fn reversed(&self) -> bool {
        self.head < self.anchor
    }

    /// Makes both ends valid for `buf`: on `char` boundaries, and a selection that touches
    /// a token covers all of it. A caret inside a token moves to the nearer edge.
    pub fn normalized(&self, buf: &Buffer) -> Selection {
        if self.is_empty() {
            return Selection::caret(buf.snap_nearest(self.head));
        }
        let r = buf.expand_range(self.range());
        if self.reversed() {
            Selection {
                anchor: r.end,
                head: r.start,
            }
        } else {
            Selection {
                anchor: r.start,
                head: r.end,
            }
        }
    }

    /// Applies a movement. With `extend` the anchor stays; without it the selection becomes
    /// a caret. Left and right on a non-empty selection collapse it to its edge.
    pub fn moved(&self, buf: &Buffer, movement: Move, extend: bool) -> Selection {
        let head = self.head;
        let target = match movement {
            Move::Left if !extend && !self.is_empty() => self.start(),
            Move::Right if !extend && !self.is_empty() => self.end(),
            Move::Left => prev_grapheme(buf, head),
            Move::Right => next_grapheme(buf, head),
            Move::WordLeft => prev_word(buf, head),
            Move::WordRight => next_word(buf, head),
            Move::LineStart => line_start(buf, head),
            Move::LineEnd => line_end(buf, head),
            Move::DocStart => 0,
            Move::DocEnd => buf.len(),
        };
        self.with_head(buf, target, extend)
    }

    /// Moves the head to `target`.
    pub fn with_head(&self, buf: &Buffer, target: usize, extend: bool) -> Selection {
        let target = buf.snap_nearest(target);
        if extend {
            Selection {
                anchor: self.anchor,
                head: target,
            }
            .normalized(buf)
        } else {
            Selection::caret(target)
        }
    }

    pub fn select_all(buf: &Buffer) -> Selection {
        Selection {
            anchor: 0,
            head: buf.len(),
        }
    }
}

/// The start of the grapheme cluster (or token) before `offset`.
pub fn prev_grapheme(buf: &Buffer, offset: usize) -> usize {
    let offset = buf.clamp(offset);
    let mut target = buf
        .text()
        .grapheme_indices(true)
        .rev()
        .find_map(|(i, _)| (i < offset).then_some(i))
        .unwrap_or(0);
    if let Some(t) = buf.token_inside(offset) {
        target = target.min(t.range.start);
    }
    match buf.token_inside(target) {
        Some(t) => t.range.start,
        None => target,
    }
}

/// The end of the grapheme cluster (or token) after `offset`.
pub fn next_grapheme(buf: &Buffer, offset: usize) -> usize {
    let offset = buf.clamp(offset);
    let mut target = buf
        .text()
        .grapheme_indices(true)
        .find_map(|(i, _)| (i > offset).then_some(i))
        .unwrap_or(buf.len());
    if let Some(t) = buf.token_inside(offset) {
        target = target.max(t.range.end);
    }
    match buf.token_inside(target) {
        Some(t) => t.range.end,
        None => target,
    }
}

fn is_blank(segment: &str) -> bool {
    segment.chars().all(char::is_whitespace)
}

/// The start of the word before `offset`. Spaces and line breaks are skipped first.
pub fn prev_word(buf: &Buffer, offset: usize) -> usize {
    let offset = buf.clamp(offset);
    let target = buf
        .text()
        .split_word_bound_indices()
        .rev()
        .filter(|(i, _)| *i < offset)
        .find(|(_, segment)| !is_blank(segment))
        .map_or(0, |(i, _)| i);
    snap_back(buf, target)
}

/// The end of the word after `offset`. Spaces and line breaks are skipped first.
pub fn next_word(buf: &Buffer, offset: usize) -> usize {
    let offset = buf.clamp(offset);
    let target = buf
        .text()
        .split_word_bound_indices()
        .filter(|(i, segment)| i + segment.len() > offset)
        .find(|(_, segment)| !is_blank(segment))
        .map_or(buf.len(), |(i, segment)| i + segment.len());
    snap_forward(buf, target)
}

fn snap_back(buf: &Buffer, offset: usize) -> usize {
    buf.token_inside(offset).map_or(offset, |t| t.range.start)
}

fn snap_forward(buf: &Buffer, offset: usize) -> usize {
    buf.token_inside(offset).map_or(offset, |t| t.range.end)
}

/// The offset just after the previous newline, or 0.
pub fn line_start(buf: &Buffer, offset: usize) -> usize {
    let offset = buf.clamp(offset);
    buf.text()[..offset].rfind('\n').map_or(0, |i| i + 1)
}

/// The offset of the next newline, or the end of the text.
pub fn line_end(buf: &Buffer, offset: usize) -> usize {
    let offset = buf.clamp(offset);
    buf.text()[offset..]
        .find('\n')
        .map_or(buf.len(), |i| offset + i)
}

/// The word (or token, or run of spaces) at `offset`, for a double click.
pub fn word_range_at(buf: &Buffer, offset: usize) -> Range<usize> {
    let offset = buf.clamp(offset);
    if let Some(t) = buf
        .tokens()
        .iter()
        .find(|t| t.range.start <= offset && offset < t.range.end)
    {
        return t.range.clone();
    }
    for (i, segment) in buf.text().split_word_bound_indices() {
        let end = i + segment.len();
        if i <= offset && offset < end {
            return buf.expand_range(i..end);
        }
    }
    offset..offset
}

/// The whole line at `offset`, without its newline, for a triple click.
pub fn line_range_at(buf: &Buffer, offset: usize) -> Range<usize> {
    line_start(buf, offset)..line_end(buf, offset)
}

/// The grapheme boundary nearest to `offset`. A click that lands inside a cluster (between a
/// letter and its combining mark) goes to the nearer edge. A token counts as one cluster.
pub fn nearest_grapheme_boundary(buf: &Buffer, offset: usize) -> usize {
    let o = buf.snap_nearest(offset);
    for (i, cluster) in buf.text().grapheme_indices(true) {
        if i >= o {
            break;
        }
        let end = i + cluster.len();
        if o < end {
            return if o - i < end - o { i } else { end };
        }
    }
    o
}

/// Where Backspace deletes back to from `offset`.
///
/// A token, an emoji sequence or a single character goes whole. For letters with combining
/// marks and for scripts where a cluster is built from several parts (Devanagari vowel signs,
/// for example), Backspace removes one code point, as editors on all platforms do. That lets
/// the user fix one vowel sign without retyping the whole syllable.
pub fn prev_backspace_boundary(buf: &Buffer, offset: usize) -> usize {
    let offset = buf.clamp(offset);
    let start = prev_grapheme(buf, offset);
    if buf.token_inside(offset).is_some() || buf.tokens().iter().any(|t| t.range.end == offset) {
        return start;
    }
    let cluster = &buf.text()[start..offset];
    if cluster.chars().nth(1).is_none() || is_emoji_cluster(cluster) {
        return start;
    }
    cluster
        .char_indices()
        .next_back()
        .map_or(start, |(i, _)| start + i)
}

fn is_emoji_cluster(cluster: &str) -> bool {
    cluster.chars().any(|c| {
        let n = c as u32;
        n >= 0x1f000
            || (0x2190..=0x2bff).contains(&n)
            || n == 0xfe0f
            || n == 0x20e3
            || (0xe0020..=0xe007f).contains(&n)
    })
}
