// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tokens: mentions and custom emoji that live in the buffer as one unit.
//!
//! A token shows as a pill with a label (`@alice`) and is sent as wire text (`<@123>`).

use std::ops::Range;

use crate::sanitize::{clean_label, dropped_on_input};

/// A snowflake id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub u64);

/// What a pill stands for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Token {
    User(Id),
    Role(Id),
    Channel(Id),
    Everyone,
    Here,
    CustomEmoji {
        id: Id,
        name: String,
        animated: bool,
    },
}

impl Token {
    /// The text Fluxer receives for this token.
    pub fn to_wire(&self) -> String {
        match self {
            Token::User(id) => format!("<@{}>", id.0),
            Token::Role(id) => format!("<@&{}>", id.0),
            Token::Channel(id) => format!("<#{}>", id.0),
            Token::Everyone => "@everyone".to_owned(),
            Token::Here => "@here".to_owned(),
            Token::CustomEmoji { id, name, animated } => {
                // The name can come from the server. Only a plain name may reach the wire,
                // or a crafted name could close the tag and open a mention.
                let name = safe_emoji_name(name);
                format!("<{}:{}:{}>", if *animated { "a" } else { "" }, name, id.0)
            }
        }
    }

    /// Parses one token at the very start of `s`. Returns the token and how many bytes it used.
    pub fn parse_wire(s: &str) -> Option<(Token, usize)> {
        if s.starts_with("@everyone") {
            return Some((Token::Everyone, "@everyone".len()));
        }
        if s.starts_with("@here") {
            return Some((Token::Here, "@here".len()));
        }
        let rest = s.strip_prefix('<')?;
        // The longest tag is `<a:` + 32 name bytes + `:` + 20 digits + `>`.
        let end = rest.bytes().take(64).position(|b| b == b'>')?;
        let (inner, len) = (&rest[..end], end + 2);
        let token = if let Some(rest) = inner.strip_prefix("@&") {
            Token::Role(parse_id(rest)?)
        } else if let Some(rest) = inner.strip_prefix("@!") {
            // The old nickname form of a user mention.
            Token::User(parse_id(rest)?)
        } else if let Some(rest) = inner.strip_prefix('@') {
            Token::User(parse_id(rest)?)
        } else if let Some(rest) = inner.strip_prefix('#') {
            Token::Channel(parse_id(rest)?)
        } else {
            let (animated, rest) = match inner.strip_prefix("a:") {
                Some(rest) => (true, rest),
                None => (false, inner.strip_prefix(':')?),
            };
            let (name, id) = rest.split_once(':')?;
            if !valid_emoji_name(name) {
                return None;
            }
            Token::CustomEmoji {
                id: parse_id(id)?,
                name: name.to_owned(),
                animated,
            }
        };
        Some((token, len))
    }

    /// True when the wire text starts with `<` (the `@everyone` and `@here` forms do not).
    pub(crate) fn wire_starts_with_angle(&self) -> bool {
        !matches!(self, Token::Everyone | Token::Here)
    }
}

fn parse_id(s: &str) -> Option<Id> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok().map(Id)
}

/// What the parser accepts in a custom emoji name: letters, digits, `_`, `-` and any
/// non-ASCII character. White space, control and bidi characters are left out here, so a
/// name from the server cannot hide anything.
fn is_emoji_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || c == '_'
        || c == '-'
        || (!c.is_ascii() && !c.is_whitespace() && !dropped_on_input(c))
}

fn safe_emoji_name(name: &str) -> String {
    if valid_emoji_name(name) {
        return name.to_owned();
    }
    let cleaned: String = name
        .chars()
        .filter(|c| is_emoji_name_char(*c))
        .take(32)
        .collect();
    if cleaned.is_empty() {
        "emoji".to_owned()
    } else {
        cleaned
    }
}

fn valid_emoji_name(name: &str) -> bool {
    (1..=32).contains(&name.chars().count()) && name.chars().all(is_emoji_name_char)
}

/// Which kind of wire syntax the text at the start of `s` could be mistaken for.
/// Plain text that starts like this must be escaped in `to_wire`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Special {
    /// `<@`, `<#`, `<:` or `<a:`: a mention or custom emoji tag.
    Angle,
    /// `@everyone` or `@here`.
    At,
}

pub(crate) fn special_at(s: &str) -> Option<Special> {
    // `<@everyone` and `<@here` are not tags (an id is digits). Only their `@` is special,
    // and it gets its own escape.
    if s.starts_with("<@everyone") || s.starts_with("<@here") {
        None
    } else if s.starts_with("<@")
        || s.starts_with("<#")
        || s.starts_with("<:")
        || s.starts_with("<a:")
    {
        Some(Special::Angle)
    } else if s.starts_with("@everyone") || s.starts_with("@here") {
        Some(Special::At)
    } else {
        None
    }
}

/// A token and the bytes of the display text it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenSpan {
    pub range: Range<usize>,
    pub token: Token,
}

/// Looks up display names when text from the server is turned into pills.
/// Return the bare name: no `@` or `#`.
pub trait Resolve {
    fn user(&self, id: Id) -> Option<String>;
    fn role(&self, id: Id) -> Option<String>;
    fn channel(&self, id: Id) -> Option<String>;
}

/// A resolver that knows nobody. Every token gets a placeholder label.
pub struct NoNames;

impl Resolve for NoNames {
    fn user(&self, _: Id) -> Option<String> {
        None
    }
    fn role(&self, _: Id) -> Option<String> {
        None
    }
    fn channel(&self, _: Id) -> Option<String> {
        None
    }
}

/// The label a token gets when it is read from wire text.
pub(crate) fn label_for(token: &Token, names: &dyn Resolve) -> String {
    let named = |name: Option<String>, sigil: char, fallback: &str| {
        let name = name.map(|n| clean_label(&n)).filter(|n| !n.is_empty());
        format!("{sigil}{}", name.as_deref().unwrap_or(fallback))
    };
    match token {
        Token::User(id) => named(names.user(*id), '@', "unknown-user"),
        Token::Role(id) => named(names.role(*id), '@', "unknown-role"),
        Token::Channel(id) => named(names.channel(*id), '#', "unknown-channel"),
        Token::Everyone => "@everyone".into(),
        Token::Here => "@here".into(),
        Token::CustomEmoji { name, .. } => format!(":{}:", clean_label(name)),
    }
}
