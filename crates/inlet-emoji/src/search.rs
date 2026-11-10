// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shortcode search. The tiers and the order inside a tier follow Fluxer's emoji search
//! (`EmojiSearchIndex.ts`), without the frecency boost: Inlet has no usage history yet.

use std::sync::OnceLock;

use crate::data::{all, Emoji};

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

struct Meta {
    lower_names: Vec<String>,
    /// Names without `_` and `-`, so that `thumbsup` finds `thumbs_up`.
    compact_names: Vec<String>,
    lower_keywords: Vec<String>,
}

fn compact(text: &str) -> String {
    text.chars().filter(|c| !matches!(c, '_' | '-')).collect()
}

fn metas() -> &'static [Meta] {
    static METAS: OnceLock<Vec<Meta>> = OnceLock::new();
    METAS.get_or_init(|| {
        all()
            .iter()
            .enumerate()
            .map(|(index, emoji)| {
                let lower_names: Vec<String> =
                    emoji.names.iter().map(|n| n.to_lowercase()).collect();
                Meta {
                    compact_names: lower_names.iter().map(|n| compact(n)).collect(),
                    lower_names,
                    lower_keywords: crate::data::keywords(index)
                        .iter()
                        .map(|k| k.to_lowercase())
                        .collect(),
                }
            })
            .collect()
    })
}

fn is_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|b| b == b'_' || b.is_ascii_uppercase())
}

/// `query` found in `name` between word boundaries (`_` or a capital), allowing a plural
/// `s` after it.
fn has_word_match(name: &str, lower_name: &str, query: &str) -> bool {
    if name.len() != lower_name.len() {
        return false;
    }
    let bytes = name.as_bytes();
    let lower = lower_name.as_bytes();
    let mut from = 0;
    while let Some(found) = lower_name[from..].find(query) {
        let start = from + found;
        let end = start + query.len();
        if is_boundary(start.checked_sub(1).map(|i| bytes[i])) {
            if is_boundary(bytes.get(end).copied()) {
                return true;
            }
            if lower.get(end) == Some(&b's') && is_boundary(bytes.get(end + 1).copied()) {
                return true;
            }
        }
        from = start + 1;
        while !lower_name.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

fn tier(emoji: &Emoji, meta: &Meta, query: &str, compact_query: &str) -> Option<Tier> {
    if meta.lower_names.iter().any(|n| n == query) {
        return Some(Tier::ExactName);
    }
    let prefix = |i: usize| {
        meta.lower_names[i].starts_with(query)
            || (!compact_query.is_empty() && meta.compact_names[i].starts_with(compact_query))
    };
    if (0..meta.lower_names.len()).any(prefix) {
        return Some(Tier::NamePrefix);
    }
    if emoji
        .names
        .iter()
        .zip(&meta.lower_names)
        .any(|(name, lower)| has_word_match(name, lower, query))
    {
        return Some(Tier::NameWord);
    }
    let contains = |i: usize| {
        meta.lower_names[i].contains(query)
            || (!compact_query.is_empty() && meta.compact_names[i].contains(compact_query))
    };
    if (0..meta.lower_names.len()).any(contains) {
        return Some(Tier::NameSubstring);
    }
    let mut keyword_substring = false;
    for keyword in &meta.lower_keywords {
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
    for (index, (emoji, meta)) in all().iter().zip(metas()).enumerate() {
        if let Some(tier) = tier(emoji, meta, &query, &compact_query) {
            hits.push((tier, &meta.lower_names[0], index));
        }
    }
    hits.sort_unstable();
    hits.into_iter()
        .take(limit)
        .map(|(_, _, index)| &all()[index])
        .collect()
}
