//! System notifications.
//!
//! - Windows: this crate shows the toast itself, from its own thread
//!   ([`notify`]). gpui's toast call pumps window messages while the app is
//!   borrowed, which makes gpui drop frames ("RefCell already borrowed"); a toast
//!   from another thread never touches the UI thread.
//! - macOS and Linux: through gpui (`App::show_system_notification`), see
//!   `gpui_glue::notify`. macOS needs an app bundle. On Linux the body is escaped
//!   first when the notification server reads markup.
//!
//! Every notification is silent: the Windows toast XML says `<audio silent="true"/>`,
//! and gpui sets no sound on macOS and sends no sound hint on Linux (where a server that
//! plays its own sound per app can still do so). The app plays its own sound, after
//! [`crate::quiet_mode`] says the system is not in a quiet mode.
//!
//! This module builds what is shown: text from the server is cleaned and capped
//! here, and when the user has turned previews off the message content never
//! enters the notification at all.

use unicode_segmentation::UnicodeSegmentation as _;

use crate::{Error, Result};

/// Longest title, in grapheme clusters.
pub const TITLE_MAX_GRAPHEMES: usize = 80;
/// Longest body, in grapheme clusters.
pub const BODY_MAX_GRAPHEMES: usize = 240;
/// Text is cut to this many bytes before it is looked at at all.
const INPUT_MAX_BYTES: usize = 4096;
/// Longest tag, in bytes.
const TAG_MAX: usize = 64;

/// Whether the message content may be shown (the user's "show message preview"
/// setting).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preview {
    /// Show the content.
    Show,
    /// Never show the content; the body is a fixed text such as "New message".
    Hide,
}

/// A notification ready to be shown. The fields are private so that every
/// notification has passed through the cleaning in the constructors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    tag: String,
    title: String,
    body: String,
}

impl Notification {
    /// A notification with free text. `tag` identifies it (on Windows and macOS a
    /// newer notification with the same tag replaces the older one; Linux shows
    /// both) and comes back in
    /// [`crate::PlatformEvent::NotificationClicked`]. It must be 1 to 64 characters of
    /// `A-Z a-z 0-9 . _ : -`.
    pub fn new(tag: &str, title: &str, body: &str) -> Result<Self> {
        check_tag(tag)?;
        Ok(Self {
            tag: tag.to_string(),
            title: clean_text(title, TITLE_MAX_GRAPHEMES),
            body: clean_text(body, BODY_MAX_GRAPHEMES),
        })
    }

    /// A notification for a message. With [`Preview::Hide`] the content is not used
    /// at all and `hidden_text` (already translated, for example "New message")
    /// becomes the body.
    pub fn message(
        tag: &str,
        title: &str,
        content: &str,
        preview: Preview,
        hidden_text: &str,
    ) -> Result<Self> {
        let body = match preview {
            Preview::Show => content,
            Preview::Hide => hidden_text,
        };
        Self::new(tag, title, body)
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

fn check_tag(tag: &str) -> Result<()> {
    let ok = !tag.is_empty()
        && tag.len() <= TAG_MAX
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidInput("notification tag"))
    }
}

/// Characters that change how text around them is displayed without being visible
/// themselves: bidi overrides and isolates (which can make a name read backwards),
/// zero-width spaces, word joiners, the BOM and soft hyphens. ZWJ (U+200D) stays,
/// because emoji sequences need it.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{200B}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            // Fillers and joiners that render as nothing, popular for blank names.
            | '\u{034F}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{180E}'
            | '\u{3164}'
            | '\u{FFA0}'
    )
}

/// Escapes text for a notification server that reads markup in the body
/// (`body-markup`): `&`, `<`, `>`, `"` and `'` become entities, so a message can
/// never turn into a link or formatting.
pub fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Shows a notification, without a sound (see the module docs). Windows only: the
/// toast is queued for this crate's own
/// notification thread and this returns at once, so it is safe to call from
/// anywhere, also while gpui's app is borrowed, and never makes the caller wait.
/// The queue holds 64; beyond that the newest is dropped and logged. Errors from
/// showing the toast are logged on that thread. A click arrives as [`crate::PlatformEvent::NotificationClicked`].
/// Elsewhere this returns [`Error::Unsupported`]; use `gpui_glue::notify`, which
/// picks the right path on every system.
pub fn notify(notification: &Notification) -> Result<()> {
    #[cfg(windows)]
    {
        crate::toast::show(notification)
    }
    #[cfg(not(windows))]
    {
        let _ = notification;
        Err(Error::Unsupported(
            "notifications go through gpui here: use gpui_glue::notify",
        ))
    }
}

/// Removes a notification shown by [`notify`] (Windows only; elsewhere a no-op).
pub fn dismiss(tag: &str) -> Result<()> {
    #[cfg(windows)]
    {
        crate::toast::dismiss(tag)
    }
    #[cfg(not(windows))]
    {
        let _ = tag;
        Ok(())
    }
}

/// Makes server text safe to hand to the OS: cuts the input to 4 KiB, turns line
/// breaks and tabs into spaces, drops control and invisible formatting characters,
/// collapses runs of white space, and caps the result at `max` grapheme clusters
/// with an ellipsis. Never splits a grapheme cluster (an emoji sequence, a letter
/// with its accents).
pub fn clean_text(text: &str, max: usize) -> String {
    let mut end = text.len().min(INPUT_MAX_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut cleaned = String::with_capacity(end);
    let mut pending_space = false;
    for c in text[..end].chars() {
        if c.is_whitespace() {
            pending_space = !cleaned.is_empty();
        } else if c.is_control() || is_invisible_format(c) {
            continue;
        } else {
            if pending_space {
                cleaned.push(' ');
                pending_space = false;
            }
            cleaned.push(c);
        }
    }
    if max == 0 {
        return String::new();
    }
    let mut graphemes = cleaned.grapheme_indices(true);
    match graphemes.nth(max - 1).and_then(|_| graphemes.next()) {
        // More than `max` clusters: keep `max - 1` and add the ellipsis.
        Some(_) => {
            let cut = cleaned
                .grapheme_indices(true)
                .nth(max - 1)
                .map_or(cleaned.len(), |(i, _)| i);
            let mut capped = cleaned[..cut].trim_end().to_string();
            capped.push('\u{2026}');
            capped
        }
        None => cleaned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_text_is_cleaned_and_capped() {
        // Line breaks, tabs and runs of spaces collapse; ends are trimmed.
        let n = Notification::new("a:1", "  Alice \n in #general ", "hi\n\nthere\t you  ").unwrap();
        assert_eq!(n.title(), "Alice in #general");
        assert_eq!(n.body(), "hi there you");

        // Control characters, bidi overrides and zero-width characters go.
        let n = Notification::new(
            "a:1",
            "Ev\u{202E}lin\u{0007}\u{200B}",
            "x\u{2066}y\u{2069}\u{0}z",
        )
        .unwrap();
        assert_eq!(n.title(), "Evlin");
        assert_eq!(n.body(), "xyz");

        // Capped at the grapheme limit with an ellipsis.
        let long = "a".repeat(1000);
        let n = Notification::new("a:1", &long, &long).unwrap();
        assert_eq!(n.title().chars().count(), TITLE_MAX_GRAPHEMES);
        assert!(n.title().ends_with('\u{2026}'));
        assert_eq!(n.body().chars().count(), BODY_MAX_GRAPHEMES);

        // Exactly at the limit: untouched.
        let exact = "b".repeat(TITLE_MAX_GRAPHEMES);
        assert_eq!(Notification::new("a:1", &exact, "").unwrap().title(), exact);

        // Emoji sequences are never split, and ZWJ survives.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let text = family.repeat(TITLE_MAX_GRAPHEMES + 5);
        let title = clean_text(&text, TITLE_MAX_GRAPHEMES);
        assert_eq!(title.graphemes(true).count(), TITLE_MAX_GRAPHEMES);
        assert!(title.starts_with(family));
        assert_eq!(
            title
                .trim_end_matches('\u{2026}')
                .graphemes(true)
                .next_back(),
            Some(family)
        );

        // Huge input is cut before it is processed, on a char boundary.
        let huge = "\u{00E5}".repeat(100_000);
        assert_eq!(
            clean_text(&huge, 10_000).chars().count(),
            INPUT_MAX_BYTES / 2
        );
        assert_eq!(clean_text("abc", 0), "");
    }

    #[test]
    fn hidden_previews_never_carry_the_content() {
        let secret = "the launch code is 1234";
        let n =
            Notification::message("m:1", "Alice", secret, Preview::Hide, "New message").unwrap();
        assert_eq!(n.body(), "New message");
        assert!(!format!("{n:?}").contains("1234"));
        let n =
            Notification::message("m:1", "Alice", secret, Preview::Show, "New message").unwrap();
        assert_eq!(n.body(), secret);
    }

    #[test]
    fn blank_name_fillers_are_removed() {
        let n = Notification::new(
            "a:1",
            "\u{3164}\u{115F}\u{1160}\u{FFA0}\u{034F}\u{180E}",
            "a\u{3164}b",
        )
        .unwrap();
        assert_eq!(n.title(), "");
        assert_eq!(n.body(), "ab");
        // Tag characters stay: flag emoji such as England need them.
        let england = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        assert_eq!(clean_text(england, 10), england);
    }

    #[test]
    fn markup_is_escaped_for_markup_servers() {
        let body = r#"<a href="https://evil.example">Log in</a> & a < b 'q'"#;
        assert_eq!(
            escape_markup(body),
            "&lt;a href=&quot;https://evil.example&quot;&gt;Log in&lt;/a&gt; &amp; a &lt; b &apos;q&apos;"
        );
        assert_eq!(escape_markup("plain"), "plain");
    }

    #[test]
    fn tags_are_checked() {
        assert!(Notification::new("acc-1:chan_2.x", "t", "b").is_ok());
        for bad in ["", "a b", "a/b", "æ", &"x".repeat(65)] {
            assert!(Notification::new(bad, "t", "b").is_err(), "{bad:?}");
        }
    }
}
