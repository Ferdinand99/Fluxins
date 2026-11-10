// SPDX-License-Identifier: AGPL-3.0-or-later
//! Client commands, as in Fluxer (`CommandUtils.ts`, `useCommands.ts`): `/shrug`,
//! `/tableflip` and `/unflip` insert their text when picked from the list; `/me` and
//! `/spoiler` wrap the message; `/nick` and `/msg` call the server; `/saved` sends a saved
//! media item. Commands are read from the wire text, so `/msg` gets the user from a pill.
//! Anything else that starts with `/` is sent as it is, like Fluxer does.

/// What picking a command does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Replaces the command with this text.
    Simple(&'static str),
    /// Leaves `/name ` for the arguments.
    Action,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub kind: Kind,
    /// The catalogue key of the one-line description.
    pub description: &'static str,
    /// Works only in a guild.
    pub guild_only: bool,
}

pub const COMMANDS: &[Command] = &[
    Command {
        name: "/shrug",
        kind: Kind::Simple("\u{af}\\_(\u{30c4})_/\u{af}"),
        description: "composer-cmd-shrug",
        guild_only: false,
    },
    Command {
        name: "/tableflip",
        kind: Kind::Simple(
            "(\u{256f}\u{b0}\u{25a1}\u{b0})\u{256f}\u{fe35} \u{253b}\u{2501}\u{253b}",
        ),
        description: "composer-cmd-tableflip",
        guild_only: false,
    },
    Command {
        name: "/unflip",
        kind: Kind::Simple("\u{252c}\u{2500}\u{252c} \u{30ce}( \u{309c}-\u{309c}\u{30ce})"),
        description: "composer-cmd-unflip",
        guild_only: false,
    },
    Command {
        name: "/me",
        kind: Kind::Action,
        description: "composer-cmd-me",
        guild_only: false,
    },
    Command {
        name: "/spoiler",
        kind: Kind::Action,
        description: "composer-cmd-spoiler",
        guild_only: false,
    },
    Command {
        name: "/nick",
        kind: Kind::Action,
        description: "composer-cmd-nick",
        guild_only: true,
    },
    Command {
        name: "/msg",
        kind: Kind::Action,
        description: "composer-cmd-msg",
        guild_only: false,
    },
    Command {
        name: "/saved",
        kind: Kind::Action,
        description: "composer-cmd-saved",
        guild_only: false,
    },
];

/// Commands whose name contains `query` (without the slash), as Fluxer filters them.
pub fn matching(query: &str, in_guild: bool) -> Vec<&'static Command> {
    let q = query.to_lowercase();
    COMMANDS
        .iter()
        .filter(|c| in_guild || !c.guild_only)
        .filter(|c| c.name[1..].contains(&q))
        .collect()
}

/// The text that replaces `/query` when a command is picked.
pub fn insertion(command: &Command) -> String {
    match command.kind {
        Kind::Simple(text) => format!("{text} "),
        Kind::Action => format!("{} ", command.name),
    }
}

/// What the message turns out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    /// `/nick`, with the new nickname or `None` to remove it.
    Nick(Option<String>),
    /// `/msg <@id> text`.
    Msg { user_id: String, message: String },
    /// Send this text instead (`/me`, `/spoiler`).
    Wrapped(String),
    /// `/saved`: needs a pick from the saved media list.
    Saved,
    /// Not a command: send as it is.
    Text,
}

/// Reads a command from the wire text of the message.
pub fn parse(wire: &str) -> Parsed {
    let t = wire.trim();
    if let Some(rest) = after_command(t, "/nick") {
        let nick = rest.trim();
        return Parsed::Nick((!nick.is_empty()).then(|| nick.to_owned()));
    }
    if let Some(rest) = after_command(t, "/msg") {
        let rest = rest.trim();
        if let Some((user_id, len)) = user_mention_at_start(rest) {
            let message = rest[len..].trim();
            if !message.is_empty() {
                return Parsed::Msg {
                    user_id,
                    message: message.to_owned(),
                };
            }
        }
        return Parsed::Text;
    }
    for (name, open, close) in [("/me", "_", "_"), ("/spoiler", "||", "||")] {
        if let Some(rest) = after_command(t, name) {
            let content = rest.trim();
            if content.is_empty() {
                return Parsed::Text;
            }
            return Parsed::Wrapped(format!("{open}{content}{close}"));
        }
    }
    if after_command(t, "/saved").is_some() {
        return Parsed::Saved;
    }
    Parsed::Text
}

/// The text after a command name: the rest when the name is followed by white space or
/// nothing, `None` when the message is not this command.
fn after_command<'a>(t: &'a str, name: &str) -> Option<&'a str> {
    let rest = t.strip_prefix(name)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(rest)
}

/// `<@123>` or `<@!123>` at the start: the user ID and the length of the tag.
fn user_mention_at_start(s: &str) -> Option<(String, usize)> {
    let rest = s.strip_prefix("<@")?;
    let rest = rest.strip_prefix('!').unwrap_or(rest);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || !rest[digits.len()..].starts_with('>') {
        return None;
    }
    let len = s.len() - rest.len() + digits.len() + 1;
    Some((digits, len))
}

/// The nickname of a `/nick` from the text the person sees (no escapes, pills as their
/// labels). `None` removes the nickname.
pub fn nick_from_display(text: &str) -> Option<String> {
    let rest = after_command(text.trim(), "/nick")?.trim();
    (!rest.is_empty()).then(|| rest.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_read_from_the_wire_text() {
        assert_eq!(parse("/nick"), Parsed::Nick(None));
        assert_eq!(parse("  /nick  Ada  "), Parsed::Nick(Some("Ada".into())));
        assert_eq!(
            parse("/msg <@12> hello <@34>"),
            Parsed::Msg {
                user_id: "12".into(),
                message: "hello <@34>".into()
            }
        );
        // A typed name is not a user: the server never sees an invented ID.
        assert_eq!(parse("/msg \\@ada hello"), Parsed::Text);
        assert_eq!(parse("/msg <@12>"), Parsed::Text);
        assert_eq!(parse("/me waves"), Parsed::Wrapped("_waves_".into()));
        assert_eq!(
            parse("/spoiler it ends"),
            Parsed::Wrapped("||it ends||".into())
        );
        assert_eq!(parse("/me "), Parsed::Text);
        assert_eq!(parse("/saved cats"), Parsed::Saved);
        assert_eq!(
            parse("/shrug"),
            Parsed::Text,
            "simple commands are inserted, not sent"
        );
        assert_eq!(parse("hello /me"), Parsed::Text);
    }

    #[test]
    fn the_list_filters_like_fluxer() {
        let names = |q, g| {
            matching(q, g)
                .into_iter()
                .map(|c| c.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("fl", true), ["/tableflip", "/unflip"]);
        assert!(names("", false).iter().all(|n| *n != "/nick"));
        assert!(names("", true).contains(&"/nick"));
        assert_eq!(insertion(&COMMANDS[0]), "\u{af}\\_(\u{30c4})_/\u{af} ");
        assert_eq!(insertion(&COMMANDS[3]), "/me ");
    }

    #[test]
    fn the_nickname_is_the_text_shown() {
        assert_eq!(nick_from_display("/nick o/ x"), Some("o/ x".into()));
        assert_eq!(nick_from_display("/nick @ada"), Some("@ada".into()));
        assert_eq!(nick_from_display("  /nick  "), None);
        assert_eq!(nick_from_display("/nickname"), None);
    }
}
