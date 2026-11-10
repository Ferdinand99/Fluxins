//! Media proxy URLs (avatars, icons, banners, emoji, stickers, attachments).
//!
//! Every URL is built from the session's checked `endpoints.media` base
//! (`https`, or loopback in tests), never from message content. IDs must be
//! snowflakes and hashes plain `[A-Za-z0-9_-]`, so nothing from the server can
//! change the host or path. URLs that come inside messages (attachment and
//! embed `proxy_url`) are only accepted when they already point at the media
//! base; then this module adds size or format parameters.
//!
//! An `a_` hash is animated. Ask for `animated: true` to get the animated
//! variant; otherwise the static frame is requested, as Fluxer's client does.

use url::Url;

/// The size ladder of the media proxy; requests snap up to the next rung.
pub const SIZE_LADDER: [u32; 33] = [
    16, 20, 22, 24, 28, 32, 40, 44, 48, 56, 60, 64, 80, 96, 100, 128, 160, 240, 256, 300, 320, 480,
    512, 600, 640, 1024, 1280, 1536, 2048, 3072, 4096, 8192, 16384,
];

/// Snaps a size to the ladder (values above the top become the top).
pub fn snap_size(size: u32) -> u32 {
    SIZE_LADDER
        .iter()
        .copied()
        .find(|s| *s >= size)
        .unwrap_or(16384)
}

/// Output format of an image asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageFormat {
    /// WebP (default; animated WebP for animated assets).
    #[default]
    Webp,
    /// PNG (static).
    Png,
    /// GIF (for animated assets in decoders without animated WebP).
    Gif,
    /// JPEG.
    Jpeg,
}

impl ImageFormat {
    fn ext(self) -> &'static str {
        match self {
            ImageFormat::Webp => "webp",
            ImageFormat::Png => "png",
            ImageFormat::Gif => "gif",
            ImageFormat::Jpeg => "jpg",
        }
    }
}

/// Options for an image asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageOptions {
    /// Square edge in pixels, snapped to [`SIZE_LADDER`].
    pub size: u32,
    /// Request the animated variant when the asset is animated.
    pub animated: bool,
    /// Output format.
    pub format: ImageFormat,
}

impl Default for ImageOptions {
    fn default() -> Self {
        ImageOptions {
            size: 128,
            animated: false,
            format: ImageFormat::Webp,
        }
    }
}

impl ImageOptions {
    /// A static image of `size` pixels.
    pub fn size(size: u32) -> Self {
        ImageOptions {
            size,
            ..Default::default()
        }
    }

    /// The animated variant (when the asset is animated).
    pub fn animated(mut self) -> Self {
        self.animated = true;
        self
    }

    /// Another output format.
    pub fn format(mut self, format: ImageFormat) -> Self {
        self.format = format;
        self
    }
}

/// Resizing for an attachment or external media URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Transform {
    /// Output width (1-16384).
    pub width: Option<u32>,
    /// Output height (1-16384).
    pub height: Option<u32>,
    /// Output format (an image format gives a video's first frame).
    pub format: Option<ImageFormat>,
    /// Animated output.
    pub animated: bool,
}

/// Builds media proxy URLs for one instance. Get it from [`crate::Instance::media`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    base: Url,
}

pub(crate) fn is_snowflake(id: &str) -> bool {
    !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())
}

pub(crate) fn is_hash(hash: &str) -> bool {
    !hash.is_empty()
        && hash.len() <= 128
        && hash
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Media {
    /// A builder for a checked base URL. `None` unless it is `https` (or
    /// `http` on loopback), has a host, and has no query, fragment or credentials.
    pub fn new(base: &str) -> Option<Media> {
        let url = crate::instance::require_scheme(base, "https", "http", "media").ok()?;
        if url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        Some(Media { base: url })
    }

    /// The base URL without a trailing slash.
    pub fn base(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    fn join(&self, segments: &[&str], query: &[(&str, String)]) -> Option<String> {
        let mut url = self.base.clone();
        {
            let mut path = url.path_segments_mut().ok()?;
            path.pop_if_empty();
            for s in segments {
                path.push(s);
            }
        }
        if !query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        Some(url.into())
    }

    /// `{kind}/{owner}/{hash}.{ext}` assets with `a_` handling.
    fn asset(&self, prefix: &[&str], hash: &str, opts: ImageOptions) -> Option<String> {
        if !is_hash(hash) {
            return None;
        }
        let animated_asset = hash.starts_with("a_");
        let animate = animated_asset && opts.animated;
        let stored = if animated_asset && !animate {
            &hash[2..]
        } else {
            hash
        };
        if stored.is_empty() {
            return None;
        }
        let file = format!("{stored}.{}", opts.format.ext());
        let mut segments: Vec<&str> = prefix.to_vec();
        segments.push(&file);
        let mut query = vec![("size", snap_size(opts.size).to_string())];
        if animate {
            query.push(("animated", "true".into()));
        }
        self.join(&segments, &query)
    }

    /// A user avatar, or `None` when the user has none (show a default avatar).
    pub fn avatar(&self, user_id: &str, hash: Option<&str>, opts: ImageOptions) -> Option<String> {
        is_snowflake(user_id).then_some(())?;
        self.asset(&["avatars", user_id], hash?, opts)
    }

    /// A member's guild avatar.
    pub fn member_avatar(
        &self,
        guild_id: &str,
        user_id: &str,
        hash: Option<&str>,
        opts: ImageOptions,
    ) -> Option<String> {
        (is_snowflake(guild_id) && is_snowflake(user_id)).then_some(())?;
        self.asset(
            &["guilds", guild_id, "users", user_id, "avatars"],
            hash?,
            opts,
        )
    }

    /// A member's guild banner.
    pub fn member_banner(
        &self,
        guild_id: &str,
        user_id: &str,
        hash: Option<&str>,
        opts: ImageOptions,
    ) -> Option<String> {
        (is_snowflake(guild_id) && is_snowflake(user_id)).then_some(())?;
        self.asset(
            &["guilds", guild_id, "users", user_id, "banners"],
            hash?,
            opts,
        )
    }

    /// A guild icon.
    pub fn guild_icon(
        &self,
        guild_id: &str,
        hash: Option<&str>,
        opts: ImageOptions,
    ) -> Option<String> {
        is_snowflake(guild_id).then_some(())?;
        self.asset(&["icons", guild_id], hash?, opts)
    }

    /// A user or guild banner.
    pub fn banner(&self, owner_id: &str, hash: Option<&str>, opts: ImageOptions) -> Option<String> {
        is_snowflake(owner_id).then_some(())?;
        self.asset(&["banners", owner_id], hash?, opts)
    }

    /// A guild invite splash.
    pub fn splash(&self, guild_id: &str, hash: Option<&str>, opts: ImageOptions) -> Option<String> {
        is_snowflake(guild_id).then_some(())?;
        self.asset(&["splashes", guild_id], hash?, opts)
    }

    /// A role icon (never animated).
    pub fn role_icon(&self, role_id: &str, hash: Option<&str>, size: u32) -> Option<String> {
        is_snowflake(role_id).then_some(())?;
        self.asset(&["role-icons", role_id], hash?, ImageOptions::size(size))
    }

    /// A custom emoji. `animated` asks for the animated variant of an animated emoji.
    pub fn emoji(&self, emoji_id: &str, animated: bool, size: u32) -> Option<String> {
        is_snowflake(emoji_id).then_some(())?;
        let file = format!("{emoji_id}.webp");
        let mut query = vec![("size", snap_size(size).to_string())];
        if animated {
            query.push(("animated", "true".into()));
        }
        self.join(&["emojis", &file], &query)
    }

    /// A sticker (always WebP).
    pub fn sticker(&self, sticker_id: &str, animated: bool, size: u32) -> Option<String> {
        is_snowflake(sticker_id).then_some(())?;
        let file = format!("{sticker_id}.webp");
        let mut query = vec![("size", snap_size(size).to_string())];
        if animated {
            query.push(("animated", "true".into()));
        }
        self.join(&["stickers", &file], &query)
    }

    /// Checks that `url` is on this media base under `/{kind}/`, and adds the
    /// transform. The signed query (`ex`, `is`, `hm`) is kept as it is.
    fn on_base(&self, url: &str, kind: &str, t: Transform) -> Option<String> {
        let mut parsed = Url::parse(url.trim()).ok()?;
        if parsed.origin() != self.base.origin()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return None;
        }
        let prefix = format!("{}/{kind}/", self.base.path().trim_end_matches('/'));
        if !parsed.path().starts_with(&prefix) || parsed.path().contains("/../") {
            return None;
        }
        parsed.set_fragment(None);
        let keep: Vec<(String, String)> = parsed
            .query_pairs()
            .filter(|(k, _)| !matches!(k.as_ref(), "width" | "height" | "format" | "animated"))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        parsed.set_query(None);
        {
            let mut q = parsed.query_pairs_mut();
            for (k, v) in &keep {
                q.append_pair(k, v);
            }
            if let Some(w) = t.width {
                q.append_pair("width", &w.clamp(1, 16384).to_string());
            }
            if let Some(h) = t.height {
                q.append_pair("height", &h.clamp(1, 16384).to_string());
            }
            if let Some(f) = t.format {
                q.append_pair("format", f.ext());
            }
            if t.animated {
                q.append_pair("animated", "true");
            }
        }
        let mut s: String = parsed.into();
        if s.ends_with('?') {
            s.pop();
        }
        Some(s)
    }

    /// An attachment URL (`url` or `proxy_url` of an attachment) through this
    /// media proxy, with an optional resize. `None` when the URL is not an
    /// attachment URL of this instance.
    pub fn attachment(&self, url: &str, t: Transform) -> Option<String> {
        self.on_base(url, "attachments", t)
    }

    /// The best URL to load an attachment from, checked against the media base.
    pub fn attachment_of(&self, a: &crate::models::Attachment, t: Transform) -> Option<String> {
        a.proxy_url
            .as_deref()
            .and_then(|u| self.attachment(u, t))
            .or_else(|| a.url.as_deref().and_then(|u| self.attachment(u, t)))
    }

    /// A signed external media URL (`proxy_url` of embed media, `proxy_icon_url`)
    /// with an optional resize. `None` unless it is on this media base under `/external/`.
    pub fn external(&self, proxy_url: &str, t: Transform) -> Option<String> {
        self.on_base(proxy_url, "external", t)
    }
}

impl crate::Instance {
    /// The media URL builder, or `None` when the instance has no usable
    /// `https` media endpoint.
    pub fn media(&self) -> Option<Media> {
        Media::new(self.inner.media.as_deref()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Attachment;
    use serde_json::json;

    fn media() -> Media {
        Media::new("https://media.example.org").unwrap()
    }

    #[test]
    fn assets_with_sizes_and_animation() {
        let m = media();
        assert_eq!(
            m.avatar("5", Some("abc"), ImageOptions::size(64))
                .as_deref(),
            Some("https://media.example.org/avatars/5/abc.webp?size=64")
        );
        assert_eq!(
            m.avatar("5", Some("a_abc"), ImageOptions::size(100))
                .as_deref(),
            Some("https://media.example.org/avatars/5/abc.webp?size=100")
        );
        assert_eq!(
            m.avatar("5", Some("a_abc"), ImageOptions::size(65).animated())
                .as_deref(),
            Some("https://media.example.org/avatars/5/a_abc.webp?size=80&animated=true")
        );
        // A static asset ignores the animated wish.
        assert_eq!(
            m.guild_icon(
                "7",
                Some("def"),
                ImageOptions::size(5000).animated().format(ImageFormat::Png)
            )
            .as_deref(),
            Some("https://media.example.org/icons/7/def.png?size=8192")
        );
        assert_eq!(
            m.member_avatar("1", "2", Some("h"), ImageOptions::default())
                .as_deref(),
            Some("https://media.example.org/guilds/1/users/2/avatars/h.webp?size=128")
        );
        assert_eq!(
            m.banner("3", Some("b"), ImageOptions::size(480)).as_deref(),
            Some("https://media.example.org/banners/3/b.webp?size=480")
        );
        assert_eq!(
            m.emoji("55", true, 48).as_deref(),
            Some("https://media.example.org/emojis/55.webp?size=48&animated=true")
        );
        assert_eq!(
            m.sticker("66", false, 160).as_deref(),
            Some("https://media.example.org/stickers/66.webp?size=160")
        );
        assert_eq!(
            m.role_icon("9", Some("r"), 16).as_deref(),
            Some("https://media.example.org/role-icons/9/r.webp?size=16")
        );
        assert_eq!(m.avatar("5", None, ImageOptions::default()), None);
    }

    #[test]
    fn hostile_ids_and_hashes_are_refused() {
        let m = media();
        for (id, hash) in [
            ("5/../../x", "abc"),
            ("5", "../../evil"),
            ("5", "abc?x=1"),
            ("5", "a_"),
            ("", "abc"),
            ("5", "abc.png#"),
            ("12345678901234567890123", "abc"),
            ("5", "https://evil.example/x"),
        ] {
            assert_eq!(
                m.avatar(id, Some(hash), ImageOptions::default()),
                None,
                "{id} {hash}"
            );
        }
        assert_eq!(m.emoji("1:2", false, 32), None);
        assert!(Media::new("http://media.example.org").is_none());
        assert!(Media::new("https://user:pw@media.example.org").is_none());
        assert!(Media::new("https://media.example.org/?a=1").is_none());
        assert!(Media::new("javascript:alert(1)").is_none());
    }

    #[test]
    fn attachment_urls_must_be_on_the_media_base() {
        let m = Media::new("https://chat.example.org/media/").unwrap();
        let signed =
            "https://chat.example.org/media/attachments/1/2/cat.png?ex=69fba4b5&is=69fa5335&hm=abc";
        assert_eq!(
            m.attachment(signed, Transform { width: Some(400), height: Some(300), ..Default::default() })
                .as_deref(),
            Some("https://chat.example.org/media/attachments/1/2/cat.png?ex=69fba4b5&is=69fa5335&hm=abc&width=400&height=300")
        );
        assert_eq!(
            m.attachment(signed, Transform::default()).as_deref(),
            Some(signed)
        );
        for bad in [
            "https://evil.example/media/attachments/1/2/x.png",
            "http://chat.example.org/media/attachments/1/2/x.png",
            "https://chat.example.org/other/attachments/1/2/x.png",
            "https://chat.example.org/media/avatars/1/x.png",
            "https://chat.example.org:8443/media/attachments/1/2/x.png",
            "https://u:p@chat.example.org/media/attachments/1/2/x.png",
            "not a url",
        ] {
            assert_eq!(m.attachment(bad, Transform::default()), None, "{bad}");
        }
        assert!(m
            .external(
                "https://chat.example.org/media/external/sig/https/x.example/a.png",
                Transform::default()
            )
            .is_some());
        assert!(m.external(signed, Transform::default()).is_none());

        let a: Attachment = serde_json::from_value(json!({
            "id": "2", "filename": "x", "size": 1, "proxy_url": "https://evil.example/x",
            "url": "https://chat.example.org/media/attachments/1/2/x.png"
        }))
        .unwrap();
        assert_eq!(
            m.attachment_of(&a, Transform::default()).as_deref(),
            Some("https://chat.example.org/media/attachments/1/2/x.png")
        );
    }
}
