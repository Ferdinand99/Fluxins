// SPDX-License-Identifier: AGPL-3.0-or-later
//! What may enter the buffer, and what the server removes before it reads a message.
//!
//! Fluxer's `normalizeString` removes every U+202E (right-to-left override) and every
//! U+000C (form feed) from message content, then trims it. The server reads mentions in the
//! result. A character that is removed there must never sit between a backslash and the tag
//! it is meant to escape, or the escape stops working. The editor therefore drops these
//! characters on the way in, and `to_wire` ignores them again when it computes the escapes.

/// Characters the server removes from message content (before it trims).
pub(crate) fn server_strips(c: char) -> bool {
    matches!(c, '\u{202e}' | '\u{000c}')
}

/// Characters that never enter the buffer: everything the server removes, the other bidi
/// controls (which can make the composer show text in another order than it is sent), and
/// control characters except the line feed and the tab.
pub(crate) fn dropped_on_input(c: char) -> bool {
    server_strips(c)
        || matches!(
            c,
            '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{061c}'
        )
        || (c.is_control() && c != '\n' && c != '\t')
}

/// Text as it goes into the buffer: line breaks are `\n`, and no character is dropped-on-input.
pub(crate) fn sanitize_input(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            c if dropped_on_input(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// A lower bound for the UTF-16 length of `sanitize_input(s)`, without allocating. Used to
/// refuse a huge paste before any work is done on it.
pub(crate) fn sanitized_units_lower_bound(s: &str) -> usize {
    s.chars()
        .filter(|c| !dropped_on_input(*c))
        .map(char::len_utf16)
        .sum()
}

/// A label for a pill. Names come from the server, so line breaks, tabs and control or bidi
/// characters are removed.
pub(crate) fn clean_label(s: &str) -> String {
    let spaced: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .filter(|c| !dropped_on_input(*c))
        .collect();
    spaced.trim().to_owned()
}

/// Trims white space the way JavaScript's `String.prototype.trim` does, which is what the
/// server does to message content. The set differs from Rust's `char::is_whitespace`: JS
/// keeps U+0085 (next line) and removes U+FEFF.
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(
            c,
            '\u{0009}'..='\u{000d}'
                | '\u{0020}'
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

/// `sanitize_input(s) == s`, without making a copy.
pub(crate) fn is_clean(s: &str) -> bool {
    !s.chars().any(|c| c == '\r' || dropped_on_input(c))
}
