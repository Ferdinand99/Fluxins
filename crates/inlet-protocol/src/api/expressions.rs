//! Custom emoji, stickers and memes.
//!
//! - A guild's emoji and stickers: READY and GUILD_CREATE
//!   ([`crate::GuildReady::emojis`], [`crate::GuildReady::stickers`]),
//!   [`crate::Client::guild_emojis`], [`crate::Client::guild_stickers`] and
//!   GUILD_EMOJIS_UPDATE / GUILD_STICKERS_UPDATE. Images come from
//!   [`crate::Media::emoji`] and [`crate::Media::sticker`].
//! - An emoji or sticker seen in a message from a guild the account is not
//!   in: [`crate::Client::emoji_metadata`] and [`crate::Client::emoji_source`]
//!   (and the sticker versions) say which guild owns it.
//! - Memes: media files saved to the account (Fluxer keeps its own copy),
//!   in READY `favorite_memes` ([`crate::Ready::favorite_memes`]) and the
//!   FAVORITE_MEME_* events. Send one with [`crate::Client::send_meme`].

use std::collections::BTreeMap;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::media::{Media, Transform};
use crate::models::{
    lenient_bool, lenient_vec, null_as_default, opt_sf, opt_u64, sf, u64_or_zero, Message, Ready,
    Snowflake,
};
use crate::rest::snowflake;
use crate::Client;

/// Most tags on one meme (Fluxer's default `max_favorite_meme_tags`).
pub const MAX_MEME_TAGS: usize = 10;

/// Who owns a custom emoji or sticker (`GET /emojis/{id}/metadata`,
/// `GET /stickers/{id}/metadata`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpressionMetadata {
    /// Emoji or sticker ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// The guild that owns it.
    #[serde(deserialize_with = "sf")]
    pub guild_id: Snowflake,
    /// Name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Animated.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub animated: bool,
    /// The guild lets people copy it to their own guild.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub allow_cloning: bool,
}

/// A short view of the guild that owns an emoji or sticker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpressionSourceGuild {
    /// Guild ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Icon hash ([`crate::Media::guild_icon`]).
    #[serde(default)]
    pub icon: Option<String>,
    /// Which of `VERIFIED`, `PARTNERED` and `DISCOVERABLE` it has.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub features: Vec<String>,
}

/// One saved meme.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meme {
    /// Meme ID.
    #[serde(deserialize_with = "sf")]
    pub id: Snowflake,
    /// Display name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Accessibility description.
    #[serde(default)]
    pub alt_text: Option<String>,
    /// Search tags.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub tags: Vec<String>,
    /// The stored copy's attachment ID.
    #[serde(default, deserialize_with = "opt_sf")]
    pub attachment_id: Option<Snowflake>,
    /// The stored copy's file name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub filename: String,
    /// MIME type.
    #[serde(default, deserialize_with = "null_as_default")]
    pub content_type: String,
    /// Size in bytes.
    #[serde(default, deserialize_with = "u64_or_zero")]
    pub size: u64,
    /// Width in pixels.
    #[serde(default, deserialize_with = "opt_u64")]
    pub width: Option<u64>,
    /// Height in pixels.
    #[serde(default, deserialize_with = "opt_u64")]
    pub height: Option<u64>,
    /// Signed attachment URL of the stored copy. Load it only through
    /// [`Meme::proxied`].
    #[serde(default, deserialize_with = "null_as_default")]
    pub url: String,
    /// Animated, or a video converted from a GIF.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub is_gifv: bool,
    /// Flagged as explicit.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub nsfw: bool,
    /// Every other field (`gif_slug`, `media`, `placeholder`, `duration`, ...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Meme {
    /// The stored copy through the instance's media endpoint, or `None` when
    /// `url` is not an attachment URL there.
    pub fn proxied(&self, media: &Media, t: Transform) -> Option<String> {
        media.attachment(&self.url, t)
    }
}

/// Saving a meme from a message ([`Client::save_meme_from_message`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewMeme {
    /// Display name (1-100 characters).
    pub name: String,
    /// Which attachment (or `None` with `embed_index` `None`: the first one).
    pub attachment_id: Option<Snowflake>,
    /// Which embed, zero-based.
    pub embed_index: Option<u32>,
    /// Accessibility description (up to 500 characters).
    pub alt_text: Option<String>,
    /// Tags, each 1-30 characters, at most 10.
    pub tags: Vec<String>,
}

/// Changes to a meme ([`Client::edit_meme`]). `None` keeps a field.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemeEdit {
    /// Display name (1-100 characters).
    pub name: Option<String>,
    /// Accessibility description; `Some(None)` clears it.
    pub alt_text: Option<Option<String>>,
    /// Tags (replaces them all).
    pub tags: Option<Vec<String>>,
}

fn check_tags(tags: &[String]) -> Result<()> {
    if tags.len() > MAX_MEME_TAGS {
        return Err(Error::InvalidInput(format!(
            "at most {MAX_MEME_TAGS} meme tags"
        )));
    }
    for t in tags {
        crate::api::nonblank(t, 30, "meme tag")?;
    }
    Ok(())
}

/// Most memes kept from one list (Fluxer's default limit is 50 per account).
pub const MAX_MEMES: usize = 1_000;

impl NewMeme {
    fn to_json(&self) -> Result<Value> {
        crate::api::nonblank(&self.name, 100, "meme name")?;
        check_tags(&self.tags)?;
        let mut body = Map::new();
        body.insert("name".into(), json!(self.name));
        if let Some(a) = &self.attachment_id {
            body.insert("attachment_id".into(), json!(snowflake(a)?));
        }
        if let Some(i) = self.embed_index {
            body.insert("embed_index".into(), json!(i));
        }
        if let Some(alt) =
            crate::api::optional_text(self.alt_text.as_deref(), 500, "alt text", false)?
        {
            body.insert("alt_text".into(), json!(alt));
        }
        if !self.tags.is_empty() {
            body.insert("tags".into(), json!(self.tags));
        }
        Ok(Value::Object(body))
    }
}

impl MemeEdit {
    fn to_json(&self) -> Result<Value> {
        let mut body = Map::new();
        if let Some(n) = &self.name {
            body.insert(
                "name".into(),
                json!(crate::api::nonblank(n, 100, "meme name")?),
            );
        }
        if let Some(alt) = &self.alt_text {
            let alt = crate::api::optional_text(alt.as_deref(), 500, "alt text", false)?;
            body.insert("alt_text".into(), json!(alt));
        }
        if let Some(tags) = &self.tags {
            check_tags(tags)?;
            body.insert("tags".into(), json!(tags));
        }
        if body.is_empty() {
            return Err(Error::InvalidInput("meme edit: nothing to change".into()));
        }
        Ok(Value::Object(body))
    }
}

impl Ready {
    /// The account's saved memes (at most [`MAX_MEMES`]).
    pub fn favorite_memes(&self) -> Vec<Meme> {
        match self.extra.get("favorite_memes") {
            Some(Value::Array(items)) => {
                crate::models::parse_values(&items[..items.len().min(MAX_MEMES)], "favorite meme")
            }
            _ => Vec::new(),
        }
    }
}

impl Client {
    /// `GET /emojis/{id}/metadata`: the guild that owns a custom emoji (also
    /// when the account is not in it).
    pub async fn emoji_metadata(&self, emoji_id: &str) -> Result<ExpressionMetadata> {
        self.raw(
            Method::GET,
            &format!("/emojis/{}/metadata", snowflake(emoji_id)?),
            None,
        )
        .await?
        .json()
    }

    /// `GET /stickers/{id}/metadata`.
    pub async fn sticker_metadata(&self, sticker_id: &str) -> Result<ExpressionMetadata> {
        self.raw(
            Method::GET,
            &format!("/stickers/{}/metadata", snowflake(sticker_id)?),
            None,
        )
        .await?
        .json()
    }

    /// `GET /emojis/{id}/source`: the owning guild, when it is discoverable
    /// or the account is in it (404 `UNKNOWN_GUILD` otherwise).
    pub async fn emoji_source(&self, emoji_id: &str) -> Result<ExpressionSourceGuild> {
        self.raw(
            Method::GET,
            &format!("/emojis/{}/source", snowflake(emoji_id)?),
            None,
        )
        .await?
        .json()
    }

    /// `GET /stickers/{id}/source`.
    pub async fn sticker_source(&self, sticker_id: &str) -> Result<ExpressionSourceGuild> {
        self.raw(
            Method::GET,
            &format!("/stickers/{}/source", snowflake(sticker_id)?),
            None,
        )
        .await?
        .json()
    }

    /// `GET /users/@me/memes`: saved memes, newest first.
    pub async fn memes(&self) -> Result<Vec<Meme>> {
        self.get_list_capped("/users/@me/memes", "meme", MAX_MEMES)
            .await
    }

    /// `POST /channels/{channel}/messages/{message}/memes`: saves an
    /// attachment or embed of a message as a meme.
    pub async fn save_meme_from_message(
        &self,
        channel_id: &str,
        message_id: &str,
        meme: &NewMeme,
    ) -> Result<Meme> {
        let body = meme.to_json()?;
        self.raw(
            Method::POST,
            &format!(
                "/channels/{}/messages/{}/memes",
                snowflake(channel_id)?,
                snowflake(message_id)?
            ),
            Some(&body),
        )
        .await?
        .json()
    }

    /// `POST /users/@me/memes` for a GIF from [`crate::Client::gif_search`]:
    /// the instance fetches and stores it (this client never contacts the
    /// provider).
    pub async fn save_gif_as_meme(&self, gif: &crate::api::gifs::Gif) -> Result<Meme> {
        let mut body = json!({ "url": gif.url });
        if !gif.slug.is_empty() && !gif.provider.is_empty() {
            body["gif_slug"] = json!(gif.slug);
            body["gif_provider"] = json!(gif.provider);
        }
        let title: String = gif.title.chars().take(100).collect();
        if !title.trim().is_empty() && !title.chars().any(char::is_control) {
            body["name"] = json!(title);
        }
        self.raw(Method::POST, "/users/@me/memes", Some(&body))
            .await?
            .json()
    }

    /// `PATCH /users/@me/memes/{id}`: renames, re-tags or describes a meme.
    pub async fn edit_meme(&self, meme_id: &str, edit: &MemeEdit) -> Result<Meme> {
        let body = edit.to_json()?;
        self.raw(
            Method::PATCH,
            &format!("/users/@me/memes/{}", snowflake(meme_id)?),
            Some(&body),
        )
        .await?
        .json()
    }

    /// `DELETE /users/@me/memes/{id}`.
    pub async fn delete_meme(&self, meme_id: &str) -> Result<()> {
        self.raw(
            Method::DELETE,
            &format!("/users/@me/memes/{}", snowflake(meme_id)?),
            None,
        )
        .await?;
        Ok(())
    }

    /// `POST /channels/{channel}/messages` with `favorite_meme_id`: sends a
    /// saved meme as a message.
    pub async fn send_meme(
        &self,
        channel_id: &str,
        meme_id: &str,
        nonce: Option<&str>,
    ) -> Result<Message> {
        let mut body = json!({ "favorite_meme_id": snowflake(meme_id)? });
        if let Some(n) = nonce {
            body["nonce"] = json!(snowflake(n)?);
        }
        self.raw(
            Method::POST,
            &format!("/channels/{}/messages", snowflake(channel_id)?),
            Some(&body),
        )
        .await?
        .json()
    }
}

/// Media formats of a GIF-sourced meme (`media`), by format name.
pub fn meme_gif_formats(meme: &Meme) -> BTreeMap<String, crate::api::gifs::GifFormat> {
    match meme.extra.get("media") {
        Some(Value::Object(m)) => m
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), serde_json::from_value(v.clone()).ok()?)))
            .collect(),
        _ => BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meme_bodies_are_checked() {
        let new = NewMeme {
            name: "parrot".into(),
            attachment_id: Some("77".into()),
            tags: vec!["party".into()],
            ..Default::default()
        };
        assert_eq!(
            new.to_json().unwrap(),
            json!({"name": "parrot", "attachment_id": "77", "tags": ["party"]})
        );
        let too_many = NewMeme {
            name: "x".into(),
            tags: vec!["t".into(); 11],
            ..Default::default()
        };
        assert!(too_many.to_json().is_err());
        assert!(NewMeme::default().to_json().is_err());
        assert!(MemeEdit::default().to_json().is_err());
        let clear = MemeEdit {
            alt_text: Some(None),
            ..Default::default()
        };
        assert_eq!(clear.to_json().unwrap(), json!({"alt_text": null}));
    }
}
