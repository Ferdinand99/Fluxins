// SPDX-License-Identifier: AGPL-3.0-or-later
//! Media: avatars, icons, attachments and embed images from the instance's media proxy,
//! through `inlet-media`, and files saved from it.
//!
//! When a session starts, this domain builds one image client for it and hands it to the
//! UI ([`Ev::Ready`]). The client only fetches from the session's media endpoint (the one
//! pinned with the token): its host is the only entry of the allow list, and the
//! [`ProxyFetcher`] refuses any URL outside the endpoint's path as well. The fetcher is
//! its own HTTP client: no token, no cookies, no redirects. Decoding, limits and the
//! memory cache (bounded in bytes) are `inlet-media`'s.
//!
//! Signed attachment URLs expire. The UI asks for fresh ones at most once per message
//! ([`Cmd::Refresh`], one `GET` of the message through the rate limiter).
//!
//! Saving a file streams it from the media proxy to the path the person picked in the
//! save dialog. The file is never opened, run or revealed afterwards.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::StreamExt;
use gpui::RenderImage;
use inlet_media::{
    DecodedImage, FetchError, FetchResponse, Fetcher, ImageSink, MediaClient, MediaConfig,
};
use inlet_protocol as p;
use url::Url;

use super::{Active, Backend};

#[cfg(any(test, feature = "demo"))]
pub mod png;
use crate::model::{self, Attachment, AttachmentKind, Embed, EmbedKind, EmbedMedia, UserAvatar};

/// Attachments of one message that are converted (Fluxer allows 10).
pub const MAX_ATTACHMENTS: usize = 10;
/// Decoded images the cache keeps, in bytes, counting gpui's copy and its atlas tile.
/// Images on screen are a few MiB; the rest is scroll-back. An image larger than this
/// (a big one in the viewer) is never kept: it lives while it is shown.
pub const MEMORY_CACHE_BYTES: u64 = 64 * 1024 * 1024;
/// Longest URL kept from a message; longer ones are dropped.
const MAX_URL_LEN: usize = 4096;

/// Commands of this domain.
pub enum Cmd {
    /// Ask the server for fresh signed URLs of a message's attachments. The UI sends
    /// it at most once per message.
    Refresh {
        channel_id: String,
        message_id: String,
    },
}

/// Events of this domain.
// The setup is boxed; the other events are small.
pub enum Ev {
    /// A session started: the image client for its media endpoint.
    Ready(Box<Setup>),
    /// The session has no usable media endpoint: avatars show initials, images a
    /// placeholder.
    Unavailable,
    /// Fresh attachment data of a message.
    Refreshed {
        message_id: String,
        attachments: Vec<Attachment>,
    },
    /// The refresh did not work; the attachments stay as they are.
    RefreshFailed { message_id: String },
}

/// What the UI needs to show and save media of one session.
pub struct Setup {
    pub client: MediaClient<GpuiImages>,
    /// Builds media URLs from the session's checked media endpoint.
    pub proxy: p::Media,
    /// The fetcher of `client`, for saving files.
    pub fetcher: Arc<dyn Fetcher>,
    /// The runtime the fetcher and the client run on.
    pub runtime: tokio::runtime::Handle,
    /// Images the cache let go of; the UI frees their textures once nothing shows them.
    pub evicted: Evictions,
}

/// Most bytes (counted like the cache: twice the pixels) that may wait in the eviction
/// queue. A whole cache cleared at once fits.
pub const EVICTION_QUEUE_BYTES: u64 = 2 * MEMORY_CACHE_BYTES;

/// The decoded bytes of a gpui image, counted like the cache (CPU copy and atlas tile).
fn counted_bytes(image: &RenderImage) -> u64 {
    let pixels: u64 = (0..image.frame_count())
        .map(|i| image.as_bytes(i).map_or(0, |b| b.len() as u64))
        .sum();
    pixels.saturating_mul(2)
}

/// The sending half of the eviction queue, inside the image sink. Bounded in bytes, it
/// never blocks (the sink runs on the client's threads) and wakes the UI, which frees
/// the textures right away instead of waiting for the next frame.
pub struct EvictionQueue {
    tx: Mutex<mpsc::Sender<(Arc<RenderImage>, u64)>>,
    wake: Mutex<futures::channel::mpsc::Sender<()>>,
    queued: Arc<AtomicU64>,
    cap: u64,
}

/// The receiving half, on the UI thread.
pub struct Evictions {
    rx: mpsc::Receiver<(Arc<RenderImage>, u64)>,
    queued: Arc<AtomicU64>,
    /// Signals that images are waiting; the UI takes it to drain the queue when signalled.
    pub wake: Option<futures::channel::mpsc::Receiver<()>>,
}

/// A new eviction queue holding at most `cap` counted bytes (one image always fits).
pub fn eviction_queue(cap: u64) -> (EvictionQueue, Evictions) {
    let (tx, rx) = mpsc::channel();
    // One pending wake is enough: the UI drains everything when it wakes.
    let (wake_tx, wake_rx) = futures::channel::mpsc::channel(1);
    let queued = Arc::new(AtomicU64::new(0));
    (
        EvictionQueue {
            tx: Mutex::new(tx),
            wake: Mutex::new(wake_tx),
            queued: queued.clone(),
            cap,
        },
        Evictions {
            rx,
            queued,
            wake: Some(wake_rx),
        },
    )
}

impl EvictionQueue {
    /// Queues an image the cache let go of. Returns `false` when the queue is full: the
    /// image is dropped here (its texture, if it was ever drawn, stays in the atlas
    /// until the window closes, which the bound makes the rare case).
    pub fn push(&self, image: Arc<RenderImage>) -> bool {
        let bytes = counted_bytes(&image);
        let mut now = self.queued.load(Ordering::Acquire);
        let reserved = loop {
            if now != 0 && now.saturating_add(bytes) > self.cap {
                break false;
            }
            match self.queued.compare_exchange_weak(
                now,
                now.saturating_add(bytes),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break true,
                Err(actual) => now = actual,
            }
        };
        if !reserved {
            log::warn!("image eviction queue full; dropping an evicted image");
            return false;
        }
        let sent = self
            .tx
            .lock()
            .is_ok_and(|tx| tx.send((image, bytes)).is_ok());
        if !sent {
            // The UI is gone; nothing will free it, nothing needs to.
            self.queued.fetch_sub(bytes, Ordering::AcqRel);
            return false;
        }
        if let Ok(mut wake) = self.wake.lock() {
            // Full means a wake is already pending.
            let _ = wake.try_send(());
        }
        true
    }
}

impl Evictions {
    /// Takes every waiting image off the queue.
    pub fn drain(&self) -> Vec<Arc<RenderImage>> {
        self.rx
            .try_iter()
            .map(|(image, bytes)| {
                self.queued.fetch_sub(bytes, Ordering::AcqRel);
                image
            })
            .collect()
    }

    /// Counted bytes waiting in the queue.
    #[cfg(test)]
    pub fn queued_bytes(&self) -> u64 {
        self.queued.load(Ordering::Acquire)
    }
}

/// Turns decoded images into gpui images and hands evicted ones to the UI thread.
pub struct GpuiImages {
    evicted: EvictionQueue,
}

impl ImageSink for GpuiImages {
    type Handle = Arc<RenderImage>;

    fn make(&self, image: DecodedImage) -> (Self::Handle, u64) {
        // gpui keeps the pixels once on the CPU side and once more in its atlas.
        let bytes = image.byte_size().saturating_mul(2);
        (Arc::new(RenderImage::new(image.into_gpui_frames())), bytes)
    }

    fn evicted(&self, handle: Self::Handle) {
        // Bounded and never blocks.
        self.evicted.push(handle);
    }
}

/// The settings of the image client for one media endpoint.
pub fn config(proxy: &p::Media) -> MediaConfig {
    let host = Url::parse(proxy.base())
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    MediaConfig {
        // Only the media endpoint. Embeds' third-party URLs never get here, and if one
        // did, the client would refuse it.
        allowed_hosts: host.into_iter().collect(),
        memory_cache_bytes: MEMORY_CACHE_BYTES,
        // Nothing on disk: images from DMs stay off the disk, and signed URLs would
        // store the same image again after every refresh.
        disk_cache: None,
        ..MediaConfig::default()
    }
}

/// Builds the image client for a media endpoint. Runs on the backend runtime.
pub fn setup(proxy: p::Media, fetcher: Arc<dyn Fetcher>) -> Setup {
    let (queue, evicted) = eviction_queue(EVICTION_QUEUE_BYTES);
    let runtime = tokio::runtime::Handle::current();
    let client = MediaClient::new(
        runtime.clone(),
        fetcher.clone(),
        GpuiImages { evicted: queue },
        config(&proxy),
    );
    Setup {
        client,
        proxy,
        fetcher,
        runtime,
        evicted,
    }
}

/// A session started: hand the UI an image client for its pinned media endpoint.
pub(super) fn session_started(b: &Backend, session: &p::Session) {
    let ready = session.instance().media().and_then(|proxy| {
        let fetcher = ProxyFetcher::new(&proxy)?;
        Some(setup(proxy, Arc::new(fetcher)))
    });
    match ready {
        Some(setup) => b.emit(Ev::Ready(Box::new(setup))),
        None => b.emit(Ev::Unavailable),
    }
}

/// The demo session started: images come from built-in pictures, never the network.
#[cfg(feature = "demo")]
pub(super) fn demo_started(b: &Backend) {
    let (proxy, fetcher) = crate::demo::media::proxy_and_fetcher();
    b.emit(Ev::Ready(Box::new(setup(proxy, fetcher))));
}

pub(super) fn handle(b: &mut Backend, cmd: Cmd) {
    match cmd {
        Cmd::Refresh {
            channel_id,
            message_id,
        } => match &b.active {
            Some(Active::Real(r)) => {
                let session = r.session.clone();
                let events = b.events.clone();
                b.spawn_guarded("refresh attachment urls", None, async move {
                    // One GET of the message, through the session's rate limiter.
                    let ev = match session.client().message(&channel_id, &message_id).await {
                        Ok(m) => Ev::Refreshed {
                            message_id,
                            attachments: m
                                .attachments
                                .iter()
                                .take(MAX_ATTACHMENTS)
                                .map(to_attachment)
                                .collect(),
                        },
                        Err(err) => {
                            log::debug!("refreshing attachment URLs failed: {err}");
                            Ev::RefreshFailed { message_id }
                        }
                    };
                    let _ = events.unbounded_send(ev);
                });
            }
            #[cfg(feature = "demo")]
            Some(Active::Demo(_)) => {
                b.emit(crate::demo::media::refreshed(&channel_id, &message_id));
            }
            None => {}
        },
    }
}

// ---- Fetching from the media proxy -----------------------------------------------------

/// Fetches from one media endpoint: no token, no cookies, no redirects, and nothing
/// outside the endpoint's origin and path.
pub struct ProxyFetcher {
    http: reqwest::Client,
    base: Url,
}

impl ProxyFetcher {
    pub fn new(proxy: &p::Media) -> Option<Self> {
        let base = Url::parse(proxy.base()).ok()?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("Inlet/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            // A redirect could lead anywhere; the media proxy answers directly.
            .redirect(reqwest::redirect::Policy::none())
            .use_rustls_tls()
            .build()
            .ok()?;
        Some(Self { http, base })
    }
}

/// Whether `url` is on the media endpoint `base`: same scheme, host and port, a path
/// under the endpoint's path, and no user name or password.
pub fn on_media_base(base: &Url, url: &Url) -> bool {
    if url.origin() != base.origin() || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let prefix = format!("{}/", base.path().trim_end_matches('/'));
    url.path().starts_with(&prefix) && !url.path().split('/').any(|s| s == "..")
}

impl Fetcher for ProxyFetcher {
    fn fetch<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<FetchResponse, FetchError>> {
        Box::pin(async move {
            if !on_media_base(&self.base, url) {
                return Err(FetchError::Network("not on the media endpoint".into()));
            }
            let resp = self
                .http
                .get(url.clone())
                .send()
                .await
                .map_err(|e| FetchError::network(e.without_url()))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(FetchError::Status(status.as_u16()));
            }
            let length = resp.content_length();
            let body = futures::stream::unfold(Some(resp), |resp| async move {
                let mut resp = resp?;
                match resp.chunk().await {
                    Ok(Some(chunk)) => Some((Ok(chunk), Some(resp))),
                    Ok(None) => None,
                    Err(e) => Some((Err(FetchError::network(e.without_url())), None)),
                }
            });
            Ok(FetchResponse::new(length, Box::pin(body)))
        })
    }
}

// ---- Saving ------------------------------------------------------------------------------

/// Why a save failed, for the notice.
#[derive(Debug)]
pub enum SaveError {
    Fetch(FetchError),
    TooLarge,
    Write(std::io::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Fetch(e) => write!(f, "{e}"),
            SaveError::TooLarge => f.write_str(&t!("media-save-too-large")),
            SaveError::Write(e) => write!(f, "{e}"),
        }
    }
}

/// Most bytes a save may write: the announced size with some slack, or 512 MiB when
/// the size is unknown, never more than 2 GiB.
pub fn save_limit(announced: u64) -> u64 {
    const MIB: u64 = 1024 * 1024;
    if announced == 0 {
        512 * MIB
    } else {
        announced.saturating_add(MIB).min(2048 * MIB)
    }
}

/// Streams `url` into `path` (written to a temporary file next to it first, then
/// renamed). Returns the bytes written. The file is only written: never opened, run
/// or shown in a file manager.
pub async fn save_to(
    fetcher: Arc<dyn Fetcher>,
    url: Url,
    path: PathBuf,
    max_bytes: u64,
) -> Result<u64, SaveError> {
    use tokio::io::AsyncWriteExt;

    let resp = fetcher.fetch(&url).await.map_err(SaveError::Fetch)?;
    if resp.content_length.is_some_and(|n| n > max_bytes) {
        return Err(SaveError::TooLarge);
    }
    let (part, mut file) = create_part(&path, part_seeds())
        .await
        .map_err(SaveError::Write)?;
    let mut body = resp.body;
    let mut written: u64 = 0;
    let result: Result<(), SaveError> = async {
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(SaveError::Fetch)?;
            written = written.saturating_add(chunk.len() as u64);
            if written > max_bytes {
                return Err(SaveError::TooLarge);
            }
            file.write_all(&chunk).await.map_err(SaveError::Write)?;
        }
        file.flush().await.map_err(SaveError::Write)?;
        file.sync_all().await.map_err(SaveError::Write)?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(err) = result {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(err);
    }
    if let Err(err) = tokio::fs::rename(&part, &path).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(SaveError::Write(err));
    }
    Ok(written)
}

/// Names tried for the temporary file of one save.
const PART_ATTEMPTS: usize = 16;

/// `dir/.name.<seed>.part`: a temporary file a save writes before it is complete.
fn part_path(path: &Path, seed: u64) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    path.with_file_name(format!(".{name}.{seed:016x}.part"))
}

/// Seeds for temporary names nobody can guess ahead: the time, the process and a counter.
fn part_seeds() -> impl Iterator<Item = u64> {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // A fresh random key per call (std seeds it from the system's randomness).
    let keys = std::collections::hash_map::RandomState::new();
    (0..PART_ATTEMPTS as u64).map(move |attempt| {
        let mut h = keys.build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u32(std::process::id());
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        h.write_u64(attempt);
        h.finish()
    })
}

/// Creates a new temporary file next to `path`, trying a name per seed. Each name is
/// created exclusively (`create_new`): an existing file is never truncated, and a
/// symbolic link in its place, even a dangling one, is never followed.
async fn create_part(
    path: &Path,
    seeds: impl IntoIterator<Item = u64>,
) -> std::io::Result<(PathBuf, tokio::fs::File)> {
    for seed in seeds {
        let part = part_path(path, seed);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .await
        {
            Ok(file) => return Ok((part, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free name for the temporary file",
    ))
}

/// A file name to suggest in the save dialog: no path separators, no characters
/// Windows refuses, no leading dots, at most 120 characters.
pub fn suggested_file_name(name: &str) -> String {
    let cleaned: String = model::clean_name(name)
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .take(120)
        .collect();
    let trimmed = cleaned
        .trim()
        .trim_start_matches(['.', '_'])
        .trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "download".into()
    } else {
        trimmed.to_string()
    }
}

// ---- Conversion ----------------------------------------------------------------------------

fn url_field(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= MAX_URL_LEN)
        .map(str::to_owned)
}

fn rfc3339_ms(s: Option<&str>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s?)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// The avatar of a user, when they have one.
pub fn user_avatar(u: &p::User) -> Option<UserAvatar> {
    let hash = u
        .avatar
        .as_deref()
        .filter(|h| !h.is_empty() && h.len() <= 128)?;
    Some(UserAvatar {
        user_id: u.id.clone(),
        hash: hash.to_string(),
    })
}

/// Converts an attachment.
pub fn to_attachment(a: &p::Attachment) -> Attachment {
    let ext = extension(&a.filename);
    let kind = if a.is_image() {
        AttachmentKind::Image
    } else if a.is_video() {
        AttachmentKind::Video
    } else if a.is_audio() {
        AttachmentKind::Audio
    } else if a.content_type.is_none() {
        // No content type: go by the extension.
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" => AttachmentKind::Image,
            "mp4" | "webm" | "mov" => AttachmentKind::Video,
            "mp3" | "ogg" | "wav" | "flac" | "m4a" | "opus" => AttachmentKind::Audio,
            _ => AttachmentKind::File,
        }
    } else {
        AttachmentKind::File
    };
    let content_type = a
        .content_type
        .as_deref()
        .filter(|t| t.len() <= 100)
        .map(str::to_owned);
    let gif = content_type.as_deref() == Some("image/gif") || ext == "gif";
    Attachment {
        id: a.id.clone(),
        filename: model::clean_name(&a.filename),
        url: url_field(a.url.as_deref()),
        proxy_url: url_field(a.proxy_url.as_deref()),
        content_type,
        size: a.size,
        width: a.width.filter(|w| *w > 0),
        height: a.height.filter(|h| *h > 0),
        kind,
        spoiler: a.is_spoiler() || a.filename.starts_with("SPOILER_"),
        explicit: a.is_explicit(),
        animated: a.is_animated() || gif,
        duration_secs: a.duration_secs(),
        alt: a
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .map(|d| model::snippet(d, 300)),
        expires_at_ms: rfc3339_ms(a.expires_at()),
        expired: a.expired(),
    }
}

fn embed_media(m: &p::models::EmbedMedia, gifv: bool) -> Option<EmbedMedia> {
    let flags = m.flags.unwrap_or(0);
    let size = |v: Option<u64>| {
        v.filter(|v| *v > 0)
            .map(|v| v.min(u64::from(u32::MAX)) as u32)
    };
    let media = EmbedMedia {
        url: url_field(Some(&m.url)).unwrap_or_default(),
        proxy_url: url_field(m.proxy_url.as_deref()),
        width: size(m.width),
        height: size(m.height),
        animated: gifv
            || flags & p::models::attachment_flags::IS_ANIMATED != 0
            || m.content_type.as_deref() == Some("image/gif"),
        explicit: flags & p::models::attachment_flags::CONTAINS_EXPLICIT_MEDIA != 0,
    };
    (!media.url.is_empty() || media.proxy_url.is_some()).then_some(media)
}

/// Fills the kind, links, icons, provider, time and media of an embed from its JSON.
pub fn fill_embed(v: &serde_json::Value, embed: &mut Embed) {
    let Ok(e) = serde_json::from_value::<p::Embed>(v.clone()) else {
        return;
    };
    embed.kind = match e.kind.as_str() {
        "link" => EmbedKind::Link,
        "article" => EmbedKind::Article,
        "image" => EmbedKind::Image,
        "gifv" => EmbedKind::Gifv,
        "video" => EmbedKind::Video,
        "bluesky" => EmbedKind::Bluesky,
        _ => EmbedKind::Rich,
    };
    let gifv = embed.kind == EmbedKind::Gifv;
    if let Some(a) = &e.author {
        embed.author_url = url_field(a.url.as_deref());
        embed.author_icon = url_field(a.proxy_icon_url.as_deref());
    }
    embed.provider = e
        .provider
        .as_ref()
        .map(|p| p.name.trim())
        .filter(|n| !n.is_empty())
        .map(|n| model::snippet(n, 256));
    embed.footer_icon = e
        .footer
        .as_ref()
        .and_then(|f| url_field(f.proxy_icon_url.as_deref()));
    embed.timestamp_ms = rfc3339_ms(e.timestamp.as_deref());
    embed.thumbnail = e.thumbnail.as_ref().and_then(|m| embed_media(m, gifv));
    embed.image = e.image.as_ref().and_then(|m| embed_media(m, gifv));
    embed.video = e.video.as_ref().and_then(|m| embed_media(m, gifv));
    embed.nsfw = e.nsfw;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use inlet_media::{Fit, MediaError, MediaKind, MediaRequest};

    use super::*;
    use crate::ui::media::requests;

    /// Records every URL it is asked for and answers with `body`.
    struct Recording {
        seen: Mutex<Vec<String>>,
        body: Bytes,
    }

    impl Fetcher for Recording {
        fn fetch<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<FetchResponse, FetchError>> {
            self.seen.lock().unwrap().push(url.to_string());
            let body = self.body.clone();
            Box::pin(async move { Ok(FetchResponse::from_bytes(body)) })
        }
    }

    /// Counts the images it is asked to make.
    #[derive(Default)]
    struct Counting(Arc<AtomicUsize>);

    impl ImageSink for Counting {
        type Handle = Arc<DecodedImage>;
        fn make(&self, image: DecodedImage) -> (Self::Handle, u64) {
            self.0.fetch_add(1, Ordering::SeqCst);
            let bytes = image.byte_size();
            (Arc::new(image), bytes)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn temporary_file_is_new_and_never_follows_a_link() {
        let dir = std::env::temp_dir().join(format!("inlet-part-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("photo.png");

        // Someone placed a file where the first name would go, and (on Unix) a link to
        // a file of theirs where the second would go.
        let taken = part_path(&target, 1);
        std::fs::write(&taken, b"keep me").unwrap();
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, b"victim").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, part_path(&target, 2)).unwrap();
        #[cfg(not(unix))]
        std::fs::write(part_path(&target, 2), b"keep me").unwrap();

        let (part, file) = create_part(&target, [1, 2, 3]).await.unwrap();
        drop(file);
        assert_eq!(part, part_path(&target, 3));
        assert_eq!(std::fs::read(&taken).unwrap(), b"keep me");
        assert_eq!(std::fs::read(&victim).unwrap(), b"victim");
        // Every name taken: refused, nothing overwritten.
        assert!(create_part(&target, [1, 2]).await.is_err());
        assert_eq!(std::fs::read(&taken).unwrap(), b"keep me");

        // Two saves never share a temporary name.
        let a: Vec<u64> = part_seeds().collect();
        let b: Vec<u64> = part_seeds().collect();
        assert_eq!(a.len(), PART_ATTEMPTS);
        assert!(a.iter().all(|s| !b.contains(s)));

        // A real save leaves a stale `.part` of an earlier save alone.
        let stale = dir.join(".photo.png.part");
        std::fs::write(&stale, b"stale").unwrap();
        let fetcher = Arc::new(Recording {
            seen: Mutex::default(),
            body: Bytes::from_static(b"png bytes"),
        });
        let url = Url::parse("https://media.example.org/media/attachments/1/2/p.png").unwrap();
        save_to(fetcher, url, target.clone(), save_limit(0))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"png bytes");
        assert_eq!(std::fs::read(&stale).unwrap(), b"stale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn render_image(side: u32) -> Arc<RenderImage> {
        let limits = inlet_media::KindLimits::for_kind(MediaKind::Attachment);
        let decoded = inlet_media::decode(
            &png::solid(side, side, [9, 9, 9, 255]),
            Fit::square(side),
            &limits,
            &inlet_media::DecodeControl::with_budget(Duration::from_secs(5)),
        )
        .unwrap();
        Arc::new(RenderImage::new(decoded.into_gpui_frames()))
    }

    #[test]
    fn evicted_images_are_bounded_and_released() {
        let a = render_image(16);
        let bytes = counted_bytes(&a);
        assert_eq!(bytes, 16 * 16 * 4 * 2);
        // Room for two such images.
        let (queue, mut evicted) = eviction_queue(bytes * 2);
        let mut wake = evicted.wake.take().unwrap();

        assert!(queue.push(a.clone()));
        assert!(queue.push(render_image(16)));
        // Full: the third is refused and let go at once, not kept.
        let c = render_image(16);
        assert!(!queue.push(c.clone()));
        assert_eq!(Arc::strong_count(&c), 1);
        assert_eq!(evicted.queued_bytes(), bytes * 2);
        // The UI was woken (once is enough).
        use futures::FutureExt;
        assert_eq!(wake.next().now_or_never(), Some(Some(())));

        // Draining hands the images over and empties the queue.
        let drained = evicted.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(evicted.queued_bytes(), 0);
        assert_eq!(Arc::strong_count(&a), 2);
        drop(drained);
        assert_eq!(Arc::strong_count(&a), 1);

        // One image larger than the whole bound still fits an empty queue (the viewer's
        // big image), and nothing else fits beside it.
        let big = render_image(64);
        assert!(queue.push(big));
        assert!(!queue.push(render_image(16)));
        assert_eq!(evicted.drain().len(), 1);

        // The UI gone: nothing is kept for it.
        drop(evicted);
        let d = render_image(16);
        assert!(!queue.push(d.clone()));
        assert_eq!(Arc::strong_count(&d), 1);
    }

    fn proxy() -> p::Media {
        p::Media::new("https://media.example.org/media").unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_media_proxy_hosts_are_fetched() {
        let fetcher = Arc::new(Recording {
            seen: Mutex::default(),
            body: Bytes::from(png::solid(8, 8, [200, 80, 40, 255])),
        });
        let proxy = proxy();
        let client = MediaClient::new(
            tokio::runtime::Handle::current(),
            fetcher.clone(),
            Counting::default(),
            config(&proxy),
        );

        // An embed whose image is on a third-party host, with a proxy URL elsewhere:
        // the UI builds no request for it at all.
        let evil = EmbedMedia {
            url: "https://tracker.example.net/pixel.png".into(),
            proxy_url: Some("https://tracker.example.net/media/external/x.png".into()),
            ..Default::default()
        };
        assert!(requests::embed_image(&proxy, &evil, (400., 300.), 1.0).is_none());
        // A raw URL from a message never becomes a request either.
        let att = Attachment {
            url: Some("https://cdn.elsewhere.example/attachments/1/2/a.png".into()),
            proxy_url: None,
            kind: AttachmentKind::Image,
            width: Some(10),
            height: Some(10),
            ..Default::default()
        };
        assert!(requests::attachment_preview(&proxy, &att, 1.0).is_none());

        // The client refuses other hosts even if a request were made by hand.
        for bad in [
            "https://tracker.example.net/media/x.png",
            "http://media.example.org/media/avatars/1/a.webp",
            "https://user:pw@media.example.org/media/avatars/1/a.webp",
        ] {
            let req =
                MediaRequest::new(Url::parse(bad).unwrap(), MediaKind::Avatar, Fit::square(40));
            let err = client.load(req).await.expect_err(bad);
            assert!(
                matches!(
                    err,
                    MediaError::HostNotAllowed(_) | MediaError::InvalidUrl(_)
                ),
                "{bad}: {err:?}"
            );
        }

        // Proxy URLs load.
        let good = EmbedMedia {
            url: "https://tracker.example.net/pixel.png".into(),
            proxy_url: Some(
                "https://media.example.org/media/external/sig/https/tracker.example.net/pixel.png"
                    .into(),
            ),
            width: Some(8),
            height: Some(8),
            ..Default::default()
        };
        let req = requests::embed_image(&proxy, &good, (400., 300.), 1.0).unwrap();
        client.load(req).await.expect("a proxy URL loads");
        let avatar = UserAvatar {
            user_id: "5".into(),
            hash: "abc".into(),
        };
        let req = requests::avatar(&proxy, &avatar, 40., 1.0, false).unwrap();
        client.load(req).await.expect("an avatar loads");

        let seen = fetcher.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen
            .iter()
            .all(|u| u.starts_with("https://media.example.org/media/")));

        // The fetcher itself refuses anything outside the endpoint's path.
        let base = Url::parse(proxy.base()).unwrap();
        for (url, ok) in [
            ("https://media.example.org/media/avatars/1/a.webp", true),
            ("https://media.example.org/other/avatars/1/a.webp", false),
            ("https://media.example.org/mediax/a.webp", false),
            ("https://media.example.org:8443/media/a.webp", false),
            ("http://media.example.org/media/a.webp", false),
            ("https://evil.example/media/a.webp", false),
        ] {
            assert_eq!(on_media_base(&base, &Url::parse(url).unwrap()), ok, "{url}");
        }
        let real = ProxyFetcher::new(&proxy).unwrap();
        let outside = Url::parse("https://evil.example/media/a.webp").unwrap();
        assert!(matches!(
            real.fetch(&outside).await,
            Err(FetchError::Network(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn huge_image_dimensions_are_refused_before_decode() {
        let made = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(Recording {
            seen: Mutex::default(),
            body: Bytes::from(png::claiming(100_000, 100_000)),
        });
        let proxy = proxy();
        let client = MediaClient::new(
            tokio::runtime::Handle::current(),
            fetcher.clone(),
            Counting(made.clone()),
            config(&proxy),
        );
        // The attachment says it is huge: the preview asks the proxy for 350 x 350 at
        // most (550 x 350 box), and the decode limits shrink to match.
        let att = Attachment {
            id: "2".into(),
            proxy_url: Some("https://media.example.org/media/attachments/1/2/huge.png".into()),
            kind: AttachmentKind::Image,
            width: Some(100_000),
            height: Some(100_000),
            ..Default::default()
        };
        let req = requests::attachment_preview(&proxy, &att, 1.0).unwrap();
        let q: Vec<(String, String)> = req.url.query_pairs().into_owned().collect();
        assert!(q.contains(&("width".into(), "350".into())), "{q:?}");
        assert!(q.contains(&("height".into(), "350".into())), "{q:?}");
        let err = client.load(req).await.expect_err("refused");
        assert!(matches!(err, MediaError::BadDimensions { .. }), "{err:?}");
        // The header was enough: nothing was decoded, no image was made.
        assert_eq!(made.load(Ordering::SeqCst), 0);
        assert_eq!(client.stats().decodes, 0);
        assert_eq!(client.stats().memory_entries, 0);

        // Even a request without a proxy size keeps the kind's own limits.
        let raw = MediaRequest::new(
            Url::parse("https://media.example.org/media/attachments/1/2/huge2.png").unwrap(),
            MediaKind::Attachment,
            Fit::PREVIEW,
        );
        assert!(matches!(
            client.load(raw).await,
            Err(MediaError::BadDimensions { .. })
        ));
        assert_eq!(made.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_never_opens_the_file() {
        let dir = std::env::temp_dir().join(format!("inlet-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = b"#!/bin/sh\necho this must never run\n".to_vec();
        let fetcher = Arc::new(Recording {
            seen: Mutex::default(),
            body: Bytes::from(payload.clone()),
        });
        let name = suggested_file_name("../../evil:name?.sh");
        assert!(!name.contains('/') && !name.contains('\\') && !name.starts_with('.'));
        let path = dir.join(&name);
        let url = Url::parse("https://media.example.org/media/attachments/1/2/evil.sh").unwrap();
        let written = save_to(fetcher.clone(), url.clone(), path.clone(), save_limit(0))
            .await
            .unwrap();
        assert_eq!(written, payload.len() as u64);
        assert_eq!(std::fs::read(&path).unwrap(), payload);
        // Only the chosen file is left, and it cannot be run.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0, "mode {mode:o}");
        }
        // Too large: nothing is left behind.
        let small = dir.join("small.bin");
        assert!(matches!(
            save_to(fetcher, url, small.clone(), 4).await,
            Err(SaveError::TooLarge)
        ));
        assert!(!small.exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);

        // Nothing in the media code opens, runs or reveals a local file. Opening the
        // original goes to the browser through the link check (`open_link`).
        let sources = [
            include_str!("mod.rs"),
            include_str!("../../ui/media/mod.rs"),
            include_str!("../../ui/media/attachment.rs"),
            include_str!("../../ui/media/viewer.rs"),
            include_str!("../../ui/media/video_thumb.rs"),
            include_str!("../../ui/media/audio_file.rs"),
            include_str!("../../ui/media/image.rs"),
            include_str!("../../ui/media/embed.rs"),
            include_str!("../../ui/media/avatar.rs"),
            include_str!("../../ui/media/requests.rs"),
        ];
        for src in sources {
            let code = src.split("#[cfg(test)]").next().unwrap_or(src);
            for call in [
                "open_with_system",
                "reveal_path",
                ".open_url(",
                "process::Command",
                "open::",
            ] {
                assert!(!code.contains(call), "media code calls {call}");
            }
        }
    }
}
