// SPDX-License-Identifier: AGPL-3.0-or-later
//! Finds out whether a message is only emoji.

use std::collections::HashSet;
use std::sync::OnceLock;

use crate::data::{glyphs, shortcode_glyph};

/// Fluxer shows a message of up to this many emoji, and nothing else, in a larger size
/// (`MAX_JUMBO_EMOJI_COUNT` in its `JumboDetector.ts`).
pub const MAX_JUMBO_EMOJI: usize = 30;

struct GlyphSet {
    /// Known glyphs without `U+FE0F`.
    set: HashSet<String>,
    /// The longest of them, in chars.
    longest: usize,
}

fn glyph_set() -> &'static GlyphSet {
    static SET: OnceLock<GlyphSet> = OnceLock::new();
    SET.get_or_init(|| {
        let mut set: HashSet<String> = glyphs().map(|g| g.replace('\u{FE0F}', "")).collect();
        // Fluxer also treats the bare skin tone modifiers and hair components as emoji.
        for c in ('\u{1F3FB}'..='\u{1F3FF}').chain('\u{1F9B0}'..='\u{1F9B3}') {
            set.insert(c.to_string());
        }
        let longest = set.iter().map(|g| g.chars().count()).max().unwrap_or(1);
        GlyphSet { set, longest }
    })
}

/// Length in chars of the emoji at the start of `chars`, the longest match, counting a
/// trailing `U+FE0F`. A ZWJ sequence is one emoji.
fn unicode_emoji_len(chars: &[char]) -> Option<usize> {
    let known = glyph_set();
    let mut candidate = String::new();
    let mut stripped_chars = 0;
    let mut matched = None;
    for (index, &c) in chars.iter().enumerate() {
        if c != '\u{FE0F}' {
            stripped_chars += 1;
            if stripped_chars > known.longest {
                break;
            }
            candidate.push(c);
        }
        if !candidate.is_empty() && known.set.contains(&candidate) {
            matched = Some(index + 1);
        }
    }
    matched
}

/// Length in chars of a `<:name:id>` or `<a:name:id>` custom emoji at the start.
fn custom_emoji_len(chars: &[char]) -> Option<usize> {
    let mut i = 1;
    if *chars.first()? != '<' {
        return None;
    }
    if chars.get(i) == Some(&'a') {
        i += 1;
    }
    if chars.get(i) != Some(&':') {
        return None;
    }
    i += 1;
    let name_start = i;
    while chars
        .get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
    {
        i += 1;
    }
    if !(2..=32).contains(&(i - name_start)) || chars.get(i) != Some(&':') {
        return None;
    }
    i += 1;
    let id_start = i;
    while chars.get(i).is_some_and(char::is_ascii_digit) {
        i += 1;
    }
    (i > id_start && chars.get(i) == Some(&'>')).then_some(i + 1)
}

/// Length in chars of a known `:shortcode:` at the start.
fn shortcode_len(chars: &[char]) -> Option<usize> {
    if *chars.first()? != ':' {
        return None;
    }
    let close = chars[1..]
        .iter()
        .position(|&c| c == ':' || c.is_whitespace())?
        + 1;
    if chars[close] != ':' || close == 1 {
        return None;
    }
    // A tone suffix has two colons (`:wave::skin-tone-3:`).
    let mut end = close + 1;
    if chars.get(end) == Some(&':') {
        if let Some(rest) = chars[end + 1..]
            .iter()
            .position(|&c| c == ':' || c.is_whitespace())
        {
            if chars[end + 1 + rest] == ':' {
                end = end + 1 + rest + 1;
            }
        }
    }
    let text: String = chars[..end].iter().collect();
    if shortcode_glyph(&text).is_some() {
        return Some(end);
    }
    let text: String = chars[..=close].iter().collect();
    shortcode_glyph(&text).map(|_| close + 1)
}

/// How many emoji a message has, if it has nothing else but spaces: Unicode emoji (a
/// family or a flag counts once), known `:shortcodes:` and custom emoji (`<:name:id>`).
/// `None` if there is other text, no emoji, or more than [`MAX_JUMBO_EMOJI`], the case in
/// which Fluxer shows the message at the normal size.
#[must_use]
pub fn emoji_only(text: &str) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0;
    let mut count = 0;
    while at < chars.len() {
        if chars[at].is_whitespace() {
            at += 1;
            continue;
        }
        let rest = &chars[at..];
        let len = custom_emoji_len(rest)
            .or_else(|| shortcode_len(rest))
            .or_else(|| unicode_emoji_len(rest))?;
        at += len;
        count += 1;
        if count > MAX_JUMBO_EMOJI {
            return None;
        }
    }
    (count > 0).then_some(count)
}
