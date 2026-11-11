//! GIF search through the instance's own GIF service (`/gifs/*` on the API).
//!
//! The instance talks to its provider (Klipy today); this client never
//! contacts the provider or any other third party. Results carry the
//! provider's direct media URLs, but [`Gif`] keeps only the media proxy
//! URLs (`proxy_src`), and [`Gif::preview`] returns one only when it is on the
//! instance's checked media endpoint. `url` is the provider page, for sending
//! as message text (the instance unfurls it); never load it.
//!
//! Budget: searches and suggestions of one account start at least
//! [`crate::coalesce::GIF_SEARCH_INTERVAL`] apart; one whose slot is more than
//! [`crate::coalesce::MAX_SEARCH_WAIT`] away is refused with
//! [`crate::Error::RateLimited`] without a request. Fluxer allows 40 per 10 s.
//! Debounce typing in the UI as well.
//!
//! Whether the instance has GIFs: [`crate::Discovery::gif_enabled`]; the
//! provider to credit: [`crate::Discovery::gif_provider`].

use std::collections::BTreeMap;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::media::{Media, Transform};
use crate::models::{lenient_vec, null_as_default, u64_or_zero};
use crate::rest::enc_query;
use crate::{Client, Discovery};

/// Format names to try for a still or animated preview, smallest image first
/// (the order Fluxer's docs give for clients without video).
pub const IMAGE_PREVIEW_ORDER: &[&str] = &[
    "tinywebp",
    "tinygif",
    "mediumwebp",
    "mediumgif",
    "webp",
    "gif",
    "nanowebp",
    "nanogif",
];

/// One format of a GIF. Only the media proxy URL is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GifFormat {
    /// Media proxy URL of this format.
    #[serde(default, deserialize_with = "null_as_default")]
    pub proxy_src: String,
    /// Width in pixels.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub width: u64,
    /// Height in pixels.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub height: u64,
}

/// One GIF from the instance's provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gif {
    /// Provider ID (required: an entry without one is skipped).
    #[serde(deserialize_with = "null_as_default")]
    pub id: String,
    /// Slug, for [`Client::register_gif_share`] and saving as a meme.
    #[serde(default, deserialize_with = "null_as_default")]
    pub slug: String,
    /// Provider name (`klipy`).
    #[serde(default, deserialize_with = "null_as_default")]
    pub provider: String,
    /// Title.
    #[serde(default, deserialize_with = "null_as_default")]
    pub title: String,
    /// Provider page URL: the text to send. Never load it.
    #[serde(default, deserialize_with = "null_as_default")]
    pub url: String,
    /// Media proxy URL of the default format.
    #[serde(default, deserialize_with = "null_as_default")]
    pub proxy_src: String,
    /// Width of the default format.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub width: u64,
    /// Height of the default format.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub height: u64,
    /// Every format by name (`tinygif`, `webm`, ...).
    #[serde(default, deserialize_with = "lenient_map")]
    pub media: BTreeMap<String, GifFormat>,
}

fn lenient_map<'de, D>(d: D) -> std::result::Result<BTreeMap<String, GifFormat>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Value::deserialize(d)? {
        Value::Object(m) => m
            .into_iter()
            .filter_map(|(k, v)| Some((k, serde_json::from_value(v).ok()?)))
            .collect(),
        _ => BTreeMap::new(),
    })
}

impl Gif {
    /// The first format in `order` that is on the media endpoint, through it.
    pub fn preview_in(&self, media: &Media, order: &[&str], t: Transform) -> Option<String> {
        order
            .iter()
            .filter_map(|name| self.media.get(*name))
            .find_map(|f| media.external(&f.proxy_src, t))
    }

    /// An image preview ([`IMAGE_PREVIEW_ORDER`]), else the default format;
    /// always through the instance's media endpoint, or `None`.
    pub fn preview(&self, media: &Media, t: Transform) -> Option<String> {
        self.preview_in(media, IMAGE_PREVIEW_ORDER, t)
            .or_else(|| media.external(&self.proxy_src, t))
    }
}

/// A suggested search term with a preview GIF.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GifCategory {
    /// The search term (send it to [`Client::gif_search`]).
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Media proxy URL of the preview.
    #[serde(default, deserialize_with = "null_as_default")]
    pub proxy_src: String,
    /// The preview GIF.
    #[serde(default, deserialize_with = "crate::models::lenient_opt")]
    pub gif: Option<Gif>,
}

impl GifCategory {
    /// The preview through the instance's media endpoint, or `None`.
    pub fn preview(&self, media: &Media, t: Transform) -> Option<String> {
        self.gif
            .as_ref()
            .and_then(|g| g.preview(media, t))
            .or_else(|| media.external(&self.proxy_src, t))
    }
}

/// `GET /gifs/featured`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GifFeatured {
    /// Featured GIFs (0 or 1).
    #[serde(default, deserialize_with = "lenient_vec")]
    pub gifs: Vec<Gif>,
    /// Categories (at most 50).
    #[serde(default, deserialize_with = "lenient_vec")]
    pub categories: Vec<GifCategory>,
}

/// The `gif` section of the discovery document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GifProvider {
    /// Machine name (`klipy`).
    #[serde(default, deserialize_with = "null_as_default")]
    pub provider: String,
    /// Name to show.
    #[serde(default, deserialize_with = "null_as_default")]
    pub display_name: String,
    /// The UI must show "Powered by …".
    #[serde(default, deserialize_with = "crate::models::lenient_bool")]
    pub attribution_required: bool,
}

impl Discovery {
    /// Whether the instance has a GIF picker (`features.gif_enabled`).
    pub fn gif_enabled(&self) -> bool {
        self.features.get("gif_enabled") == Some(&Value::Bool(true))
    }

    /// The GIF provider to credit.
    pub fn gif_provider(&self) -> Option<GifProvider> {
        crate::models::extra_field(&self.extra, "gif")
    }
}

/// The locales Fluxer accepts (`LocaleSchema`, a closed list).
pub const FLUXER_LOCALES: &[&str] = &[
    "ar", "be", "bg", "bn", "ca", "cs", "da", "de", "el", "en-GB", "en-US", "es-ES", "es-419",
    "fi", "fil", "fr", "he", "hi", "hr", "hu", "id", "it", "ja", "ko", "lt", "lv", "nl", "no",
    "pl", "pt-BR", "pt-PT", "ro", "ru", "sk", "sl", "sq", "sv-SE", "th", "tok", "tr", "uk", "vi",
    "zh-CN", "zh-TW",
];

/// Most GIFs (or suggestions, or categories) kept from one answer; Fluxer sends at most 50.
pub const MAX_GIFS: usize = 100;

/// A locale Fluxer accepts for `tag` (a BCP 47 tag such as `nb-NO`, `de-DE`
/// or `en`): the exact entry of [`FLUXER_LOCALES`] (any case), else Norwegian
/// `nb`/`nn` as `no`, else the entry for the language alone (`de-DE` is `de`,
/// `en` is `en-US`, `pt` is `pt-BR`, traditional Chinese is `zh-TW`), else
/// `en-US`. Only list values reach a request.
pub fn fluxer_locale(tag: &str) -> &'static str {
    let tag = tag.trim().replace('_', "-");
    if let Some(l) = FLUXER_LOCALES.iter().find(|l| l.eq_ignore_ascii_case(&tag)) {
        return l;
    }
    let lang = tag
        .split('-')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match lang.as_str() {
        "nb" | "nn" | "no" => "no",
        "en" => "en-US",
        "es" => "es-ES",
        "pt" => "pt-BR",
        "sv" => "sv-SE",
        "zh" => {
            let t = tag.to_ascii_lowercase();
            let traditional =
                t.contains("hant") || ["-tw", "-hk", "-mo"].iter().any(|r| t.ends_with(r));
            if traditional {
                "zh-TW"
            } else {
                "zh-CN"
            }
        }
        "tl" => "fil",
        "iw" => "he",
        "in" => "id",
        _ => FLUXER_LOCALES
            .iter()
            .find(|l| **l == lang)
            .copied()
            .unwrap_or("en-US"),
    }
}

impl Client {
    /// Waits for the account's next GIF search slot, or refuses when it is far away.
    async fn gif_slot(&self) -> Result<()> {
        let limiter = self.rate_limiter();
        match limiter
            .coalesce
            .reserve_gif_search(tokio::time::Instant::now())
        {
            Ok(wait) if wait.is_zero() => Ok(()),
            Ok(wait) => {
                tokio::time::sleep(wait).await;
                Ok(())
            }
            Err(retry_after) => Err(Error::RateLimited { retry_after }),
        }
    }

    /// `GET /gifs/search?q=..&locale=..`: up to 50 GIFs. Paced (module docs).
    pub async fn gif_search(&self, query: &str, locale_tag: &str) -> Result<Vec<Gif>> {
        crate::api::nonblank(query, 256, "GIF search")?;
        let path = format!(
            "/gifs/search?q={}&locale={}",
            enc_query(query),
            fluxer_locale(locale_tag)
        );
        self.gif_slot().await?;
        self.get_list_capped(&path, "GIF", MAX_GIFS).await
    }

    /// `GET /gifs/suggest?q=..&locale=..`: search terms for a partial query.
    /// Paced together with [`Client::gif_search`].
    pub async fn gif_suggestions(&self, query: &str, locale_tag: &str) -> Result<Vec<String>> {
        crate::api::nonblank(query, 256, "GIF search")?;
        let path = format!(
            "/gifs/suggest?q={}&locale={}",
            enc_query(query),
            fluxer_locale(locale_tag)
        );
        self.gif_slot().await?;
        self.get_list_capped(&path, "GIF suggestion", MAX_GIFS)
            .await
    }

    /// `GET /gifs/trending?locale=..`: up to 50 trending GIFs.
    pub async fn gif_trending(&self, locale_tag: &str) -> Result<Vec<Gif>> {
        let path = format!("/gifs/trending?locale={}", fluxer_locale(locale_tag));
        self.get_list_capped(&path, "GIF", MAX_GIFS).await
    }

    /// `GET /gifs/featured?locale=..`: a featured GIF and the categories.
    pub async fn gif_featured(&self, locale_tag: &str) -> Result<GifFeatured> {
        let path = format!("/gifs/featured?locale={}", fluxer_locale(locale_tag));
        let mut featured: GifFeatured = self.raw(Method::GET, &path, None).await?.json()?;
        featured.gifs.truncate(MAX_GIFS);
        featured.categories.truncate(MAX_GIFS);
        Ok(featured)
    }

    /// `POST /gifs/register-share`: tells the instance (which tells its
    /// provider) that a GIF was sent, as Fluxer's client does.
    pub async fn register_gif_share(
        &self,
        gif: &Gif,
        query: Option<&str>,
        locale_tag: &str,
    ) -> Result<()> {
        let id = if gif.slug.is_empty() {
            &gif.id
        } else {
            &gif.slug
        };
        crate::api::text(id, 300, "GIF id")?;
        let query = crate::api::optional_text(query, 256, "GIF search", false)?;
        let body = json!({ "id": id, "q": query, "locale": fluxer_locale(locale_tag) });
        self.raw(Method::POST, "/gifs/register-share", Some(&body))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locales_map_to_fluxers_list() {
        for (tag, want) in [
            ("en-US", "en-US"),
            ("EN-us", "en-US"),
            ("en", "en-US"),
            ("en_GB", "en-GB"),
            ("nb", "no"),
            ("nb-NO", "no"),
            ("nn-NO", "no"),
            ("no", "no"),
            ("de-DE", "de"),
            ("pt", "pt-BR"),
            ("pt-PT", "pt-PT"),
            ("zh-Hant-TW", "zh-TW"),
            ("zh-HK", "zh-TW"),
            ("zh", "zh-CN"),
            ("zh-TW", "zh-TW"),
            ("sv", "sv-SE"),
            ("", "en-US"),
            ("en-US&x=1", "en-US"),
            ("xx", "en-US"),
        ] {
            assert_eq!(fluxer_locale(tag), want, "{tag}");
        }
        assert!(FLUXER_LOCALES.iter().all(|l| fluxer_locale(l) == *l));
    }
}
