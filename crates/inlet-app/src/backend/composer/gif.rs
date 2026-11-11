// SPDX-License-Identifier: AGPL-3.0-or-later
//! The pickers' requests: GIFs through the instance's GIF service, the synced
//! preferences the pickers keep (favourite and recently used emoji, the skin tone,
//! favourite GIFs), the saved memes, and sending a sticker or a meme.
//!
//! - **GIFs** go only to the instance (`/gifs/*` on its API); the provider is never
//!   contacted. The UI builds previews from the media proxy URLs only (`Gif::preview`),
//!   so a provider URL is never fetched.
//! - **Preferences** are written with `update_synced_preferences`: one writer per
//!   account, edits batched, maps merged by key, and nothing sent when the write would
//!   lose a field. An edit ([`PrefEdit`]) sets values on the snapshot it is given and
//!   never panics, whatever the snapshot holds. Where they go is where Fluxer keeps
//!   them: `emoji_picker.usage` (`unicode:<name>`, `custom:<guild>:<id>`),
//!   `emoji_picker.favorite_emoji_ids`, `emoji.skin_tone` (the modifier itself) and
//!   `favorite_gifs.entries`.
//!
//! Request budget (all REST goes through the protocol's limiter):
//! - GIF search: the UI waits 350 ms after the last key and asks only for the newest
//!   text; the protocol starts at most one search per account every 500 ms (one more
//!   than 3 s away is refused without a request);
//! - the GIF home (featured search terms) and trending: at most one request of each
//!   kind per account every [`GIF_HOME_INTERVAL`]; the UI keeps the answers for the
//!   session;
//! - one `POST /gifs/register-share` per GIF picked, as Fluxer's client does;
//! - preferences: no request when an edit changes nothing; otherwise batched by the
//!   protocol (one PATCH per 500 ms batch, two at least 1 s apart); one `GET` when the
//!   picker opens for an account whose READY gave no preferences;
//! - memes: one `GET` when the memes tab opens and READY gave none;
//! - a picked sticker or meme is one message (`POST`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use inlet_protocol as p;
use inlet_protocol::api::gifs::GifCategory;
use inlet_protocol::api::preferences::Preferences;
use inlet_protocol::proto::preferences::{
    EmojiPickerState, EmojiState, FavoriteGifEntry, FavoriteGifSettings, UsageStat,
};

use super::State;
use crate::backend::session::{Failure, FailureKind};
use crate::backend::{Active, Backend, Internal, Ui};

/// Shortest time between two requests for the GIF home (or for trending) of one account.
pub const GIF_HOME_INTERVAL: Duration = Duration::from_secs(10);
/// Emoji usage entries kept (Fluxer's `MAX_TRACKED_USAGE_KEYS`).
pub const MAX_USAGE_KEYS: usize = 200;
/// Favourite emoji kept (Fluxer's `MAX_FAVORITE_EMOJIS`).
pub const MAX_FAVOURITE_EMOJI: usize = 500;
/// Favourite GIFs kept.
pub const MAX_FAVOURITE_GIFS: usize = 500;
/// Longest usage or favourite key (Fluxer's `MAX_USAGE_KEY_LENGTH`).
pub const MAX_KEY_LEN: usize = 256;
/// Highest usage count (Fluxer's `MAX_USAGE_COUNT`).
const MAX_USAGE_COUNT: u32 = 100_000;
/// Usage counts lose half their weight in this time (Fluxer's frecency half-life).
const HALF_LIFE_MS: f64 = 7. * 24. * 3600. * 1000.;
/// Prefix of Fluxer's usage key of a Unicode emoji: `unicode:<name>`.
pub const UNICODE_PREFIX: &str = "unicode:";
/// Prefix of Fluxer's usage key of a custom emoji: `custom:<guild>:<id>`.
pub const CUSTOM_PREFIX: &str = "custom:";

pub enum Cmd {
    /// What the pickers need from the instance. `traits` are the account's (from the
    /// account state), for the limit that allows emoji of other communities.
    Setup { traits: Vec<String> },
    /// GIFs for a search. Answered with [`Ev::Gifs`] carrying `nonce`.
    SearchGifs {
        query: String,
        locale: String,
        nonce: u64,
    },
    /// The GIF home: the featured search terms. Answered with [`Ev::Categories`].
    FeaturedGifs { locale: String, nonce: u64 },
    /// Trending GIFs. Answered with [`Ev::Gifs`].
    TrendingGifs { locale: String, nonce: u64 },
    /// A GIF was picked: told to the instance, as Fluxer's client does.
    SharedGif {
        gif: Box<p::Gif>,
        query: Option<String>,
        locale: String,
    },
    /// The account's synced preferences, when READY did not bring them.
    LoadPreferences,
    /// A change to the synced preferences, on top of `base` (the snapshot the UI knows).
    EditPreferences {
        base: Arc<p::SyncedPreferences>,
        edit: PrefEdit,
    },
    /// The account's saved memes, when READY did not bring them.
    LoadMemes,
    /// Sends a sticker as a message of its own.
    SendSticker {
        channel_id: String,
        sticker_id: String,
        nonce: String,
    },
    /// Sends a saved meme as a message of its own.
    SendMeme {
        channel_id: String,
        meme_id: String,
        nonce: String,
    },
}

pub enum Ev {
    Setup(Setup),
    Gifs {
        nonce: u64,
        result: Result<Vec<p::Gif>, String>,
    },
    /// The featured search terms, and the featured GIF (the trending tile's picture).
    Categories {
        nonce: u64,
        result: Result<(Vec<GifCategory>, Option<Box<p::Gif>>), String>,
    },
    /// The preferences as stored, read for the pickers.
    Preferences(Box<p::SyncedPreferences>),
    /// The preferences as stored after one [`Cmd::EditPreferences`] (one answer per
    /// edit, success or failure).
    Edited(Box<p::SyncedPreferences>),
    /// A preference write failed: the UI goes back to the stored snapshot.
    PreferencesFailed(String),
    /// Reading the preferences failed; the pickers ask again next time they open.
    PreferencesLoadFailed(String),
    Memes(Vec<p::Meme>),
    /// Reading the memes failed; the tab says so and asks again when it opens next.
    MemesFailed(String),
    /// A sticker or meme was not sent.
    SendFailed(String),
}

/// What the pickers know about the instance and the account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Setup {
    /// The instance has a GIF service (`features.gif_enabled`).
    pub gif_enabled: bool,
    /// "Powered by …": the provider's name when the instance asks for the credit.
    pub gif_attribution: Option<String>,
    /// The account may use emoji and stickers of other communities (the limit
    /// `feature_global_expressions`, off unless the instance grants it).
    pub global_expressions: bool,
}

impl Setup {
    /// The setup from the instance's discovery document and the account's traits.
    pub fn of(discovery: &p::Discovery, traits: &[String]) -> Setup {
        let gif_attribution = discovery
            .gif_provider()
            .filter(|g| g.attribution_required)
            .map(|g| crate::model::clean_name(&g.display_name))
            .filter(|n| !n.is_empty());
        Setup {
            gif_enabled: discovery.gif_enabled(),
            gif_attribution,
            global_expressions: discovery
                .resolve_limit("feature_global_expressions", traits)
                .unwrap_or(0)
                > 0,
        }
    }
}

// ---- Preference edits -------------------------------------------------------------------

/// One change to the pickers' synced preferences. Each sets a value on the snapshot it
/// is applied to, so applying it again after a conflict gives the right result.
#[derive(Clone, Debug, PartialEq)]
pub enum PrefEdit {
    /// An emoji was used at `at_ms` (Unix milliseconds): its usage count goes up.
    UsedEmoji { key: String, at_ms: i64 },
    /// An emoji is a favourite (`on`) or not.
    FavouriteEmoji { key: String, on: bool },
    /// The skin tone: `""` for the default, or one of the five modifiers.
    SkinTone(String),
    /// A GIF is a favourite (`on`) or not; matched by its `url`.
    FavouriteGif {
        entry: Box<FavoriteGifEntry>,
        on: bool,
    },
}

/// Whether `key` is one of Fluxer's emoji usage keys: `unicode:<name>` with a name, or
/// `custom:<guild>:<id>` with a snowflake ID (the guild may be empty, as Fluxer writes
/// it for an emoji without one).
pub fn is_emoji_key(key: &str) -> bool {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return false;
    }
    let snowflake =
        |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(name) = key.strip_prefix(UNICODE_PREFIX) {
        return !name.is_empty() && !name.contains(char::is_whitespace);
    }
    match key
        .strip_prefix(CUSTOM_PREFIX)
        .and_then(|r| r.split_once(':'))
    {
        Some((guild, id)) => snowflake(id) && (guild.is_empty() || snowflake(guild)),
        None => false,
    }
}

/// Fluxer's frecency of a usage entry at `now_ms`: `log2(count + 1)`, halved every week.
pub fn frecency(stat: &UsageStat, now_ms: i64) -> f64 {
    let age = now_ms.saturating_sub(stat.last_used_ms).max(0) as f64;
    (f64::from(stat.count) + 1.).log2() * 0.5f64.powf(age / HALF_LIFE_MS)
}

/// Usage keys, best first (Fluxer's order: score, then last use, count, key).
pub fn ranked<'a>(
    usage: impl IntoIterator<Item = (&'a String, &'a UsageStat)>,
    now_ms: i64,
) -> Vec<&'a String> {
    let mut all: Vec<(&String, &UsageStat, f64)> = usage
        .into_iter()
        .filter(|(_, s)| s.count > 0)
        .map(|(k, s)| (k, s, frecency(s, now_ms)))
        .collect();
    all.sort_by(|a, b| {
        b.2.total_cmp(&a.2)
            .then(b.1.last_used_ms.cmp(&a.1.last_used_ms))
            .then(b.1.count.cmp(&a.1.count))
            .then(a.0.cmp(b.0))
    });
    all.into_iter().map(|(k, _, _)| k).collect()
}

/// Whether `tone` is a skin tone Fluxer stores: empty or one modifier.
pub fn is_skin_tone(tone: &str) -> bool {
    tone.is_empty()
        || inlet_emoji::SkinTone::TONES
            .iter()
            .filter_map(|t| t.modifier())
            .any(|m| tone.chars().eq(std::iter::once(m)))
}

impl PrefEdit {
    /// Applies the edit. Invalid input (a bad key, an unknown tone, a GIF without a URL)
    /// changes nothing; nothing here can panic.
    pub fn apply(&self, prefs: &mut Preferences) {
        match self {
            PrefEdit::UsedEmoji { key, at_ms } => {
                if !is_emoji_key(key) {
                    return;
                }
                let at_ms = (*at_ms).max(0);
                let state = prefs
                    .emoji_picker
                    .get_or_insert_with(EmojiPickerState::default);
                let stat = state.usage.entry(key.clone()).or_default();
                stat.count = stat.count.saturating_add(1).min(MAX_USAGE_COUNT);
                stat.last_used_ms = stat.last_used_ms.max(at_ms);
                if state.usage.len() > MAX_USAGE_KEYS {
                    let keep: std::collections::HashSet<String> = ranked(&state.usage, at_ms)
                        .into_iter()
                        .take(MAX_USAGE_KEYS)
                        .cloned()
                        .collect();
                    state.usage.retain(|k, _| keep.contains(k));
                }
            }
            PrefEdit::FavouriteEmoji { key, on } => {
                if !is_emoji_key(key) {
                    return;
                }
                let state = prefs
                    .emoji_picker
                    .get_or_insert_with(EmojiPickerState::default);
                let has = state.favorite_emoji_ids.iter().any(|k| k == key);
                if *on && !has && state.favorite_emoji_ids.len() < MAX_FAVOURITE_EMOJI {
                    state.favorite_emoji_ids.push(key.clone());
                } else if !*on {
                    state.favorite_emoji_ids.retain(|k| k != key);
                }
            }
            PrefEdit::SkinTone(tone) => {
                if !is_skin_tone(tone) {
                    return;
                }
                prefs
                    .emoji
                    .get_or_insert_with(EmojiState::default)
                    .skin_tone
                    .clone_from(tone);
            }
            PrefEdit::FavouriteGif { entry, on } => {
                if entry.url.is_empty() || entry.url.len() > 2048 {
                    return;
                }
                let gifs = prefs
                    .favorite_gifs
                    .get_or_insert_with(FavoriteGifSettings::default);
                let has = gifs.entries.iter().any(|e| e.url == entry.url);
                if *on && !has && gifs.entries.len() < MAX_FAVOURITE_GIFS {
                    gifs.entries.push((**entry).clone());
                } else if !*on {
                    gifs.entries.retain(|e| e.url != entry.url);
                }
            }
        }
    }
}

/// The content type Fluxer gives a GIF format key (`inferFormatContentType` in Fluxer's
/// `GifMediaFormatKeys.ts`); empty for a key it does not know.
pub fn format_content_type(key: &str) -> &'static str {
    match key {
        "gif" | "mediumgif" | "nanogif" | "tinygif" => "image/gif",
        "webp" | "mediumwebp" | "nanowebp" | "tinywebp" => "image/webp",
        "mp4" | "loopedmp4" | "mediummp4" | "nanomp4" | "tinymp4" => "video/mp4",
        "webm" | "mediumwebm" | "nanowebm" | "tinywebm" => "video/webm",
        _ => "",
    }
}

/// Fluxer's preview order of the formats (`PREVIEW_FORMAT_PRIORITY`).
const PREVIEW_FORMAT_PRIORITY: [&str; 17] = [
    "tinywebm",
    "tinymp4",
    "mediumwebm",
    "mediummp4",
    "webm",
    "mp4",
    "loopedmp4",
    "nanowebm",
    "nanomp4",
    "tinywebp",
    "tinygif",
    "mediumwebp",
    "webp",
    "mediumgif",
    "gif",
    "nanowebp",
    "nanogif",
];
/// The image formats of a GIF, best first for a tile (Fluxer's preview order, images
/// only).
pub const IMAGE_KEYS: [&str; 8] = [
    "tinywebp",
    "tinygif",
    "mediumwebp",
    "webp",
    "mediumgif",
    "gif",
    "nanowebp",
    "nanogif",
];
/// The width, in device pixels, Fluxer picks a stored preview for (a 200 px tile at 2x).
const STORED_PREVIEW_WIDTH: u64 = 400;

/// A favourite GIF entry for a GIF from a search, in the shape Fluxer stores
/// (`slimFavoriteGifEntry`): one preview format in `proxy_url` with its size and
/// `content_type`, and `media` empty, so Fluxer's client reads it unchanged. Fluxer picks
/// its preview among every format and prefers a video; Inlet plays no video, so it picks
/// the best image format by Fluxer's rule (`pickBestPreviewFormat(media, 'image')`: the
/// narrowest at least 400 px wide, else the widest; then Fluxer's order). Without an
/// image format the default one is kept, with no content type.
pub fn favourite_entry(gif: &p::Gif) -> FavoriteGifEntry {
    let priority = |key: &str| {
        PREVIEW_FORMAT_PRIORITY
            .iter()
            .position(|k| *k == key)
            .unwrap_or(PREVIEW_FORMAT_PRIORITY.len())
    };
    let images: Vec<(&String, &p::api::gifs::GifFormat)> = gif
        .media
        .iter()
        .filter(|(key, f)| {
            format_content_type(key).starts_with("image/")
                && !f.proxy_src.is_empty()
                && f.width > 0
                && f.height > 0
        })
        .collect();
    let filling: Vec<_> = images
        .iter()
        .filter(|(_, f)| f.width >= STORED_PREVIEW_WIDTH)
        .copied()
        .collect();
    let narrowest_filling = !filling.is_empty();
    let mut pool = if narrowest_filling { filling } else { images };
    pool.sort_by(|(ka, a), (kb, b)| {
        let by_width = if narrowest_filling {
            a.width.cmp(&b.width)
        } else {
            b.width.cmp(&a.width)
        };
        by_width
            .then(priority(ka).cmp(&priority(kb)))
            .then(ka.cmp(kb))
    });
    let dim = |v: u64| u32::try_from(v).unwrap_or(0);
    let (proxy_url, width, height, content_type) = match pool.first() {
        Some((key, f)) => (
            f.proxy_src.clone(),
            dim(f.width),
            dim(f.height),
            format_content_type(key).to_owned(),
        ),
        None => (
            gif.proxy_src.clone(),
            dim(gif.width),
            dim(gif.height),
            String::new(),
        ),
    };
    FavoriteGifEntry {
        url: gif.url.clone(),
        proxy_url,
        width,
        height,
        media: Default::default(),
        content_type,
        placeholder: String::new(),
    }
}

// ---- Requests ------------------------------------------------------------------------------

/// A failure as a sentence; `None` when the session ended (the loop is told).
fn reason(
    internal: &tokio::sync::mpsc::UnboundedSender<Internal>,
    gen: u64,
    err: p::Error,
) -> Option<String> {
    let failure = Failure::from(err);
    if failure.kind == FailureKind::SessionEnded {
        let _ = internal.send(Internal::Ended {
            gen,
            message: failure.message,
        });
        return None;
    }
    Some(failure.message)
}

/// The system's language for the GIF service (the protocol maps it onto Fluxer's list).
pub fn locale() -> String {
    sys_locale::get_locale().unwrap_or_else(|| "en-US".into())
}

/// A GIF search through the protocol's paced slot.
pub async fn search_gifs(client: &p::Client, query: &str, locale: &str) -> p::Result<Vec<p::Gif>> {
    let query: String = query.trim().chars().take(256).collect();
    client.gif_search(&query, locale).await
}

/// The parts of the active session the pickers' tasks need.
struct Ctx {
    client: p::Client,
    events: Ui,
    internal: tokio::sync::mpsc::UnboundedSender<Internal>,
    gen: u64,
    state: State,
}

impl Backend {
    fn pickers_ctx(&self) -> Option<Ctx> {
        match &self.active {
            Some(Active::Real(r)) => Some(Ctx {
                client: r.session.client().clone(),
                events: self.events.clone(),
                internal: self.internal.clone(),
                gen: r.gen,
                state: r.composer.clone(),
            }),
            _ => None,
        }
    }

    pub(super) fn pickers(&mut self, cmd: Cmd) {
        #[cfg(feature = "demo")]
        if let Some(Active::Demo(_)) = &self.active {
            return crate::demo::pickers::answer(&self.events, cmd);
        }
        let Some(ctx) = self.pickers_ctx() else {
            // No session: every request that waits for an answer gets one.
            let none = || t!("composer-not-connected");
            let ev = match cmd {
                Cmd::SearchGifs { nonce, .. } | Cmd::TrendingGifs { nonce, .. } => Ev::Gifs {
                    nonce,
                    result: Err(none()),
                },
                Cmd::FeaturedGifs { nonce, .. } => Ev::Categories {
                    nonce,
                    result: Err(none()),
                },
                Cmd::EditPreferences { .. } => Ev::PreferencesFailed(none()),
                Cmd::LoadPreferences => Ev::PreferencesLoadFailed(none()),
                Cmd::LoadMemes => Ev::MemesFailed(none()),
                Cmd::SendSticker { .. } | Cmd::SendMeme { .. } => Ev::SendFailed(none()),
                _ => return,
            };
            return self.emit(super::Ev::Pickers(ev));
        };
        match cmd {
            Cmd::Setup { traits } => {
                let setup = Setup::of(ctx.client.instance().discovery(), &traits);
                self.emit(super::Ev::Pickers(Ev::Setup(setup)));
            }
            Cmd::SearchGifs {
                query,
                locale,
                nonce,
            } => {
                self.spawn_guarded("gif search", None, async move {
                    let result = search_gifs(&ctx.client, &query, &locale).await;
                    ctx.gifs(nonce, result);
                });
            }
            Cmd::TrendingGifs { locale, nonce } => {
                if !ctx.state.gif_home_slot(ctx.gen, 0, Instant::now()) {
                    return self.emit(super::Ev::Pickers(Ev::Gifs {
                        nonce,
                        result: Err(t!("picker-gif-wait")),
                    }));
                }
                self.spawn_guarded("gif trending", None, async move {
                    let result = ctx.client.gif_trending(&locale).await;
                    ctx.gifs(nonce, result);
                });
            }
            Cmd::FeaturedGifs { locale, nonce } => {
                if !ctx.state.gif_home_slot(ctx.gen, 1, Instant::now()) {
                    return self.emit(super::Ev::Pickers(Ev::Categories {
                        nonce,
                        result: Err(t!("picker-gif-wait")),
                    }));
                }
                self.spawn_guarded("gif featured", None, async move {
                    let result = match ctx.client.gif_featured(&locale).await {
                        Ok(f) => Ok((f.categories, f.gifs.into_iter().next().map(Box::new))),
                        Err(err) => match reason(&ctx.internal, ctx.gen, err) {
                            Some(r) => Err(r),
                            None => return,
                        },
                    };
                    let _ = ctx
                        .events
                        .unbounded_send(super::Ev::Pickers(Ev::Categories { nonce, result }));
                });
            }
            Cmd::SharedGif { gif, query, locale } => {
                self.spawn_guarded("gif share", None, async move {
                    let query = query.filter(|q| !q.trim().is_empty());
                    let shared = ctx
                        .client
                        .register_gif_share(&gif, query.as_deref(), &locale)
                        .await;
                    if let Err(err) = shared {
                        // Only a courtesy to the provider: nothing to show, but a token
                        // that is gone ends the session as anywhere else.
                        log::debug!("a GIF share was not registered");
                        let _ = reason(&ctx.internal, ctx.gen, err);
                    }
                });
            }
            Cmd::LoadPreferences => {
                self.spawn_guarded("load preferences", None, async move {
                    match ctx.client.synced_preferences().await {
                        Ok(prefs) => {
                            let _ = ctx
                                .events
                                .unbounded_send(super::Ev::Pickers(Ev::Preferences(Box::new(
                                    prefs,
                                ))));
                        }
                        Err(err) => {
                            if let Some(r) = reason(&ctx.internal, ctx.gen, err) {
                                let _ = ctx.events.unbounded_send(super::Ev::Pickers(
                                    Ev::PreferencesLoadFailed(r),
                                ));
                            }
                        }
                    }
                });
            }
            Cmd::EditPreferences { base, edit } => {
                self.spawn_guarded("edit preferences", None, async move {
                    let ev = match edit_preferences(&ctx.client, &base, edit).await {
                        Ok(stored) => Ev::Edited(Box::new(stored)),
                        Err(err) => match reason(&ctx.internal, ctx.gen, err) {
                            Some(r) => Ev::PreferencesFailed(r),
                            None => return,
                        },
                    };
                    let _ = ctx.events.unbounded_send(super::Ev::Pickers(ev));
                });
            }
            Cmd::LoadMemes => {
                self.spawn_guarded("load memes", None, async move {
                    match ctx.client.memes().await {
                        Ok(memes) => {
                            let _ = ctx
                                .events
                                .unbounded_send(super::Ev::Pickers(Ev::Memes(memes)));
                        }
                        Err(err) => {
                            if let Some(r) = reason(&ctx.internal, ctx.gen, err) {
                                let _ = ctx
                                    .events
                                    .unbounded_send(super::Ev::Pickers(Ev::MemesFailed(r)));
                            }
                        }
                    }
                });
            }
            Cmd::SendSticker {
                channel_id,
                sticker_id,
                nonce,
            } => {
                self.spawn_guarded("send sticker", None, async move {
                    let message = p::OutgoingMessage {
                        nonce: Some(nonce),
                        sticker_ids: vec![sticker_id],
                        ..Default::default()
                    };
                    if let Err(err) = ctx.client.send(&channel_id, &message).await {
                        ctx.send_failed(err);
                    }
                });
            }
            Cmd::SendMeme {
                channel_id,
                meme_id,
                nonce,
            } => {
                self.spawn_guarded("send meme", None, async move {
                    let sent = ctx
                        .client
                        .send_meme(&channel_id, &meme_id, Some(&nonce))
                        .await;
                    if let Err(err) = sent {
                        ctx.send_failed(err);
                    }
                });
            }
        }
    }
}

/// Writes one edit through the account's single preference writer.
pub async fn edit_preferences(
    client: &p::Client,
    base: &p::SyncedPreferences,
    edit: PrefEdit,
) -> p::Result<p::SyncedPreferences> {
    client
        .update_synced_preferences(base, move |prefs| edit.apply(prefs))
        .await
}

impl Ctx {
    fn gifs(&self, nonce: u64, result: p::Result<Vec<p::Gif>>) {
        let result = match result {
            Ok(gifs) => Ok(gifs),
            Err(err) => match reason(&self.internal, self.gen, err) {
                Some(r) => Err(r),
                None => return,
            },
        };
        let _ = self
            .events
            .unbounded_send(super::Ev::Pickers(Ev::Gifs { nonce, result }));
    }

    fn send_failed(&self, err: p::Error) {
        if let Some(r) = reason(&self.internal, self.gen, err) {
            let _ = self
                .events
                .unbounded_send(super::Ev::Pickers(Ev::SendFailed(r)));
        }
    }
}

#[cfg(test)]
mod tests;
