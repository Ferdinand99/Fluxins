// SPDX-License-Identifier: AGPL-3.0-or-later
//! Autocomplete for `@`, `#`, `:` and `/`: what the list offers for a trigger, and the
//! list itself. A pick becomes a real pill (a token with the server's ID) or plain text
//! (a Unicode emoji, a command). Every ID comes from the server's data: the members a
//! search returned, the guild's roles, channels and custom emoji, or the account state.
//! Typed text never turns into a mention, so a mention can never point at an ID the
//! person made up. The full emoji, GIF and sticker pickers live in `pickers`.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, SharedString};
use inlet_editor::{Id, Token, Trigger};
use inlet_state::{AccountState, ChannelId, GuildId};

use super::commands;
use super::editor::ComposerView;
use crate::backend::composer::{ChannelEntry, CustomEmoji, Directory, Person, Rights, RoleEntry};
use crate::theme;

/// Rows the list shows at most.
pub const MAX_ROWS: usize = 10;
/// Members remembered per guild from searches.
pub const MAX_MEMBERS: usize = 500;
const EVERYONE: &str = "everyone";
const HERE: &str = "here";

/// What a row puts into the message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// A pill with this label (sigil included).
    Token(Token, String),
    /// Text in place of the trigger.
    Text(String),
}

/// The group a list belongs to, shown as its heading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Mentions,
    Channels,
    Emoji,
    Commands,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The name shown.
    pub title: String,
    /// Shown dimmed after the name: account name, shortcode, description.
    pub detail: Option<String>,
    /// An emoji glyph shown in front.
    pub glyph: Option<String>,
    pub pick: Pick,
}

/// What autocomplete can offer in the open channel.
pub struct Sources<'a> {
    pub directory: Option<&'a Directory>,
    /// Members found by searches in this guild.
    pub members: &'a [Person],
    pub in_guild: bool,
    /// The account's rights in the open channel.
    pub rights: Rights,
}

/// The rows for a trigger, best first, at most [`MAX_ROWS`].
pub fn candidates(trigger: &Trigger, src: &Sources) -> (Section, Vec<Candidate>) {
    match trigger {
        Trigger::Mention(q) => (Section::Mentions, mentions(q, src)),
        Trigger::Channel(q) => (Section::Channels, channels(q, src)),
        Trigger::Emoji(q) => (Section::Emoji, emoji(q, src)),
        Trigger::Command(q) => (Section::Commands, command_rows(q, src.in_guild)),
    }
}

/// 0 for a prefix match, 1 for a match inside, `None` for no match.
fn rank(haystack: &str, q: &str) -> Option<u8> {
    let h = haystack.to_lowercase();
    if h.starts_with(q) {
        Some(0)
    } else if h.contains(q) {
        Some(1)
    } else {
        None
    }
}

fn mentions(query: &str, src: &Sources) -> Vec<Candidate> {
    if !src.in_guild {
        // Deviation: Fluxer offers the recipients in direct messages; the account state
        // does not hold them for the composer yet, so DMs get no mention rows.
        return Vec::new();
    }
    let q = query.to_lowercase();
    let mut ranked: Vec<(u8, Candidate)> = Vec::new();
    for p in src.members {
        let r = rank(&p.name, &q)
            .into_iter()
            .chain(rank(&p.username, &q))
            .min();
        if let Some(r) = r {
            ranked.push((
                r,
                Candidate {
                    title: p.name.clone(),
                    detail: (p.username != p.name).then(|| p.username.clone()),
                    glyph: None,
                    pick: Pick::Token(Token::User(Id(p.id)), format!("@{}", p.name)),
                },
            ));
        }
    }
    if let Some(dir) = src.directory {
        let all_roles = src.rights.mention_everyone;
        for role in dir.roles.iter().filter(|r| r.mentionable || all_roles) {
            if let Some(r) = rank(&role.name, &q) {
                ranked.push((
                    r,
                    Candidate {
                        title: format!("@{}", role.name),
                        detail: None,
                        glyph: None,
                        pick: Pick::Token(Token::Role(Id(role.id)), format!("@{}", role.name)),
                    },
                ));
            }
        }
        if src.rights.mention_everyone {
            for (name, token) in [(EVERYONE, Token::Everyone), (HERE, Token::Here)] {
                if name.starts_with(&q) {
                    ranked.push((
                        2,
                        Candidate {
                            title: format!("@{name}"),
                            detail: None,
                            glyph: None,
                            pick: Pick::Token(token, format!("@{name}")),
                        },
                    ));
                }
            }
        }
    }
    ranked.sort_by_key(|(r, _)| *r);
    ranked.into_iter().map(|(_, c)| c).take(MAX_ROWS).collect()
}

fn channels(query: &str, src: &Sources) -> Vec<Candidate> {
    let Some(dir) = src.directory else {
        return Vec::new();
    };
    let q = query.to_lowercase();
    let mut ranked: Vec<(u8, &ChannelEntry)> = dir
        .channels
        .iter()
        .filter_map(|c| rank(&c.name, &q).map(|r| (r, c)))
        .collect();
    ranked.sort_by_key(|(r, _)| *r);
    ranked
        .into_iter()
        .take(MAX_ROWS)
        .map(|(_, c)| Candidate {
            title: format!("#{}", c.name),
            detail: None,
            glyph: None,
            pick: Pick::Token(Token::Channel(Id(c.id)), format!("#{}", c.name)),
        })
        .collect()
}

fn emoji(query: &str, src: &Sources) -> Vec<Candidate> {
    let q = query.to_lowercase();
    let mut out: Vec<Candidate> = Vec::new();
    if let Some(dir) = src.directory {
        let mut custom: Vec<(u8, &CustomEmoji)> = dir
            .emojis
            .iter()
            .filter_map(|e| rank(&e.name, &q).map(|r| (r, e)))
            .collect();
        custom.sort_by_key(|(r, _)| *r);
        out.extend(custom.into_iter().take(MAX_ROWS).map(|(_, e)| Candidate {
            title: format!(":{}:", e.name),
            detail: None,
            glyph: None,
            pick: Pick::Token(
                Token::CustomEmoji {
                    id: Id(e.id),
                    name: e.name.clone(),
                    animated: e.animated,
                },
                format!(":{}:", e.name),
            ),
        }));
    }
    let room = MAX_ROWS.saturating_sub(out.len());
    out.extend(
        inlet_emoji::search(&q, room)
            .into_iter()
            .map(|e| Candidate {
                title: format!(":{}:", e.name()),
                detail: None,
                glyph: Some(e.glyph.to_owned()),
                pick: Pick::Text(format!("{} ", e.glyph)),
            }),
    );
    out
}

fn command_rows(query: &str, in_guild: bool) -> Vec<Candidate> {
    commands::matching(query, in_guild)
        .into_iter()
        .take(MAX_ROWS)
        .map(|c| Candidate {
            title: c.name.to_owned(),
            detail: Some(command_description(c.name)),
            glyph: None,
            pick: Pick::Text(commands::insertion(c)),
        })
        .collect()
}

fn command_description(name: &str) -> String {
    match name {
        "/shrug" => t!("composer-cmd-shrug"),
        "/tableflip" => t!("composer-cmd-tableflip"),
        "/unflip" => t!("composer-cmd-unflip"),
        "/me" => t!("composer-cmd-me"),
        "/spoiler" => t!("composer-cmd-spoiler"),
        "/nick" => t!("composer-cmd-nick"),
        "/msg" => t!("composer-cmd-msg"),
        _ => t!("composer-cmd-saved"),
    }
}

/// Adds members from a search to what is remembered for a guild (newest first, capped).
pub fn remember_members(known: &mut Vec<Person>, found: Vec<Person>) {
    for p in found.into_iter().rev() {
        known.retain(|k| k.id != p.id);
        known.insert(0, p);
    }
    known.truncate(MAX_MEMBERS);
}

/// The same data from an [`AccountState`]: members it holds, roles, text
/// channels the account can see, custom emoji, and the account's rights in `channel`.
pub fn from_account_state(
    state: &AccountState,
    guild: GuildId,
    channel: ChannelId,
) -> (Directory, Vec<Person>) {
    use inlet_protocol::permissions::{BYPASS_SLOWMODE, MENTION_EVERYONE};
    use inlet_state::channels::is_guild_text_based;
    let clean = crate::model::clean_name;
    let id = |s: &str| crate::backend::composer::snowflake_u64(s);
    let perms = state.permissions(guild, channel);
    let rights = Rights {
        mention_everyone: perms.has(MENTION_EVERYONE),
        bypass_slowmode: perms.has(BYPASS_SLOWMODE),
    };
    let Some(g) = state.guild(guild) else {
        return (Directory::default(), Vec::new());
    };
    let roles = g
        .roles
        .by_rank()
        .into_iter()
        .filter(|r| r.id != guild.to_string())
        .filter_map(|r| {
            Some(RoleEntry {
                id: id(&r.id)?,
                name: clean(&r.name),
                mentionable: r.mentionable,
            })
        })
        .collect();
    let channels = state
        .visible_channels(guild)
        .into_iter()
        .filter(|c| is_guild_text_based(c.kind))
        .filter_map(|c| {
            Some(ChannelEntry {
                id: id(&c.id)?,
                name: clean(c.name.as_deref().unwrap_or_default()),
            })
        })
        .collect();
    let emojis = g
        .emojis
        .iter()
        .filter(|e| {
            !e.name.is_empty()
                && e.name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .filter_map(|e| {
            Some(CustomEmoji {
                id: id(&e.id)?,
                name: e.name.clone(),
                animated: e.animated,
            })
        })
        .collect();
    let members = g
        .members
        .others()
        .filter_map(|(uid, m)| {
            let user = m.user.as_ref().or_else(|| state.user(uid))?;
            let name = m.display_name().unwrap_or_else(|| user.display_name());
            Some(Person {
                id: uid.get(),
                name: clean(name),
                username: clean(&user.username),
            })
        })
        .take(MAX_MEMBERS)
        .collect();
    (
        Directory {
            roles,
            channels,
            emojis,
            rights: std::iter::once((channel.to_string(), rights)).collect(),
        },
        members,
    )
}

impl ComposerView {
    /// The list above the composer box.
    pub(super) fn render_autocomplete(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.autocomplete.as_ref()?;
        if open.rows.is_empty() {
            return None;
        }
        let heading: SharedString = match open.section {
            Section::Mentions => t!("composer-ac-mentions"),
            Section::Channels => t!("composer-ac-channels"),
            Section::Emoji => t!("composer-ac-emoji"),
            Section::Commands => t!("composer-ac-commands"),
        }
        .into();
        let rows = open.rows.iter().enumerate().map(|(i, c)| {
            let selected = i == open.selected;
            div()
                .id(("ac-row", i))
                .px_2()
                .h(px(32.))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .rounded(px(4.))
                .cursor_pointer()
                .when(selected, |d| d.bg(theme::selected_bg()))
                .hover(|s| s.bg(theme::hover_bg()))
                .children(c.glyph.clone().map(|g| div().text_size(px(18.)).child(g)))
                .child(
                    div()
                        .text_color(theme::text_strong())
                        .font_weight(FontWeight::MEDIUM)
                        .child(c.title.clone()),
                )
                .children(c.detail.clone().map(|d| {
                    div()
                        .text_color(theme::text_tertiary())
                        .text_size(px(13.))
                        .overflow_hidden()
                        .child(d)
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.pick(i, window, cx);
                }))
        });
        Some(
            div()
                .mx(px(6.))
                .mb_1()
                .p_2()
                .rounded(px(8.))
                .bg(theme::panel_bg())
                .border_1()
                .border_color(theme::ring())
                .flex()
                .flex_col()
                .child(
                    div()
                        .px_2()
                        .pb_1()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_tertiary())
                        .child(heading),
                )
                .children(rows)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn people() -> Vec<Person> {
        vec![
            Person {
                id: 11,
                name: "Ada".into(),
                username: "ada".into(),
            },
            Person {
                id: 12,
                name: "Grace".into(),
                username: "hopper".into(),
            },
        ]
    }

    fn dir() -> Directory {
        Directory {
            roles: vec![
                RoleEntry {
                    id: 21,
                    name: "Admins".into(),
                    mentionable: true,
                },
                RoleEntry {
                    id: 22,
                    name: "Hidden".into(),
                    mentionable: false,
                },
            ],
            channels: vec![ChannelEntry {
                id: 31,
                name: "general".into(),
            }],
            emojis: vec![CustomEmoji {
                id: 41,
                name: "party_ada".into(),
                animated: true,
            }],
            rights: Default::default(),
        }
    }

    fn picked_ids(rows: &[Candidate]) -> Vec<Token> {
        rows.iter()
            .filter_map(|c| match &c.pick {
                Pick::Token(t, _) => Some(t.clone()),
                Pick::Text(_) => None,
            })
            .collect()
    }

    #[test]
    fn mention_autocomplete_never_invents_an_id() {
        let members = people();
        let d = dir();
        let src = Sources {
            directory: Some(&d),
            members: &members,
            in_guild: true,
            rights: Rights::default(),
        };
        let known = [
            Token::User(Id(11)),
            Token::User(Id(12)),
            Token::Role(Id(21)),
        ];
        // Names, numbers, wire tags and odd text: every row is someone the server knows.
        for q in [
            "",
            "a",
            "ad",
            "123",
            "11",
            "<@99>",
            "99",
            "@everyone",
            "x\u{202e}y",
        ] {
            let (_, rows) = candidates(&Trigger::Mention(q.into()), &src);
            for t in picked_ids(&rows) {
                assert!(known.contains(&t), "query {q:?} offered {t:?}");
            }
        }
        let (_, rows) = candidates(&Trigger::Mention("12345".into()), &src);
        assert!(rows.is_empty(), "a typed ID is not a member");
        let (_, rows) = candidates(&Trigger::Mention("ho".into()), &src);
        assert_eq!(
            picked_ids(&rows),
            [Token::User(Id(12))],
            "the account name counts"
        );
        // Without the right, no everyone pills; outside a guild, nothing.
        let (_, rows) = candidates(&Trigger::Mention("every".into()), &src);
        assert!(rows.is_empty());
        let dm = Sources {
            directory: None,
            members: &members,
            in_guild: false,
            rights: Rights::default(),
        };
        assert!(candidates(&Trigger::Mention("a".into()), &dm).1.is_empty());
    }

    #[test]
    fn everyone_is_offered_only_with_the_right() {
        let d = dir();
        let src = Sources {
            directory: Some(&d),
            members: &[],
            in_guild: true,
            rights: Rights {
                mention_everyone: true,
                bypass_slowmode: false,
            },
        };
        let (_, rows) = candidates(&Trigger::Mention("ev".into()), &src);
        assert_eq!(picked_ids(&rows), [Token::Everyone]);
        // With the right, roles nobody else may mention are offered too.
        let (_, rows) = candidates(&Trigger::Mention("hid".into()), &src);
        assert_eq!(picked_ids(&rows), [Token::Role(Id(22))]);
        let (_, rows) = candidates(&Trigger::Mention(String::new()), &src);
        assert!(picked_ids(&rows).contains(&Token::Here));
    }

    #[test]
    fn channels_emoji_and_commands() {
        let d = dir();
        let src = Sources {
            directory: Some(&d),
            members: &[],
            in_guild: true,
            rights: Rights::default(),
        };
        let (_, rows) = candidates(&Trigger::Channel("gen".into()), &src);
        assert_eq!(picked_ids(&rows), [Token::Channel(Id(31))]);
        let (_, rows) = candidates(&Trigger::Emoji("party".into()), &src);
        assert!(
            matches!(&rows[0].pick, Pick::Token(Token::CustomEmoji { id: Id(41), .. }, l) if l == ":party_ada:")
        );
        assert!(rows[1..].iter().all(|r| matches!(r.pick, Pick::Text(_))));
        let (_, rows) = candidates(&Trigger::Emoji("smile".into()), &src);
        assert!(rows
            .iter()
            .any(|r| r.pick == Pick::Text("\u{1f604} ".into())));
        let (_, rows) = candidates(&Trigger::Command("ni".into()), &src);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pick, Pick::Text("/nick ".into()));
    }

    #[test]
    fn searches_add_members_without_duplicates() {
        let mut known = people();
        remember_members(
            &mut known,
            vec![Person {
                id: 12,
                name: "Grace H".into(),
                username: "hopper".into(),
            }],
        );
        assert_eq!(known.len(), 2);
        assert_eq!(known[0].name, "Grace H");
    }

    #[test]
    fn the_account_state_gives_the_same_kind_of_data() {
        use serde_json::json;
        let everyone = (1u64 << 10) | (1 << 11); // view, send
        let ready: inlet_protocol::Ready = serde_json::from_value(json!({
            "session_id": "s",
            "user": { "id": "10", "username": "me" },
            "guilds": [{
                "id": "100",
                "properties": { "id": "100", "name": "G", "owner_id": "1" },
                "roles": [
                    { "id": "100", "name": "@everyone", "position": 0,
                      "permissions": everyone.to_string() },
                    { "id": "7", "name": "Mods", "position": 1, "permissions": "0",
                      "mentionable": true },
                    { "id": "8", "name": "Hidden", "position": 2, "permissions": "0" }
                ],
                "channels": [
                    { "id": "301", "type": 0, "name": "general", "position": 0,
                      "permission_overwrites": [] },
                    { "id": "302", "type": 2, "name": "voice", "position": 1,
                      "permission_overwrites": [] }
                ],
                "members": [
                    { "user": { "id": "10", "username": "me" }, "roles": [] },
                    { "user": { "id": "20", "username": "ada" }, "nick": "Ada L", "roles": [] }
                ],
                "emojis": [ { "id": "41", "name": "wave", "animated": false } ]
            }]
        }))
        .unwrap();
        let state = AccountState::from_ready(&ready);
        let (d, members) = from_account_state(
            &state,
            GuildId::parse("100").unwrap(),
            ChannelId::parse("301").unwrap(),
        );
        assert_eq!(d.roles.iter().map(|r| r.id).collect::<Vec<_>>(), [8, 7]);
        assert!(d.channels.iter().any(|c| c.id == 301));
        assert_eq!(d.emojis.len(), 1);
        assert_eq!(d.rights("301"), Rights::default());
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "Ada L");
    }
}
