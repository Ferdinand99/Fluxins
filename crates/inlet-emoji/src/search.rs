// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shortcode search. The tiers and the order inside a tier follow Fluxer's emoji search
//! (`EmojiSearchIndex.ts`), without the frecency boost: Inlet has no usage history yet.
//!
//! `build.rs` guarantees that every name and keyword is lower case, so the data is
//! compared as it is.

use std::sync::OnceLock;

use crate::data::{all, keywords, Emoji};

/// How well a query matches, best first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    ExactName,
    NamePrefix,
    NameWord,
    NameSubstring,
    KeywordPrefix,
    KeywordSubstring,
}

fn is_compact_char(c: char) -> bool {
    !matches!(c, '_' | '-')
}

/// A name without `_` and `-`, so that `thumbsup` finds `thumbs_up`.
fn compact(text: &str) -> String {
    text.chars().filter(|&c| is_compact_char(c)).collect()
}

/// Per emoji and name, the compact form, kept only where it differs from the name.
fn compact_names() -> &'static [Vec<Option<String>>] {
    static COMPACT: OnceLock<Vec<Vec<Option<String>>>> = OnceLock::new();
    COMPACT.get_or_init(|| {
        all()
            .iter()
            .map(|emoji| {
                emoji
                    .names
                    .iter()
                    .map(|name| name.contains(['_', '-']).then(|| compact(name)))
                    .collect()
            })
            .collect()
    })
}

fn is_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|b| b == b'_')
}

/// `query` found in `name` between word boundaries (`_`), allowing a plural `s` after it.
fn has_word_match(name: &str, query: &str) -> bool {
    let bytes = name.as_bytes();
    let mut from = 0;
    while let Some(found) = name[from..].find(query) {
        let start = from + found;
        let end = start + query.len();
        if is_boundary(start.checked_sub(1).map(|i| bytes[i])) {
            if is_boundary(bytes.get(end).copied()) {
                return true;
            }
            if bytes.get(end) == Some(&b's') && is_boundary(bytes.get(end + 1).copied()) {
                return true;
            }
        }
        from = start + 1;
        while !name.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

fn tier(index: usize, emoji: &Emoji, query: &str, compact_query: &str) -> Option<Tier> {
    if emoji.names.contains(&query) {
        return Some(Tier::ExactName);
    }
    let compact_of = |i: usize| -> &str {
        compact_names()[index][i]
            .as_deref()
            .unwrap_or(emoji.names[i])
    };
    let compact_ok = !compact_query.is_empty();
    let mut names = 0..emoji.names.len();
    if names.clone().any(|i| {
        emoji.names[i].starts_with(query)
            || (compact_ok && compact_of(i).starts_with(compact_query))
    }) {
        return Some(Tier::NamePrefix);
    }
    if emoji.names.iter().any(|name| has_word_match(name, query)) {
        return Some(Tier::NameWord);
    }
    if names.any(|i| {
        emoji.names[i].contains(query) || (compact_ok && compact_of(i).contains(compact_query))
    }) {
        return Some(Tier::NameSubstring);
    }
    let mut keyword_substring = false;
    for keyword in keywords(index) {
        if keyword.starts_with(query) {
            return Some(Tier::KeywordPrefix);
        }
        keyword_substring |= keyword.contains(query);
    }
    keyword_substring.then_some(Tier::KeywordSubstring)
}

/// Up to `limit` emoji for a query typed after a colon (`smi`, `thumbs_up`). A leading
/// colon is ignored and the case does not matter.
///
/// Names that start with the query come first, then names that contain it at a word, then
/// anywhere in a name, then the keywords (`happy` finds `grinning`). Inside a tier the
/// emoji are ordered by their main name. An empty query gives the first emoji of the
/// picker, as Fluxer does.
#[must_use]
pub fn search(query: &str, limit: usize) -> Vec<&'static Emoji> {
    let query = query.trim();
    let query = query.strip_prefix(':').unwrap_or(query).to_lowercase();
    if limit == 0 {
        return Vec::new();
    }
    if query.is_empty() {
        return all().iter().take(limit).collect();
    }
    let compact_query = compact(&query);
    let mut hits: Vec<(Tier, &str, usize)> = Vec::new();
    for (index, emoji) in all().iter().enumerate() {
        if let Some(tier) = tier(index, emoji, &query, &compact_query) {
            hits.push((tier, emoji.names[0], index));
        }
    }
    hits.sort_unstable();
    hits.into_iter()
        .take(limit)
        .map(|(_, _, index)| &all()[index])
        .collect()
}
