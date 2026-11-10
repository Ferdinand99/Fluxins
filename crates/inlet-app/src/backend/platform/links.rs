// SPDX-License-Identifier: AGPL-3.0-or-later
//! Links from outside the window: `inlet://` deep links (from the browser, another app,
//! a later start, macOS) and invite links clicked in a message.
//!
//! A link never does anything by itself. [`decide`] turns every link into a
//! [`Prompt`]: the window shows what the link points at and waits for a click. A channel
//! link opens the channel after "Open"; an invite opens the invite page after "Open
//! invite"; a link to a channel this account cannot see, or an invite on another
//! instance, says so first. The only actions are navigation and showing an invite.
//!
//! [`LinkGate`] sits in front: the same link again within 1.5 s is dropped (Fluxer does
//! the same), and at most one link per second is shown, so a page that fires links in
//! a loop cannot make the window flicker or swap the prompt under the cursor.

use std::time::{Duration, Instant};

use inlet_platform::deep_link::{self, Deduper};
use inlet_platform::DeepLink;

/// At most one link per this long is shown.
pub const MIN_GAP: Duration = Duration::from_secs(1);

/// A link to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming {
    /// An `inlet://` link. It names no instance: it is read against the signed-in one.
    Deep(DeepLink),
    /// `https://<host>/invite/<code>`, clicked in a message.
    WebInvite { origin: String, code: String },
}

/// Recognises an invite link (`https://<host>/invite/<code>`, nothing more). The code
/// follows the same rules as in `inlet://invite/<code>`.
pub fn web_invite(url: &str) -> Option<Incoming> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let host = parsed.host_str()?;
    let code = parsed.path().strip_prefix("/invite/")?;
    // Reuses the deep link parser's checks on the code.
    match deep_link::parse(&format!("inlet://invite/{code}"))? {
        DeepLink::InvitePreview { code } => {
            let origin = match parsed.port() {
                Some(port) => format!("https://{host}:{port}"),
                None => format!("https://{host}"),
            };
            Some(Incoming::WebInvite { origin, code })
        }
        DeepLink::Channel { .. } => None,
    }
}

/// What the window asks before following a link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    /// A channel this account can see. "Open" goes there.
    OpenChannel {
        guild: Option<String>,
        channel: String,
        message: Option<String>,
        /// How the window names it, for example `#general` and the server's name.
        channel_name: String,
        guild_name: Option<String>,
    },
    /// A channel this account cannot see (or that is not loaded). Nothing to open.
    UnknownChannel,
    /// An invite. "Open invite" shows its page in the browser; joining is a step there.
    Invite {
        url: String,
        /// The host the invite is on, as shown.
        host: String,
        /// Whether the invite is on the signed-in instance. When it is not, the prompt
        /// says so plainly: the invite belongs to a server Inlet is not signed in to.
        known_instance: bool,
    },
    /// Not signed in: there is nowhere to open a link yet.
    SignInFirst,
}

/// What [`decide`] needs to know about the signed-in account.
pub trait Lookup {
    /// The signed-in instance's origin (`https://chat.example`), if signed in.
    fn instance(&self) -> Option<&str>;
    /// The channel's name and its guild's name, if this account has that channel in
    /// that guild (`guild` `None`: a direct message).
    fn channel(&self, guild: Option<u64>, channel: u64) -> Option<(String, Option<String>)>;
}

/// The host of an origin, for display and comparison; the text as it is when it is not
/// a URL.
fn host_of(origin: &str) -> String {
    url::Url::parse(origin)
        .ok()
        .and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_string(),
            })
        })
        .unwrap_or_else(|| origin.to_string())
}

/// What to ask before following `link`. There is no answer that acts at once.
// Deviation: Fluxer opens a channel link at once and shows invites in the app. Inlet
// asks first for every link that comes from outside the window, and opens invites in
// the browser until it has an invite view of its own.
pub fn decide(link: &Incoming, lookup: &dyn Lookup) -> Prompt {
    let Some(instance) = lookup.instance() else {
        return Prompt::SignInFirst;
    };
    let instance = instance.trim_end_matches('/');
    match link {
        Incoming::Deep(DeepLink::Channel {
            guild,
            channel,
            message,
        }) => match lookup.channel(*guild, *channel) {
            Some((channel_name, guild_name)) => Prompt::OpenChannel {
                guild: guild.map(|g| g.to_string()),
                channel: channel.to_string(),
                message: message.map(|m| m.to_string()),
                channel_name,
                guild_name,
            },
            None => Prompt::UnknownChannel,
        },
        Incoming::Deep(DeepLink::InvitePreview { code }) => Prompt::Invite {
            url: format!("{instance}/invite/{code}"),
            host: host_of(instance),
            known_instance: true,
        },
        Incoming::WebInvite { origin, code } => {
            let host = host_of(origin);
            Prompt::Invite {
                url: format!("{origin}/invite/{code}"),
                known_instance: !host.is_empty() && host == host_of(instance),
                host,
            }
        }
    }
}

/// Drops repeated links and keeps to one link per second.
#[derive(Debug, Default)]
pub struct LinkGate {
    dedupe: Deduper,
    last: Option<Instant>,
}

impl LinkGate {
    /// Whether to show this link now.
    pub fn admit(&mut self, link: &DeepLink, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < MIN_GAP)
        {
            log::info!("a deep link came within a second of the last one; dropped");
            return false;
        }
        if !self.dedupe.accept(link, now) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Account {
        instance: Option<&'static str>,
    }

    impl Lookup for Account {
        fn instance(&self) -> Option<&str> {
            self.instance
        }
        fn channel(&self, guild: Option<u64>, channel: u64) -> Option<(String, Option<String>)> {
            match (guild, channel) {
                (Some(1), 2) => Some(("#general".into(), Some("Server".into()))),
                (None, 7) => Some(("@friend".into(), None)),
                _ => None,
            }
        }
    }

    const SIGNED_IN: Account = Account {
        instance: Some("https://chat.example"),
    };

    fn deep(url: &str) -> Incoming {
        Incoming::Deep(deep_link::parse(url).unwrap())
    }

    #[test]
    fn deep_link_to_unknown_instance_asks_first() {
        // An invite on an instance Inlet is not signed in to: asked, and marked as such.
        let link = web_invite("https://other.example/invite/abc123").unwrap();
        assert_eq!(
            decide(&link, &SIGNED_IN),
            Prompt::Invite {
                url: "https://other.example/invite/abc123".into(),
                host: "other.example".into(),
                known_instance: false,
            }
        );
        // An invite on the signed-in instance is asked too.
        let link = web_invite("https://chat.example/invite/abc123").unwrap();
        assert!(matches!(
            decide(&link, &SIGNED_IN),
            Prompt::Invite {
                known_instance: true,
                ..
            }
        ));
        assert_eq!(
            decide(&deep("inlet://invite/abc123"), &SIGNED_IN),
            Prompt::Invite {
                url: "https://chat.example/invite/abc123".into(),
                host: "chat.example".into(),
                known_instance: true,
            }
        );
        // A channel this account does not have: nothing to open.
        assert_eq!(
            decide(&deep("inlet://channels/9/9"), &SIGNED_IN),
            Prompt::UnknownChannel
        );
        assert_eq!(
            decide(&deep("inlet://channels/@me/2"), &SIGNED_IN),
            Prompt::UnknownChannel
        );
        // A known channel still asks; "Open" is the only way there.
        assert_eq!(
            decide(&deep("inlet://channels/1/2/3"), &SIGNED_IN),
            Prompt::OpenChannel {
                guild: Some("1".into()),
                channel: "2".into(),
                message: Some("3".into()),
                channel_name: "#general".into(),
                guild_name: Some("Server".into()),
            }
        );
        assert!(matches!(
            decide(&deep("inlet://channels/@me/7"), &SIGNED_IN),
            Prompt::OpenChannel { guild: None, .. }
        ));
        // Signed out: nothing at all.
        let signed_out = Account { instance: None };
        assert_eq!(
            decide(&deep("inlet://channels/1/2"), &signed_out),
            Prompt::SignInFirst
        );
        assert_eq!(decide(&link, &signed_out), Prompt::SignInFirst);
    }

    #[test]
    fn only_plain_https_invite_links_are_recognised() {
        assert_eq!(
            web_invite("https://chat.example:8443/invite/Ab-9"),
            Some(Incoming::WebInvite {
                origin: "https://chat.example:8443".into(),
                code: "Ab-9".into()
            })
        );
        for bad in [
            "http://chat.example/invite/abc",
            "https://user@chat.example/invite/abc",
            "https://chat.example/invite/abc?join=1",
            "https://chat.example/invite/abc#x",
            "https://chat.example/invite/",
            "https://chat.example/invite/a/b",
            "https://chat.example/invites/abc",
            "https://chat.example/channels/1/2",
            "https://chat.example/invite/%41bc",
            "inlet://invite/abc",
        ] {
            assert_eq!(web_invite(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_gate_drops_repeats_and_keeps_to_one_link_a_second() {
        let mut gate = LinkGate::default();
        let a = deep_link::parse("inlet://channels/1/2").unwrap();
        let b = deep_link::parse("inlet://channels/1/3").unwrap();
        let t = Instant::now();
        assert!(gate.admit(&a, t));
        // Another link within the second: dropped.
        assert!(!gate.admit(&b, t + Duration::from_millis(500)));
        // The same link after a second but within 1.5 s: still a repeat.
        assert!(!gate.admit(&a, t + Duration::from_millis(1200)));
        assert!(gate.admit(&b, t + Duration::from_millis(1300)));
        // A flood: at most one per second gets through.
        let mut shown = 0;
        for i in 0..100u64 {
            let link = deep_link::parse(&format!("inlet://channels/1/{}", 100 + i)).unwrap();
            if gate.admit(&link, t + Duration::from_millis(2400 + i * 50)) {
                shown += 1;
            }
        }
        assert!(shown <= 6, "{shown}");
    }
}
