//! Typed snowflake IDs.
//!
//! The protocol crate keeps IDs as decimal strings. The state keeps them as
//! numbers: they are `Copy`, compare numerically (newer messages have larger
//! IDs) and take 8 bytes. An ID that is not a decimal number is not a Fluxer
//! snowflake; events that carry one are ignored.

use std::fmt;

use inlet_protocol::models::FLUXER_EPOCH_MS;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u64);

        impl $name {
            /// Parses a decimal snowflake; anything else is `None`.
            pub fn parse(s: &str) -> Option<Self> {
                let s = s.trim();
                if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                s.parse().ok().map($name)
            }

            /// Parses an optional snowflake.
            pub fn parse_opt(s: Option<&str>) -> Option<Self> {
                s.and_then(Self::parse)
            }

            /// The raw number.
            pub fn get(self) -> u64 {
                self.0
            }

            /// Creation time in Unix milliseconds.
            pub fn timestamp_ms(self) -> i64 {
                ((self.0 >> 22) + FLUXER_EPOCH_MS) as i64
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<u64> for $name {
            fn from(v: u64) -> Self {
                $name(v)
            }
        }
    };
}

id_type!(
    /// A guild ID (also the ID of its everyone role).
    GuildId
);
id_type!(
    /// A channel ID.
    ChannelId
);
id_type!(
    /// A user ID.
    UserId
);
id_type!(
    /// A role ID.
    RoleId
);
id_type!(
    /// A message ID. Larger is newer.
    MessageId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_decimal_snowflakes() {
        assert_eq!(ChannelId::parse("123"), Some(ChannelId(123)));
        assert_eq!(ChannelId::parse(" 7 "), Some(ChannelId(7)));
        assert_eq!(ChannelId::parse(""), None);
        assert_eq!(ChannelId::parse("-1"), None);
        assert_eq!(ChannelId::parse("+1"), None);
        assert_eq!(ChannelId::parse("@me"), None);
        assert_eq!(ChannelId::parse("99999999999999999999999"), None);
        assert_eq!(
            MessageId(1 << 22).timestamp_ms(),
            FLUXER_EPOCH_MS as i64 + 1
        );
        assert_eq!(GuildId(5).to_string(), "5");
    }
}
