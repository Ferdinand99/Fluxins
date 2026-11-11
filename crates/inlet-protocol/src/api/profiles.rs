//! Changing the account's profile, account-wide and per guild.
//!
//! Reading profiles is [`crate::Client::user_profile`] (with `guild_id` for
//! the per-guild profile and member). Fluxer limits each profile field to 25
//! changes per 30 minutes (per guild for member profiles) and refuses more
//! with 400 `INVALID_FORM_BODY` (`AVATAR_CHANGED_TOO_MANY_TIMES`, ...). The
//! per-guild avatar, banner, bio and accent colour need the instance's
//! `feature_per_guild_profiles`; without it Fluxer ignores them.

use base64::Engine;
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::models::{CurrentUser, GuildMember};
use crate::rest::snowflake;
use crate::Client;

/// Largest image this crate uploads (Fluxer's default `avatar_max_size`).
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Image types Fluxer takes for avatars, banners and group DM icons (SVG is
/// left out on purpose: this crate never handles SVG from or for the network).
const IMAGE_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/apng",
    "image/avif",
];

/// An image to upload as an avatar, banner or icon (sent as a data URL).
#[derive(Clone, PartialEq, Eq)]
pub struct ImageData {
    content_type: String,
    bytes: Vec<u8>,
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageData")
            .field("content_type", &self.content_type)
            .field("len", &self.bytes.len())
            .finish()
    }
}

impl ImageData {
    /// An image of type `content_type` (PNG, JPEG, WebP, GIF, APNG or AVIF),
    /// at most [`MAX_IMAGE_BYTES`].
    pub fn new(content_type: &str, bytes: Vec<u8>) -> Result<Self> {
        let content_type = content_type.trim().to_ascii_lowercase();
        if !IMAGE_TYPES.contains(&content_type.as_str()) {
            return Err(Error::InvalidInput(format!(
                "image type {content_type} is not supported"
            )));
        }
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Err(Error::InvalidInput("image: 1 byte to 10 MiB".into()));
        }
        Ok(Self {
            content_type,
            bytes,
        })
    }

    /// `data:<type>;base64,<data>`.
    pub(crate) fn data_url(&self) -> String {
        format!(
            "data:{};base64,{}",
            self.content_type,
            base64::engine::general_purpose::STANDARD.encode(&self.bytes)
        )
    }
}

/// Changes to the account-wide profile ([`Client::edit_profile`]). `None`
/// keeps a field; `Some(None)` (or an empty string) clears it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProfileEdit {
    /// Display name (1-32 characters).
    pub global_name: Option<Option<String>>,
    /// Avatar.
    pub avatar: Option<Option<ImageData>>,
    /// Banner (needs the instance's premium entitlement).
    pub banner: Option<Option<ImageData>>,
    /// Biography (up to the instance's `max_bio_length`, 320 by default;
    /// this crate allows at most 4096 and the server checks the rest).
    pub bio: Option<Option<String>>,
    /// Pronouns (1-40 characters).
    pub pronouns: Option<Option<String>>,
    /// Accent colour as 24-bit RGB.
    pub accent_color: Option<Option<u32>>,
}

/// Changes to the account's profile in one guild ([`Client::edit_member_profile`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemberProfileEdit {
    /// Nickname (1-32 characters; needs `CHANGE_NICKNAME`).
    pub nick: Option<Option<String>>,
    /// Guild avatar.
    pub avatar: Option<Option<ImageData>>,
    /// Guild banner.
    pub banner: Option<Option<ImageData>>,
    /// Guild biography.
    pub bio: Option<Option<String>>,
    /// Guild pronouns (1-40 characters).
    pub pronouns: Option<Option<String>>,
    /// Guild accent colour as 24-bit RGB.
    pub accent_color: Option<Option<u32>>,
}

/// Adds a text field: 1 to `max` characters, no control characters (line
/// breaks allowed when `multiline`); blank text is sent as `null` (clear).
fn put_text(
    body: &mut Map<String, Value>,
    key: &str,
    value: &Option<Option<String>>,
    max: usize,
    multiline: bool,
) -> Result<()> {
    if let Some(v) = value {
        let v = v.as_deref().filter(|s| !s.trim().is_empty());
        if let Some(s) = v {
            let bad = |c: char| c.is_control() && !(multiline && c == '\n');
            if s.chars().count() > max || s.chars().any(bad) {
                return Err(Error::InvalidInput(format!(
                    "{key}: 1-{max} characters without control characters"
                )));
            }
        }
        body.insert(key.into(), json!(v));
    }
    Ok(())
}

fn put_image(body: &mut Map<String, Value>, key: &str, value: &Option<Option<ImageData>>) {
    if let Some(v) = value {
        body.insert(
            key.into(),
            v.as_ref().map_or(Value::Null, |i| json!(i.data_url())),
        );
    }
}

fn put_color(body: &mut Map<String, Value>, value: &Option<Option<u32>>) -> Result<()> {
    if let Some(v) = value {
        if v.is_some_and(|c| c > 0xFF_FFFF) {
            return Err(Error::InvalidInput("accent colour: 24-bit RGB".into()));
        }
        body.insert("accent_color".into(), json!(v));
    }
    Ok(())
}

fn finish(body: Map<String, Value>) -> Result<Value> {
    if body.is_empty() {
        return Err(Error::InvalidInput(
            "profile edit: nothing to change".into(),
        ));
    }
    Ok(Value::Object(body))
}

impl ProfileEdit {
    pub(crate) fn to_json(&self) -> Result<Value> {
        let mut body = Map::new();
        put_text(&mut body, "global_name", &self.global_name, 32, false)?;
        put_image(&mut body, "avatar", &self.avatar);
        put_image(&mut body, "banner", &self.banner);
        put_text(&mut body, "bio", &self.bio, 4096, true)?;
        put_text(&mut body, "pronouns", &self.pronouns, 40, false)?;
        put_color(&mut body, &self.accent_color)?;
        finish(body)
    }
}

impl MemberProfileEdit {
    pub(crate) fn to_json(&self) -> Result<Value> {
        let mut body = Map::new();
        put_text(&mut body, "nick", &self.nick, 32, false)?;
        put_image(&mut body, "avatar", &self.avatar);
        put_image(&mut body, "banner", &self.banner);
        put_text(&mut body, "bio", &self.bio, 4096, true)?;
        put_text(&mut body, "pronouns", &self.pronouns, 40, false)?;
        put_color(&mut body, &self.accent_color)?;
        finish(body)
    }
}

impl Client {
    /// `PATCH /users/@me`: changes the display name, avatar, banner, bio,
    /// pronouns or accent colour. Returns the account (USER_UPDATE follows).
    /// Username, email and password changes need sudo mode and are not here.
    pub async fn edit_profile(&self, edit: &ProfileEdit) -> Result<CurrentUser> {
        let body = edit.to_json()?;
        self.raw(Method::PATCH, "/users/@me", Some(&body))
            .await?
            .json()
    }

    /// `PATCH /guilds/{guild}/members/@me`: changes the account's nickname and
    /// profile in one guild. Returns the membership (GUILD_MEMBER_UPDATE follows).
    pub async fn edit_member_profile(
        &self,
        guild_id: &str,
        edit: &MemberProfileEdit,
    ) -> Result<GuildMember> {
        let body = edit.to_json()?;
        self.raw(
            Method::PATCH,
            &format!("/guilds/{}/members/@me", snowflake(guild_id)?),
            Some(&body),
        )
        .await?
        .json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_edit_bodies() {
        let edit = ProfileEdit {
            global_name: Some(Some("Ana".into())),
            bio: Some(None),
            pronouns: Some(Some(String::new())),
            accent_color: Some(Some(0x5865F2)),
            avatar: Some(Some(ImageData::new("image/png", vec![1, 2, 3]).unwrap())),
            ..Default::default()
        };
        assert_eq!(
            edit.to_json().unwrap(),
            json!({"global_name": "Ana", "avatar": "data:image/png;base64,AQID",
                   "bio": null, "pronouns": null, "accent_color": 0x5865F2})
        );
        assert!(ProfileEdit::default().to_json().is_err());
        let bad = ProfileEdit {
            accent_color: Some(Some(0x100_0000)),
            ..Default::default()
        };
        assert!(bad.to_json().is_err());
        let long = MemberProfileEdit {
            pronouns: Some(Some("x".repeat(41))),
            ..Default::default()
        };
        assert!(long.to_json().is_err());
        let bio = MemberProfileEdit {
            bio: Some(Some("line one\nline two".into())),
            ..Default::default()
        };
        assert!(bio.to_json().is_ok());
        let name = ProfileEdit {
            global_name: Some(Some("a\nb".into())),
            ..Default::default()
        };
        assert!(name.to_json().is_err());
        assert!(ImageData::new("image/svg+xml", vec![1]).is_err());
        assert!(ImageData::new("image/png", Vec::new()).is_err());
        // Debug never prints the bytes.
        let dbg = format!("{:?}", ImageData::new("image/png", vec![7; 64]).unwrap());
        assert!(dbg.contains("len: 64") && !dbg.contains("7, 7"));
    }
}
