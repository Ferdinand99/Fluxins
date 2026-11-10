// SPDX-License-Identifier: AGPL-3.0-or-later
//! Mention pills (`<@user>`, `<@&role>`, `<#channel>`, `@everyone`, `@here`, commands).
//!
//! Parsing stores the ID and the name the backend knew (`fallback`); drawing looks the
//! name up again in the account state ([`MentionNames`], built per message when it is
//! drawn), so a renamed member or channel shows its new name without parsing again.
//! As in Fluxer's `MentionRenderer`: a user shows their guild nickname or display name,
//! a role its name in its own colour, a channel `#name` and opens it on click, a
//! category plain `#name`, and anything unknown a pill with a generic label.

use std::collections::HashMap;
use std::rc::Rc;

use fluxer_markdown_parser::ast::{GuildNavigationType, MentionKind};
use gpui::{App, Hsla};
use inlet_state::{AccountState, ChannelId, GuildId, RoleId, UserId};

use super::blocks::lookup;
use super::inline::{push_live, push_marked, Paint};
use super::{timestamp, Block, Inline, LiveKind, Style, CHANNEL_LINK};
use crate::app::Inlet;
use crate::model::{self, Message};
use crate::theme;
use inlet_protocol::models::channel_type;

/// The names of the mentions in one message, from the account state, worked out when
/// it is drawn.
#[derive(Clone, Debug, Default)]
pub struct MentionNames {
    pub users: HashMap<String, String>,
    pub roles: HashMap<String, RoleName>,
    pub channels: HashMap<String, ChannelName>,
}

#[derive(Clone, Debug)]
pub struct RoleName {
    pub name: String,
    /// `0xRRGGBB`, when the role has a colour.
    pub color: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct ChannelName {
    pub name: String,
    /// A channel one can open (a pill); a category shows as plain text.
    pub pill: bool,
}

/// A live piece as it is drawn now.
pub(super) struct Shown {
    pub text: String,
    pub paint: Option<Paint>,
    /// A click opens this (a channel).
    pub link: Option<String>,
}

/// Names never wrap inside a pill (`white-space: nowrap` in Fluxer).
fn nowrap(name: &str) -> String {
    name.replace(' ', "\u{a0}")
}

fn user_label(name: Option<&str>) -> String {
    match name {
        Some(name) => format!("@{}", nowrap(name)),
        None => format!("@{}", t!("markdown-unknown-user")),
    }
}

fn role_label(name: Option<&str>) -> String {
    match name {
        Some(name) => format!("@{}", nowrap(name)),
        None => format!("@{}", t!("markdown-unknown-role")),
    }
}

fn channel_label(name: Option<&str>) -> String {
    match name {
        Some(name) => format!("#{}", nowrap(name)),
        None => t!("markdown-unknown-channel"),
    }
}

/// Parsing: appends a mention to `inline`.
pub(super) fn push(kind: &MentionKind, inline: &mut Inline) {
    match kind {
        MentionKind::User { id } => {
            let fallback = lookup(|n| n.users.get(id).cloned());
            let label = user_label(fallback.as_deref());
            push_live(
                inline,
                &label,
                LiveKind::User {
                    id: id.clone(),
                    fallback,
                },
            );
        }
        MentionKind::Role { id } => {
            let fallback = lookup(|n| n.roles.get(id).cloned());
            let label = role_label(fallback.as_deref());
            push_live(
                inline,
                &label,
                LiveKind::Role {
                    id: id.clone(),
                    fallback,
                },
            );
        }
        MentionKind::Channel { id } => {
            let fallback = lookup(|n| n.channels.get(id).cloned());
            let label = channel_label(fallback.as_deref());
            push_live(
                inline,
                &label,
                LiveKind::Channel {
                    id: id.clone(),
                    fallback,
                },
            );
        }
        MentionKind::Command {
            name,
            subcommand_group,
            subcommand,
            ..
        } => {
            let mut label = format!("/{name}");
            for part in [subcommand_group, subcommand].into_iter().flatten() {
                label.push(' ');
                label.push_str(part);
            }
            push_marked(inline, &label, Style::Mention);
        }
        MentionKind::GuildNavigation {
            navigation_type,
            id,
        } => {
            let label = match (navigation_type, id) {
                (GuildNavigationType::Customize, _) => t!("markdown-nav-customize"),
                (GuildNavigationType::Browse, _) => t!("markdown-nav-browse"),
                (GuildNavigationType::Guide, _) => t!("markdown-nav-guide"),
                (GuildNavigationType::LinkedRoles, Some(id)) => {
                    t!("markdown-nav-linked-roles-id", id = id.as_str())
                }
                (GuildNavigationType::LinkedRoles, None) => t!("markdown-nav-linked-roles"),
            };
            push_marked(inline, &label, Style::Mention);
        }
        MentionKind::Everyone => push_marked(inline, &t!("markdown-everyone"), Style::Mention),
        MentionKind::Here => push_marked(inline, &t!("markdown-here"), Style::Mention),
    }
}

/// The neutral pill of a mention nobody can resolve: muted, and nothing to click.
fn neutral(text: String) -> Shown {
    Shown {
        text,
        paint: Some(Paint::Pill {
            text: theme::mention_neutral_text(),
            fill: theme::mention_neutral_fill(),
        }),
        link: None,
    }
}

fn pill(text: String) -> Shown {
    Shown {
        text,
        paint: Some(Paint::Style(Style::Mention)),
        link: None,
    }
}

fn rgb_with_alpha(color: u32, a: f32) -> Hsla {
    let mut c: Hsla = gpui::rgb(color).into();
    c.a = a;
    c
}

/// Drawing: the label and look of a live piece now.
pub(super) fn resolve(kind: &LiveKind, names: &MentionNames, now_s: i64) -> Shown {
    match kind {
        LiveKind::User { id, fallback } => {
            match names
                .users
                .get(id)
                .map(String::as_str)
                .or(fallback.as_deref())
            {
                Some(name) => pill(user_label(Some(name))),
                // Deviation: Fluxer shows the raw user ID here; Inlet shows a generic
                // label in a neutral pill.
                None => neutral(user_label(None)),
            }
        }
        LiveKind::Role { id, fallback } => match names.roles.get(id) {
            Some(role) => match role.color {
                Some(color) => Shown {
                    text: role_label(Some(&role.name)),
                    paint: Some(Paint::Pill {
                        text: rgb_with_alpha(color, 1.),
                        fill: rgb_with_alpha(color, 0.1),
                    }),
                    link: None,
                },
                None => pill(role_label(Some(&role.name))),
            },
            None => match fallback {
                Some(name) => pill(role_label(Some(name))),
                None => neutral(role_label(None)),
            },
        },
        LiveKind::Channel { id, fallback } => {
            let link = Some(format!("{CHANNEL_LINK}{id}"));
            match names.channels.get(id) {
                Some(ChannelName { name, pill: true }) => Shown {
                    link,
                    ..pill(channel_label(Some(name)))
                },
                Some(ChannelName { name, pill: false }) => Shown {
                    text: channel_label(Some(name)),
                    paint: None,
                    link: None,
                },
                None => match fallback {
                    Some(name) => Shown {
                        link,
                        ..pill(channel_label(Some(name)))
                    },
                    None => neutral(channel_label(None)),
                },
            }
        }
        LiveKind::Timestamp { unix, style } => Shown {
            text: timestamp::label(*unix, *style, now_s),
            paint: Some(Paint::Style(Style::Timestamp)),
            link: None,
        },
    }
}

/// Calls `f` for every inline in `blocks`, nested ones too.
pub(super) fn each_inline(blocks: &[Block], f: &mut impl FnMut(&Inline)) {
    for b in blocks {
        match b {
            Block::Text(i) | Block::Heading(_, i) | Block::Subtext(i) => f(i),
            Block::Code(_) => {}
            Block::Quote(children) | Block::Alert(_, children) | Block::Spoiler(children) => {
                each_inline(children, f)
            }
            Block::List(list) => list.items.iter().for_each(|item| each_inline(item, f)),
            Block::Table(table) => {
                table.header.iter().for_each(&mut *f);
                table.rows.iter().flatten().for_each(&mut *f);
            }
        }
    }
}

impl MentionNames {
    /// The names of the mentions in `lists`, looked up in `state` for a message in
    /// `channel`. Only mentions that occur are looked up.
    pub fn for_blocks<'a>(
        lists: impl IntoIterator<Item = &'a [Block]>,
        state: &AccountState,
        channel: Option<ChannelId>,
    ) -> MentionNames {
        let guild = channel.and_then(|c| state.channels().guild_of(c));
        let mut out = MentionNames::default();
        for blocks in lists {
            each_inline(blocks, &mut |inline| {
                for live in &inline.live {
                    out.add(&live.kind, state, guild);
                }
            });
        }
        out
    }

    fn add(&mut self, kind: &LiveKind, state: &AccountState, guild: Option<GuildId>) {
        match kind {
            LiveKind::User { id, .. } => {
                if self.users.contains_key(id) {
                    return;
                }
                let Some(user) = UserId::parse(id) else {
                    return;
                };
                let member = guild
                    .and_then(|g| state.member(g, user))
                    .and_then(|m| m.display_name());
                let name = member.or_else(|| state.user(user).map(|u| u.display_name()));
                if let Some(name) = name {
                    self.users.insert(id.clone(), model::clean_name(name));
                }
            }
            LiveKind::Role { id, .. } => {
                if self.roles.contains_key(id) {
                    return;
                }
                let role = guild
                    .and_then(|g| state.guild(g))
                    .zip(RoleId::parse(id))
                    .and_then(|(g, r)| g.roles.get(r));
                if let Some(role) = role {
                    let color = role.color & 0xFF_FF_FF;
                    self.roles.insert(
                        id.clone(),
                        RoleName {
                            name: model::clean_name(&role.name),
                            color: (color != 0).then_some(color as u32),
                        },
                    );
                }
            }
            LiveKind::Channel { id, .. } => {
                if self.channels.contains_key(id) {
                    return;
                }
                let Some(channel) = ChannelId::parse(id).and_then(|c| state.channel(c)) else {
                    return;
                };
                let pill = match channel.kind {
                    channel_type::GUILD_CATEGORY => false,
                    // Direct messages are never mentioned by name: unknown, as in Fluxer.
                    channel_type::DM | channel_type::GROUP_DM | channel_type::PERSONAL_NOTES => {
                        return
                    }
                    _ => true,
                };
                if let Some(name) = channel.name.as_deref().filter(|n| !n.is_empty()) {
                    self.channels.insert(
                        id.clone(),
                        ChannelName {
                            name: model::clean_name(name),
                            pill,
                        },
                    );
                }
            }
            LiveKind::Timestamp { .. } => {}
        }
    }
}

impl Inlet {
    /// Mention names for drawing message `m` (its text and embeds).
    pub(crate) fn mention_names(&self, m: &Message, cx: &App) -> Rc<MentionNames> {
        let Some(state) = self.account.as_ref().map(|a| a.read(cx)) else {
            return Rc::default();
        };
        let lists = std::iter::once(m.blocks.as_slice()).chain(m.embeds.iter().flat_map(|e| {
            std::iter::once(e.description.as_slice())
                .chain(e.fields.iter().map(|f| f.value.as_slice()))
        }));
        Rc::new(MentionNames::for_blocks(
            lists,
            state,
            ChannelId::parse(&m.channel_id),
        ))
    }

    /// Mention names for blocks shown in the open channel (messages on their way).
    pub(crate) fn mention_names_in_open_channel(
        &self,
        blocks: &[Block],
        cx: &App,
    ) -> Rc<MentionNames> {
        let Some(state) = self.account.as_ref().map(|a| a.read(cx)) else {
            return Rc::default();
        };
        let channel = self.selected_channel.as_deref().and_then(ChannelId::parse);
        Rc::new(MentionNames::for_blocks([blocks], state, channel))
    }
}

#[cfg(test)]
mod tests {
    use super::super::inline::compose;
    use super::super::{parse, Block};
    use super::*;

    fn inline(blocks: &[Block]) -> &Inline {
        match &blocks[0] {
            Block::Text(i) => i,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_mention_shows_a_neutral_pill() {
        let blocks = parse("<@123> <@&456> <#789> <@42>");
        let mut names = MentionNames::default();
        names.users.insert("42".into(), "ada".into());
        let c = compose(inline(&blocks), &names, 0);
        assert_eq!(c.text, "@unknown-user @unknown-role #unknown-channel @ada");
        let neutral = Paint::Pill {
            text: theme::mention_neutral_text(),
            fill: theme::mention_neutral_fill(),
        };
        let paint_of = |label: &str| {
            let start = c.text.find(label).unwrap();
            c.paints
                .iter()
                .find(|(r, _)| r.start == start && r.end == start + label.len())
                .map(|(_, p)| *p)
        };
        // Unknown user, role and channel: the same neutral pill, never the ID, and
        // nothing to click.
        for label in ["@unknown-user", "@unknown-role", "#unknown-channel"] {
            assert_eq!(paint_of(label), Some(neutral), "{label}");
        }
        assert!(!c.text.contains("123") && !c.text.contains("456"));
        assert!(c.links.is_empty());
        // A known user is a normal pill.
        assert_eq!(paint_of("@ada"), Some(Paint::Style(Style::Mention)));
    }

    #[test]
    fn role_pills_take_the_role_colour_and_categories_are_plain() {
        let blocks = parse("<@&7> <#8>");
        let mut names = MentionNames::default();
        names.roles.insert(
            "7".into(),
            RoleName {
                name: "Moderators".into(),
                color: Some(0xE6_7E_22),
            },
        );
        names.channels.insert(
            "8".into(),
            ChannelName {
                name: "info".into(),
                pill: false,
            },
        );
        let c = compose(inline(&blocks), &names, 0);
        assert_eq!(c.text, "@Moderators #info");
        assert!(matches!(c.paints[0].1, Paint::Pill { .. }));
        assert_eq!(c.paints.len(), 1, "the category is plain text");
        assert!(c.links.is_empty());
    }

    #[test]
    fn everyone_here_and_commands_are_pills() {
        let blocks = parse("@everyone @here </deploy run:123>");
        let i = inline(&blocks);
        assert!(i.text.starts_with("@everyone @here"), "{:?}", i.text);
        assert_eq!(
            i.highlights
                .iter()
                .filter(|(_, s)| *s == Style::Mention)
                .count(),
            3,
            "{i:?}"
        );
        assert!(i.text.contains("/deploy run"), "{:?}", i.text);
    }
}
