//! `inlet://` deep links.
//!
//! Inlet has its own scheme and never registers or accepts `fluxer://`. A link only
//! names a place in the app; it can never carry a token or an action. The parser is
//! strict: only the routes below, only ASCII, no query, fragment, user info, port,
//! percent-encoding or empty segments. Anything else is rejected, not repaired.
//!
//! | Link | Meaning |
//! |---|---|
//! | `inlet://channels/@me/<channel>` | open a DM or group DM |
//! | `inlet://channels/<guild>/<channel>` | open a guild channel |
//! | `inlet://channels/<guild or @me>/<channel>/<message>` | jump to a message |
//! | `inlet://invite/<code>` | show the invite preview; joining needs a click |
//!
//! Like Fluxer, a link may not name an instance: it always resolves against the
//! account in the foreground, so a link cannot point the app at another server.

use std::time::{Duration, Instant};

use crate::{Error, Result};

/// Inlet's URL scheme.
pub const SCHEME: &str = "inlet";

/// Longest accepted link, in bytes.
pub const MAX_LEN: usize = 256;

/// Fluxer drops a repeated link within this window (`DUPLICATE_URL_SUPPRESS_MS`);
/// so does Inlet, see [`Deduper`].
pub const DUPLICATE_WINDOW: Duration = Duration::from_millis(1500);

/// A validated deep link. Every variant is navigation only.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DeepLink {
    /// A channel, or a message in it. `guild` is `None` for DMs (`@me`).
    Channel {
        guild: Option<u64>,
        channel: u64,
        message: Option<u64>,
    },
    /// The preview of an invite. Joining is a separate, confirmed step in the UI.
    InvitePreview { code: String },
}

impl DeepLink {
    /// The canonical form of the link, for example `inlet://channels/@me/42`.
    pub fn to_url(&self) -> String {
        match self {
            DeepLink::Channel {
                guild,
                channel,
                message,
            } => {
                let guild = guild.map_or_else(|| "@me".to_string(), |g| g.to_string());
                match message {
                    Some(m) => format!("{SCHEME}://channels/{guild}/{channel}/{m}"),
                    None => format!("{SCHEME}://channels/{guild}/{channel}"),
                }
            }
            DeepLink::InvitePreview { code } => format!("{SCHEME}://invite/{code}"),
        }
    }
}

/// Parses one `inlet://` link. Returns `None` for anything that is not exactly one
/// of the known routes.
pub fn parse(arg: &str) -> Option<DeepLink> {
    if arg.len() > MAX_LEN || !arg.is_ascii() {
        return None;
    }
    // Only printable ASCII, no spaces; this also rules out control characters.
    if !arg.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let (scheme, rest) = arg.split_once("://")?;
    if !scheme.eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    // No query, fragment, user info, port, escapes or backslashes. `@` is only
    // allowed as the `@me` segment, checked below.
    if rest.contains(['?', '#', '%', '\\', ':', ';', '"', '\'', '<', '>', '|']) {
        return None;
    }
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let segments: Vec<&str> = rest.split('/').collect();
    if segments
        .iter()
        .any(|s| s.is_empty() || *s == "." || *s == "..")
    {
        return None;
    }
    match segments.as_slice() {
        ["channels", guild, channel] => channel_link(guild, channel, None),
        ["channels", guild, channel, message] => channel_link(guild, channel, Some(message)),
        ["invite", code] if valid_invite_code(code) => Some(DeepLink::InvitePreview {
            code: (*code).to_string(),
        }),
        _ => None,
    }
}

fn channel_link(guild: &str, channel: &str, message: Option<&str>) -> Option<DeepLink> {
    let guild = match guild {
        "@me" => None,
        g => Some(snowflake(g)?),
    };
    let channel = snowflake(channel)?;
    let message = match message {
        Some(m) => Some(snowflake(m)?),
        None => None,
    };
    Some(DeepLink::Channel {
        guild,
        channel,
        message,
    })
}

/// A Fluxer id: decimal digits, no sign, no leading zero, not zero, fits in a u64.
fn snowflake(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 20 || s.starts_with('0') || !s.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    s.parse().ok()
}

fn valid_invite_code(code: &str) -> bool {
    (2..=32).contains(&code.len())
        && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !code.starts_with('-')
        && !code.ends_with('-')
}

/// Finds the deep link in a command line, split with [`crate::split_args`]: when
/// there is a `--`, only the values after it are looked at (that is how the
/// registered handlers pass the link); otherwise the arguments after `--url`, or
/// all of them. Only the first `inlet:` argument is considered; if it does not
/// parse, there is no link.
pub fn extract<S: AsRef<str>>(args: &[S]) -> Option<DeepLink> {
    let looks_like_link = |a: &&S| {
        a.as_ref()
            .get(..=SCHEME.len())
            .is_some_and(|head| head.eq_ignore_ascii_case("inlet:"))
    };
    let has_separator = args.iter().any(|a| a.as_ref() == "--");
    let (options, positional) = crate::split_args(args);
    let candidates: &[S] = if has_separator {
        positional
    } else {
        match options.iter().position(|a| a.as_ref() == "--url") {
            Some(flag) => &options[flag + 1..],
            None => options,
        }
    };
    candidates
        .iter()
        .find(looks_like_link)
        .and_then(|a| parse(a.as_ref()))
}

/// Drops the same link if it comes again within [`DUPLICATE_WINDOW`] (a click that
/// both starts a second instance and reaches the first, for example).
#[derive(Debug, Default)]
pub struct Deduper {
    last: Option<(DeepLink, Instant)>,
}

impl Deduper {
    /// Returns `false` if this link was accepted less than the window ago.
    pub fn accept(&mut self, link: &DeepLink, now: Instant) -> bool {
        if let Some((last, at)) = &self.last {
            if last == link && now.saturating_duration_since(*at) < DUPLICATE_WINDOW {
                return false;
            }
        }
        self.last = Some((link.clone(), now));
        true
    }
}

/// Only Inlet's own scheme (and `inlet-…` variants for tests and test builds) may
/// be registered. Never `fluxer`.
fn check_scheme(scheme: &str) -> Result<()> {
    let well_formed = !scheme.is_empty()
        && scheme.len() <= 32
        && scheme
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if well_formed && (scheme == SCHEME || scheme.starts_with("inlet-")) {
        Ok(())
    } else {
        Err(Error::InvalidInput(
            "only the inlet scheme can be registered",
        ))
    }
}

/// Makes this executable the handler of `scheme://` links for the current user.
///
/// - Windows: `HKCU\Software\Classes\<scheme>`, command `"<exe>" --url -- "%1"`.
/// - Linux: a hidden desktop entry with `MimeType=x-scheme-handler/<scheme>` in
///   `$XDG_DATA_HOME/applications`, made the default in `$XDG_CONFIG_HOME/mimeapps.list`.
/// - macOS: not at run time. The scheme is declared in the bundle's `Info.plist`
///   (`CFBundleURLTypes`) and links arrive through gpui's `on_open_urls`.
pub fn register(scheme: &str) -> Result<()> {
    check_scheme(scheme)?;
    let exe = std::env::current_exe()?;
    imp::register(scheme, &exe)
}

/// Removes what [`register`] wrote.
pub fn unregister(scheme: &str) -> Result<()> {
    check_scheme(scheme)?;
    imp::unregister(scheme)
}

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use crate::{Error, Result};

    fn key_path(scheme: &str) -> String {
        format!(r"Software\Classes\{scheme}")
    }

    pub(super) fn command(exe: &Path) -> Result<String> {
        let exe = exe
            .to_str()
            .ok_or(Error::InvalidInput("executable path is not valid Unicode"))?;
        if exe.contains('"') {
            return Err(Error::InvalidInput("executable path contains a quote"));
        }
        // `--` ends option parsing, so a link can never be read as a flag.
        Ok(format!("\"{exe}\" --url -- \"%1\""))
    }

    pub(super) fn register(scheme: &str, exe: &Path) -> Result<()> {
        let command = command(exe)?;
        let root = windows_registry::CURRENT_USER.create(key_path(scheme))?;
        root.set_string("", format!("URL:{} link", crate::APP_NAME))?;
        root.set_string("URL Protocol", "")?;
        let icon = format!("\"{}\",0", exe.display());
        root.create("DefaultIcon")?.set_string("", icon)?;
        root.create(r"shell\open\command")?
            .set_string("", command)?;
        Ok(())
    }

    pub(super) fn unregister(scheme: &str) -> Result<()> {
        match windows_registry::CURRENT_USER.remove_tree(key_path(scheme)) {
            Ok(()) => Ok(()),
            Err(err)
                if err.code() == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.to_hresult() =>
            {
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
    }

    /// The command registered for the scheme, if any. Used by the tests.
    #[cfg(test)]
    pub(super) fn registered_command(scheme: &str) -> Option<String> {
        windows_registry::CURRENT_USER
            .open(format!(r"{}\shell\open\command", key_path(scheme)))
            .ok()?
            .get_string("")
            .ok()
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::{Path, PathBuf};

    use crate::xdg;
    use crate::Result;

    fn entry_name(scheme: &str) -> String {
        format!("{}-{scheme}-handler.desktop", crate::APP_ID)
    }

    pub(super) fn register(scheme: &str, exe: &Path) -> Result<()> {
        register_in(&xdg::data_home()?, &xdg::config_home()?, scheme, exe)
    }

    pub(super) fn unregister(scheme: &str) -> Result<()> {
        unregister_in(&xdg::data_home()?, &xdg::config_home()?, scheme)
    }

    pub(super) fn register_in(
        data_home: &Path,
        config_home: &Path,
        scheme: &str,
        exe: &Path,
    ) -> Result<()> {
        let apps = data_home.join("applications");
        std::fs::create_dir_all(&apps)?;
        let exec = format!("{} --url -- %u", xdg::exec_arg(exe)?);
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName={name}\nNoDisplay=true\nExec={exec}\nMimeType=x-scheme-handler/{scheme};\n",
            name = crate::APP_NAME,
        );
        xdg::write_atomic(&apps.join(entry_name(scheme)), entry.as_bytes())?;
        set_default(config_home, scheme, Some(&entry_name(scheme)))
    }

    pub(super) fn unregister_in(data_home: &Path, config_home: &Path, scheme: &str) -> Result<()> {
        match std::fs::remove_file(data_home.join("applications").join(entry_name(scheme))) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
        set_default(config_home, scheme, None)
    }

    fn mimeapps(config_home: &Path) -> PathBuf {
        config_home.join("mimeapps.list")
    }

    /// Sets or removes `x-scheme-handler/<scheme>` under `[Default Applications]`,
    /// keeping every other line of the file as it was.
    fn set_default(config_home: &Path, scheme: &str, entry: Option<&str>) -> Result<()> {
        let path = mimeapps(config_home);
        let old = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err.into()),
        };
        let key = format!("x-scheme-handler/{scheme}=");
        let mut out = Vec::new();
        let mut section = String::new();
        let mut written = false;
        for line in old.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                if section == "[Default Applications]" && !written {
                    if let Some(entry) = entry {
                        out.push(format!("{key}{entry};"));
                    }
                    written = true;
                }
                section = trimmed.to_string();
            } else if section == "[Default Applications]" && trimmed.starts_with(&key) {
                continue;
            }
            out.push(line.to_string());
        }
        if !written {
            if let Some(entry) = entry {
                if section != "[Default Applications]" {
                    out.push("[Default Applications]".to_string());
                }
                out.push(format!("{key}{entry};"));
            }
        }
        let mut text = out.join("\n");
        text.push('\n');
        std::fs::create_dir_all(config_home)?;
        xdg::write_atomic(&path, text.as_bytes())
    }

    #[cfg(test)]
    pub(super) fn default_for(config_home: &Path, scheme: &str) -> Option<String> {
        let text = std::fs::read_to_string(mimeapps(config_home)).ok()?;
        let key = format!("x-scheme-handler/{scheme}=");
        text.lines().find_map(|l| {
            l.trim()
                .strip_prefix(&key)
                .map(|v| v.trim_end_matches(';').to_string())
        })
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use std::path::Path;

    use crate::{Error, Result};

    pub(super) fn register(_scheme: &str, _exe: &Path) -> Result<()> {
        Err(Error::Unsupported(
            "URL schemes are declared in the app bundle's Info.plist (CFBundleURLTypes)",
        ))
    }

    pub(super) fn unregister(_scheme: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(guild: Option<u64>, channel: u64, message: Option<u64>) -> DeepLink {
        DeepLink::Channel {
            guild,
            channel,
            message,
        }
    }

    #[test]
    fn deep_link_parse_accepts_the_known_routes() {
        assert_eq!(
            parse("inlet://channels/@me/42"),
            Some(channel(None, 42, None))
        );
        assert_eq!(
            parse("inlet://channels/1/2"),
            Some(channel(Some(1), 2, None))
        );
        assert_eq!(
            parse("inlet://channels/1/2/3"),
            Some(channel(Some(1), 2, Some(3)))
        );
        assert_eq!(
            parse("inlet://channels/@me/2/3/"),
            Some(channel(None, 2, Some(3)))
        );
        assert_eq!(
            parse("INLET://channels/1/2"),
            Some(channel(Some(1), 2, None))
        );
        assert_eq!(
            parse("inlet://invite/abc-DEF9"),
            Some(DeepLink::InvitePreview {
                code: "abc-DEF9".into()
            })
        );
        let max = format!("inlet://channels/{0}/{0}/{0}", u64::MAX);
        assert!(parse(&max).is_some());
    }

    #[test]
    fn deep_link_parse_rejects_other_schemes_and_odd_hosts() {
        let rejected = [
            // Other schemes, Fluxer's included.
            "fluxer://channels/@me/42",
            "https://channels/@me/42",
            "inlet-evil://channels/@me/42",
            "inlet:channels/@me/42",
            "inlet:/channels/@me/42",
            "inlet:///channels/@me/42",
            // Unknown or odd hosts and routes.
            "inlet://evil.example/channels/@me/42",
            "inlet://channels.evil/@me/42",
            "inlet://Channels/@me/42",
            "inlet://login",
            "inlet://handoff",
            "inlet://settings",
            "inlet://",
            "inlet://channels",
            "inlet://channels/@me",
            "inlet://channels/1/2/3/4",
            "inlet://invite",
            "inlet://invite/a",
            "inlet://invite/-abc",
            "inlet://invite/abc_def",
            "inlet://invite/abc/join",
            // User info, ports, queries, fragments, escapes, traversal.
            "inlet://user@channels/1/2",
            "inlet://channels:443/1/2",
            "inlet://channels/1/2?token=abc",
            "inlet://channels/1/2#token=abc",
            "inlet://invite/abc?instance=evil.example",
            "inlet://channels/%31/2",
            "inlet://channels/../1/2",
            "inlet://channels//1/2",
            "inlet://channels/1/./2",
            "inlet://channels\\1\\2",
            "inlet://channels/@you/2",
            "inlet://channels/@me@me/2",
            // Bad ids.
            "inlet://channels/0/2",
            "inlet://channels/01/2",
            "inlet://channels/-1/2",
            "inlet://channels/+1/2",
            "inlet://channels/1/2x",
            "inlet://channels/1/18446744073709551616",
            "inlet://channels/1/123456789012345678901",
            // Whitespace, control characters, non-ASCII.
            " inlet://channels/1/2",
            "inlet://channels/1/2 ",
            "inlet://channels/1/2\n",
            "inlet://channels/1/\u{0}2",
            "inlet://channels/1/２",
            "inlet://invite/abc\u{202e}",
        ];
        for link in rejected {
            assert_eq!(parse(link), None, "{link:?} should be rejected");
        }
        let long = format!("inlet://invite/{}", "a".repeat(MAX_LEN));
        assert_eq!(parse(&long), None);
    }

    #[test]
    fn to_url_round_trips() {
        for link in [
            channel(None, 42, None),
            channel(Some(7), 8, Some(9)),
            DeepLink::InvitePreview { code: "xyz".into() },
        ] {
            assert_eq!(parse(&link.to_url()), Some(link));
        }
    }

    #[test]
    fn extract_follows_the_url_flag_and_the_separator() {
        let args = ["--url", "--", "inlet://channels/@me/5"];
        assert_eq!(extract(&args), Some(channel(None, 5, None)));
        let args = ["--start-minimized", "inlet://invite/abc"];
        assert_eq!(
            extract(&args),
            Some(DeepLink::InvitePreview { code: "abc".into() })
        );
        // A bad first link is not skipped in favour of a later one.
        let args = ["inlet://channels/0/1", "inlet://channels/1/1"];
        assert_eq!(extract(&args), None);
        // With --url, links before the flag are ignored.
        let args = ["inlet://channels/1/1", "--url", "--", "not-a-link"];
        assert_eq!(extract(&args), None);
        assert_eq!(extract::<&str>(&[]), None);
        assert_eq!(extract(&["fluxer://channels/@me/5"]), None);
        // A link before the separator is an option, not the link.
        let args = ["inlet://channels/1/1", "--", "inlet://channels/2/2"];
        assert_eq!(extract(&args), Some(channel(Some(2), 2, None)));
    }

    /// A link that broke out of its quotes in `"exe" --url -- "%1"` cannot set
    /// flags: everything after `--` is data.
    #[test]
    fn a_link_that_breaks_out_of_its_quotes_sets_no_flags() {
        let args = [
            "--url",
            "--",
            "inlet://channels/1/2\"",
            "--autostart",
            "--start-minimized",
            "",
        ];
        assert_eq!(extract(&args), None, "the broken link does not parse");
        assert!(!crate::autostart::launched_at_login(&args));
        assert!(!crate::autostart::start_minimized(&args));
        let args = [
            "--url",
            "--",
            "inlet://channels/1/2",
            "--autostart",
            "--start-minimized",
        ];
        assert_eq!(extract(&args), Some(channel(Some(1), 2, None)));
        assert!(!crate::autostart::launched_at_login(&args));
        assert!(!crate::autostart::start_minimized(&args));
    }

    #[test]
    fn duplicates_inside_the_window_are_dropped() {
        let mut dedupe = Deduper::default();
        let t0 = Instant::now();
        let a = channel(None, 1, None);
        let b = channel(None, 2, None);
        assert!(dedupe.accept(&a, t0));
        assert!(!dedupe.accept(&a, t0 + Duration::from_millis(1000)));
        assert!(dedupe.accept(&b, t0 + Duration::from_millis(1100)));
        assert!(dedupe.accept(&a, t0 + Duration::from_millis(1200)));
        assert!(dedupe.accept(&a, t0 + Duration::from_millis(2800)));
    }

    #[test]
    fn only_the_inlet_scheme_can_be_registered() {
        assert!(check_scheme("inlet").is_ok());
        assert!(check_scheme("inlet-test-1").is_ok());
        for bad in ["fluxer", "Inlet", "inlet ", "http", "inlet:", "", "inletx"] {
            assert!(check_scheme(bad).is_err(), "{bad:?}");
        }
        assert!(register("fluxer").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_quotes_the_link_after_the_separator() {
        let cmd = imp::command(std::path::Path::new(r"C:\Program Files\Inlet\inlet.exe")).unwrap();
        assert_eq!(cmd, r#""C:\Program Files\Inlet\inlet.exe" --url -- "%1""#);
    }

    /// Writes to the real `HKCU\Software\Classes`, with a test scheme, and removes it.
    #[cfg(windows)]
    #[test]
    #[ignore = "touches the real registry; run in CI with --include-ignored"]
    fn windows_scheme_registration_round_trip() {
        let scheme = format!("inlet-test-{}", std::process::id());
        register(&scheme).unwrap();
        let cmd = imp::registered_command(&scheme).expect("command registered");
        assert!(cmd.ends_with(r#" --url -- "%1""#), "{cmd}");
        unregister(&scheme).unwrap();
        assert_eq!(imp::registered_command(&scheme), None);
        unregister(&scheme).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_scheme_registration_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (data, config) = (dir.path().join("data"), dir.path().join("config"));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("mimeapps.list"),
            "[Default Applications]\ntext/html=firefox.desktop\n\n[Added Associations]\nimage/png=eog.desktop;\n",
        )
        .unwrap();
        let exe = std::path::Path::new("/opt/In let/inlet");
        imp::register_in(&data, &config, "inlet", exe).unwrap();
        let entry = std::fs::read_to_string(
            data.join("applications")
                .join("no.opland.inlet-inlet-handler.desktop"),
        )
        .unwrap();
        assert!(
            entry.contains("Exec=\"/opt/In let/inlet\" --url -- %u\n"),
            "{entry}"
        );
        assert!(entry.contains("MimeType=x-scheme-handler/inlet;\n"));
        assert_eq!(
            imp::default_for(&config, "inlet").as_deref(),
            Some("no.opland.inlet-inlet-handler.desktop")
        );
        let list = std::fs::read_to_string(config.join("mimeapps.list")).unwrap();
        assert!(list.contains("text/html=firefox.desktop"));
        assert!(list.contains("image/png=eog.desktop;"));
        // Registering twice leaves one line.
        imp::register_in(&data, &config, "inlet", exe).unwrap();
        let list = std::fs::read_to_string(config.join("mimeapps.list")).unwrap();
        assert_eq!(list.matches("x-scheme-handler/inlet=").count(), 1, "{list}");

        imp::unregister_in(&data, &config, "inlet").unwrap();
        assert_eq!(imp::default_for(&config, "inlet"), None);
        assert!(!data
            .join("applications")
            .join("no.opland.inlet-inlet-handler.desktop")
            .exists());
        let list = std::fs::read_to_string(config.join("mimeapps.list")).unwrap();
        assert!(list.contains("text/html=firefox.desktop"));
    }
}
