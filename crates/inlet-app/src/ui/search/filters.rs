// SPDX-License-Identifier: AGPL-3.0-or-later
//! The filters of a message search, as typed fields.
//!
//! The search field takes Fluxer's filter words (`from:`, `mentions:`, `in:`, `has:`,
//! `before:`, `after:`, `pinned:`) beside the free text. [`parse`] splits what was
//! typed into words and filters, [`resolve`] turns names into IDs through the account
//! state (a name it cannot place is reported, never sent as text), and
//! [`Filters::query`] fills the protocol's [`p::MessageSearch`] field by field. No query
//! string is ever put together by hand, so nothing typed can change the request's
//! shape; the protocol checks every value against Fluxer's limits before sending.

use chrono::{NaiveDate, TimeZone};
use inlet_protocol as p;
use inlet_protocol::api::search::search_has;
use inlet_state::{ChannelId, GuildId, UserId};

/// A filter word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    From,
    Mentions,
    In,
    Has,
    Before,
    After,
    Pinned,
}

impl Key {
    /// Every filter, in the order the search options list them (Fluxer's order).
    pub const ALL: [Key; 7] = [
        Key::From,
        Key::Mentions,
        Key::Has,
        Key::Before,
        Key::After,
        Key::In,
        Key::Pinned,
    ];

    /// The word as typed, without the colon.
    pub fn word(self) -> &'static str {
        match self {
            Key::From => "from",
            Key::Mentions => "mentions",
            Key::In => "in",
            Key::Has => "has",
            Key::Before => "before",
            Key::After => "after",
            Key::Pinned => "pinned",
        }
    }

    fn of(word: &str) -> Option<Key> {
        Key::ALL
            .into_iter()
            .find(|k| k.word().eq_ignore_ascii_case(word))
    }
}

/// What a message must have (`has:`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Has {
    Link,
    Embed,
    File,
    Image,
    Video,
    Sound,
    Sticker,
    Forward,
}

impl Has {
    /// In the order Fluxer offers them.
    pub const ALL: [Has; 8] = [
        Has::Image,
        Has::Video,
        Has::Link,
        Has::File,
        Has::Embed,
        Has::Sound,
        Has::Sticker,
        Has::Forward,
    ];

    /// The word as typed after `has:`.
    pub fn word(self) -> &'static str {
        match self {
            Has::Link => "link",
            Has::Embed => "embed",
            Has::File => "file",
            Has::Image => "image",
            Has::Video => "video",
            Has::Sound => "sound",
            Has::Sticker => "sticker",
            Has::Forward => "forward",
        }
    }

    /// The API's value.
    fn api(self) -> &'static str {
        match self {
            Has::Link => search_has::LINK,
            Has::Embed => search_has::EMBED,
            Has::File => search_has::FILE,
            Has::Image => search_has::IMAGE,
            Has::Video => search_has::VIDEO,
            Has::Sound => search_has::SOUND,
            Has::Sticker => search_has::STICKER,
            Has::Forward => search_has::SNAPSHOT,
        }
    }

    fn of(word: &str) -> Option<Has> {
        Has::ALL
            .into_iter()
            .find(|h| h.word().eq_ignore_ascii_case(word))
    }
}

/// Where to search: the open guild, or the open DM or group DM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Guild(GuildId),
    Channel(ChannelId),
}

/// The order of the results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Newest,
    Oldest,
    Relevance,
}

impl Sort {
    /// The next sort for the toggle in the results header.
    pub fn next(self) -> Sort {
        match self {
            Sort::Newest => Sort::Oldest,
            Sort::Oldest => Sort::Relevance,
            Sort::Relevance => Sort::Newest,
        }
    }
}

/// A search, resolved: the text and every filter as IDs, kinds and days.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filters {
    pub text: String,
    pub from: Vec<UserId>,
    pub mentions: Vec<UserId>,
    pub in_channels: Vec<ChannelId>,
    pub has: Vec<Has>,
    /// Sent before this day began.
    pub before: Option<NaiveDate>,
    /// Sent after this day ended.
    pub after: Option<NaiveDate>,
    pub pinned: bool,
}

impl Filters {
    /// Nothing to search for.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.from.is_empty()
            && self.mentions.is_empty()
            && self.in_channels.is_empty()
            && self.has.is_empty()
            && self.before.is_none()
            && self.after.is_none()
            && !self.pinned
    }

    /// The request for page `page` (1-based): each filter goes into its own field.
    /// Days are taken in `tz` (the computer's time zone in the app).
    pub fn query<Tz: TimeZone>(
        &self,
        scope: Scope,
        sort: Sort,
        page: u16,
        tz: &Tz,
    ) -> p::MessageSearch {
        let mut q = match scope {
            Scope::Guild(g) => p::MessageSearch::in_guild(g.to_string()),
            Scope::Channel(ch) => p::MessageSearch::in_channel(ch.to_string()),
        };
        let text = self.text.trim();
        if !text.is_empty() {
            q = q.text(text);
        }
        for u in &self.from {
            q = q.from_author(u.to_string());
        }
        for u in &self.mentions {
            q = q.mentioning(u.to_string());
        }
        // `in:` only narrows a guild search; a DM search is one channel already.
        if matches!(scope, Scope::Guild(_)) {
            for ch in &self.in_channels {
                q = q.in_channels(ch.to_string());
            }
        }
        for h in &self.has {
            if !q.has.iter().any(|x| x == h.api()) {
                q = q.with(h.api());
            }
        }
        if let Some(day) = self.before {
            q = q.before_time(start_of(day, tz));
        }
        if let Some(day) = self.after {
            // After the day: from the start of the next one (Fluxer's `after:`).
            let next = day.succ_opt().unwrap_or(day);
            q = q.after_time(start_of(next, tz));
        }
        if self.pinned {
            q = q.pinned_only();
        }
        match sort {
            Sort::Newest => {}
            Sort::Oldest => q = q.oldest_first(),
            Sort::Relevance => q.sort_by = Some("relevance".into()),
        }
        q.hits_per_page = Some(crate::backend::search::PAGE_SIZE);
        q.page(page.clamp(1, 400))
    }
}

/// The Unix time in milliseconds when `day` began in `tz`.
fn start_of<Tz: TimeZone>(day: NaiveDate, tz: &Tz) -> u64 {
    let midnight = day.and_hms_opt(0, 0, 0).unwrap_or_default();
    tz.from_local_datetime(&midnight)
        .earliest()
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|| midnight.and_utc().timestamp_millis())
        .max(0) as u64
}

/// One filter as typed: its word and its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub key: Key,
    pub value: String,
}

/// The search field split into free text and filters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub text: String,
    pub tokens: Vec<Token>,
}

/// Splits the search field into words and `key:value` filters. A value may be quoted
/// (`from:"Ada L"`). A word with an unknown key stays text.
pub fn parse(input: &str) -> Parsed {
    let mut out = Parsed::default();
    let mut words: Vec<String> = Vec::new();
    let mut rest = input.trim_start();
    while !rest.is_empty() {
        let (word, after) = next_word(rest);
        rest = after.trim_start();
        let filter = word
            .split_once(':')
            .and_then(|(k, v)| Some((Key::of(k)?, v)));
        match filter {
            Some((key, value)) => {
                let value = value.trim_matches('"').trim().to_string();
                if !value.is_empty() {
                    out.tokens.push(Token { key, value });
                }
            }
            None => words.push(word.to_string()),
        }
    }
    out.text = words.join(" ");
    out
}

/// The first word of `s` (a quoted part keeps its spaces) and what follows it.
fn next_word(s: &str) -> (&str, &str) {
    let mut quoted = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => return (&s[..i], &s[i..]),
            _ => {}
        }
    }
    (s, "")
}

/// The filter being typed at the end of the field, for the suggestions: its key and the
/// part of the value typed so far, and where the filter starts.
pub fn typing_filter(input: &str) -> Option<(Key, &str, usize)> {
    let start = input
        .rfind(|c: char| c.is_whitespace())
        .map_or(0, |i| i + 1);
    let word = &input[start..];
    let (key, value) = word.split_once(':')?;
    Some((Key::of(key)?, value.trim_start_matches('"'), start))
}

/// A filter that could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    UnknownUser(String),
    UnknownChannel(String),
    UnknownHas(String),
    BadDate(String),
    /// `pinned:` takes `true` or `false` only.
    BadPinned(String),
}

/// Turns the typed filters into IDs and days: `user` finds a user by name, mention or
/// ID, `channel` a channel of the guild by name or ID. Every filter that cannot be
/// placed is reported; the search then does not start.
pub fn resolve(
    parsed: &Parsed,
    user: impl Fn(&str) -> Option<UserId>,
    channel: impl Fn(&str) -> Option<ChannelId>,
) -> Result<Filters, Vec<Problem>> {
    let mut f = Filters {
        text: parsed.text.clone(),
        ..Filters::default()
    };
    let mut problems = Vec::new();
    for t in &parsed.tokens {
        let v = t.value.as_str();
        match t.key {
            Key::From | Key::Mentions => match user(v) {
                Some(id) => {
                    let list = if t.key == Key::From {
                        &mut f.from
                    } else {
                        &mut f.mentions
                    };
                    if !list.contains(&id) {
                        list.push(id);
                    }
                }
                None => problems.push(Problem::UnknownUser(v.to_string())),
            },
            Key::In => match channel(v) {
                Some(id) if !f.in_channels.contains(&id) => f.in_channels.push(id),
                Some(_) => {}
                None => problems.push(Problem::UnknownChannel(v.to_string())),
            },
            Key::Has => match Has::of(v) {
                Some(h) if !f.has.contains(&h) => f.has.push(h),
                Some(_) => {}
                None => problems.push(Problem::UnknownHas(v.to_string())),
            },
            Key::Before | Key::After => match day_of(v) {
                Some(day) if t.key == Key::Before => f.before = Some(day),
                Some(day) => f.after = Some(day),
                None => problems.push(Problem::BadDate(v.to_string())),
            },
            Key::Pinned => match v {
                "true" => f.pinned = true,
                "false" => f.pinned = false,
                _ => problems.push(Problem::BadPinned(v.to_string())),
            },
        }
    }
    if problems.is_empty() {
        Ok(f)
    } else {
        Err(problems)
    }
}

/// A day written as year-month-day (`2026-03-01`).
pub fn day_of(value: &str) -> Option<NaiveDate> {
    let mut parts = value.splitn(3, '-').map(|p| p.trim().parse::<u32>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    NaiveDate::from_ymd_opt(i32::try_from(y).ok()?, m, d)
}

/// A user ID from a mention (`<@123>`, `<@!123>`) or a bare ID.
pub fn user_id_in(value: &str) -> Option<UserId> {
    let inner = value
        .strip_prefix("<@")
        .and_then(|v| v.strip_suffix('>'))
        .map(|v| v.trim_start_matches('!'))
        .unwrap_or(value);
    UserId::parse(inner)
}

/// A channel ID from a channel mention (`<#123>`) or a bare ID.
pub fn channel_id_in(value: &str) -> Option<ChannelId> {
    let inner = value
        .strip_prefix("<#")
        .and_then(|v| v.strip_suffix('>'))
        .unwrap_or(value);
    ChannelId::parse(inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn user(v: &str) -> Option<UserId> {
        match v {
            "ada" => UserId::parse("20"),
            _ => user_id_in(v),
        }
    }

    fn channel(v: &str) -> Option<ChannelId> {
        match v.trim_start_matches('#') {
            "general" => ChannelId::parse("700"),
            _ => channel_id_in(v),
        }
    }

    fn ms(y: i32, m: u32, d: u32) -> u64 {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis() as u64
    }

    #[test]
    fn filters_from_in_has_before_after_mentions_build_the_query() {
        let typed = [
            "hello",
            "from:ada",
            "in:general",
            "has:link",
            "has:image",
            "before:2026-03-10",
            "after:2026-03-01",
            "mentions:30",
            "world",
            "pinned:true",
            "has:link",
        ]
        .join(" ");
        let parsed = parse(&typed);
        assert_eq!(parsed.text, "hello world");
        let filters = resolve(&parsed, user, channel).expect("every filter resolves");
        assert_eq!(filters.from, vec![UserId::parse("20").unwrap()]);
        assert_eq!(filters.mentions, vec![UserId::parse("30").unwrap()]);
        assert_eq!(filters.in_channels, vec![ChannelId::parse("700").unwrap()]);
        assert_eq!(filters.has, vec![Has::Link, Has::Image]);
        // Mentions and channel links resolve as well as names.
        assert_eq!(user_id_in("<@!30>"), UserId::parse("30"));
        assert_eq!(channel("<#700>"), ChannelId::parse("700"));
        assert_eq!(channel("#general"), ChannelId::parse("700"));

        let guild = GuildId::parse("600").unwrap();
        let q = filters.query(Scope::Guild(guild), Sort::Newest, 2, &Utc);
        let body = q.to_json().expect("a valid request");
        assert_eq!(body["context_guild_id"], json!("600"));
        assert_eq!(body["content"], json!("hello world"));
        assert_eq!(body["author_id"], json!(["20"]));
        assert_eq!(body["mentions"], json!(["30"]));
        assert_eq!(body["channel_ids"], json!(["700"]));
        assert_eq!(body["has"], json!(["link", "image"]));
        assert_eq!(body["pinned"], json!(true));
        // before: the start of that day; after: the end of that day.
        let before = p::models::snowflake_at_ms(ms(2026, 3, 10));
        let after = p::models::snowflake_at_ms(ms(2026, 3, 2));
        assert_eq!(body["max_id"], json!(before));
        assert_eq!(body["min_id"], json!(after));
        assert_eq!(body["page"], json!(2));
        assert_eq!(body["hits_per_page"], json!(25));
        assert_eq!(body.get("sort_order"), None);

        // A DM search is the one channel: `in:` is not sent, the order is asked for.
        let dm = ChannelId::parse("901").unwrap();
        let body = filters
            .query(Scope::Channel(dm), Sort::Oldest, 1, &Utc)
            .to_json()
            .unwrap();
        assert_eq!(body["context_channel_id"], json!("901"));
        assert_eq!(body.get("channel_ids"), None);
        assert_eq!(body["sort_order"], json!("asc"));
    }

    /// What is typed only ever lands in a field's value: words that look like JSON or
    /// other filters stay text, and names that cannot be placed stop the search.
    #[test]
    fn typed_text_never_changes_the_request_shape() {
        let json_like = "{\"author_id\":[\"1\"]}";
        let typed = [
            json_like,
            "from:nobody",
            "in:nowhere",
            "has:poll",
            "before:soon",
        ]
        .join(" ");
        let parsed = parse(&typed);
        assert_eq!(parsed.text, json_like);
        let problems = resolve(&parsed, user, channel).unwrap_err();
        assert_eq!(
            problems,
            vec![
                Problem::UnknownUser("nobody".into()),
                Problem::UnknownChannel("nowhere".into()),
                Problem::UnknownHas("poll".into()),
                Problem::BadDate("soon".into()),
            ]
        );
        // A quoted value keeps its spaces.
        assert_eq!(parse("from:\"Grace H\" x").tokens[0].value, "Grace H");
        let only_text = Filters {
            text: parsed.text.clone(),
            ..Filters::default()
        };
        let body = only_text
            .query(
                Scope::Channel(ChannelId::parse("901").unwrap()),
                Sort::Relevance,
                1,
                &Utc,
            )
            .to_json()
            .unwrap();
        assert!(body.get("author_id").is_none());
        assert_eq!(body["content"], json!(json_like));
        assert_eq!(body["sort_by"], json!("relevance"));
    }

    #[test]
    fn the_filter_being_typed_is_found() {
        assert_eq!(typing_filter("hi from:ad"), Some((Key::From, "ad", 3)));
        assert_eq!(typing_filter("has:"), Some((Key::Has, "", 0)));
        assert_eq!(typing_filter("hello"), None);
        assert_eq!(typing_filter("hello "), None);
        assert_eq!(day_of("2026-02-30"), None);
        let maybe = parse("pinned:maybe");
        assert_eq!(
            resolve(&maybe, user, channel).unwrap_err(),
            vec![Problem::BadPinned("maybe".into())]
        );
        assert!(Filters::default().is_empty());
    }
}
