// SPDX-License-Identifier: AGPL-3.0-or-later
//! Files attached to the message being written: the plus button and the file dialog,
//! files dropped on the composer, images pasted into it, and the tray of tiles above
//! the box with a preview, a spoiler switch, alt text and a remove button per file.
//!
//! Nothing is attached without the person: a file comes only from the dialog, a drop
//! or a paste. Names are cleaned with the protocol's `safe_filename` when a file is
//! attached. A pasted image stays in memory (at most [`MAX_PASTED_BYTES`] for all
//! channels together) and is never written to disk. Files from disk are only looked at
//! (size, and the bytes of a preview) until they are sent.
//!
//! Previews are decoded through `inlet-media`'s checks (encoded size, declared size and
//! the memory a decode needs, all before decoding), one at a time, only for images of
//! at most 12.6 MP and a decode of at most 64 MiB, and as a still (the first frame).
//! Larger images show their file tile.
//!
//! Pasted BMP and TIFF images (Windows screenshots come as BMP) are turned into PNG in
//! memory, with limits on their size; formats Inlet cannot send as images are refused
//! with a message.
//!
//! With metadata removal on, each photo is checked when it is attached (from its bytes,
//! not its name). A photo whose metadata cannot be removed (an unreadable file, HEIC,
//! AVIF, TIFF) shows a warning and is not sent until the person removes it or chooses
//! "Send without removing" for that file.
//!
//! Each channel keeps its own attachments until the message is sent or they are removed;
//! sign-out drops them all.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use gpui::{
    div, img, prelude::*, px, AnyElement, ClipboardEntry, Context, Entity, ExternalPaths,
    Focusable, FontWeight, ObjectFit, PathPromptOptions, RenderImage, SharedString, Subscription,
    Window,
};
use inlet_editor::{Editor, EditorEvent};
use inlet_media::{DecodeControl, DecodedImage, Fit, KindLimits, MediaKind};
use inlet_protocol as p;

use super::drafts::Draft;
use super::editor::{editor_style, ComposerView};
use crate::backend::composer::{exif, FileToSend, Limits, Refused, Source};
use crate::theme;

const MIB: u64 = 1024 * 1024;
/// Pasted images held in memory, all channels together.
pub const MAX_PASTED_BYTES: u64 = 64 * MIB;
/// A tile of the tray (Fluxer's 200 px).
const TILE: f32 = 200.;
/// The preview inside a tile.
const PREVIEW_H: f32 = 148.;
/// The longest alt text (Fluxer allows 4096; the field keeps it short).
const ALT_MAX: usize = 1024;
/// Previews are decoded at up to twice the tile size, for high-density screens.
const PREVIEW_FIT: Fit = Fit::Contain {
    max_width: 368,
    max_height: 296,
};
/// Decoded bytes one animated preview may take (only its first frame is kept).
const PREVIEW_ANIMATION_BYTES: u64 = 8 * MIB;
/// The largest image a preview is decoded for (4096 x 3072): a decode never works on
/// a full 24 MP image.
const PREVIEW_MAX_PIXELS: u64 = 4096 * 3072;
/// The most memory one preview decode may need, by `inlet-media`'s estimate.
const PREVIEW_MEMORY: u64 = 64 * MIB;
/// The largest photo whose metadata is checked when it is attached; a larger one is
/// checked when it is sent (and refused then if it cannot be cleaned).
const CHECK_MAX_BYTES: u64 = 64 * MIB;
/// Most pasted pixels turned into PNG (a 4K screen is 8.3 MP).
const PASTE_MAX_PIXELS: u64 = 36_000_000;
/// Uploads remembered for a draft when the account is switched.
const MAX_IN_FLIGHT: usize = 32;

/// One decode at a time for previews and metadata checks, across all composers.
static PREVIEW_LANE: Mutex<()> = Mutex::new(());

/// What metadata removal will do with a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metadata {
    /// Not a photo type with metadata, or nothing to remove.
    Clean,
    /// Still being looked at.
    Checking,
    /// It will be removed (or checked when sent, for a large photo).
    Removable,
    /// It cannot be removed: not sent unless the person chooses to.
    Stuck,
}

/// A file in the tray.
pub struct Staged {
    pub key: u64,
    pub file: FileToSend,
    pub preview: Option<Arc<RenderImage>>,
    pub metadata: Metadata,
}

/// A message with files on its way, kept so its text can become a draft again when
/// the account is switched before it is sent.
pub struct InFlight {
    pub nonce: String,
    pub channel_id: String,
    pub draft: Option<Draft>,
    pub names: Vec<String>,
}

/// The alt text field of one file.
pub struct AltEdit {
    pub channel: String,
    pub key: u64,
    pub editor: Entity<Editor>,
    _events: Subscription,
}

/// The attachments of every channel, and what applies to them.
pub struct Attachments {
    by_channel: HashMap<String, Vec<Staged>>,
    next_key: u64,
    /// The account's limits (Fluxer's defaults until they are known).
    pub limits: Limits,
    /// Remove photo metadata before upload. On by default.
    // Deviation: Fluxer has no such switch (its server strips metadata after upload);
    // Inlet strips before the bytes leave the machine and shows the switch in the tray.
    pub strip_exif: bool,
    pub alt: Option<AltEdit>,
    /// Messages with files not sent yet (on their way or failed).
    pub in_flight: Vec<InFlight>,
    /// Bumped when everything is dropped (sign-out): late results are ignored.
    epoch: u64,
    /// The demo scene's tray was filled.
    #[cfg(feature = "demo")]
    demo_shown: bool,
}

impl Default for Attachments {
    fn default() -> Self {
        Attachments {
            by_channel: HashMap::new(),
            next_key: 1,
            limits: Limits::default(),
            strip_exif: true,
            alt: None,
            in_flight: Vec::new(),
            epoch: 0,
            #[cfg(feature = "demo")]
            demo_shown: false,
        }
    }
}

/// The media type for a file name, from its extension. The server derives its own; this
/// one goes with the bytes.
pub fn content_type_of(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "json" => "application/json",
        "txt" | "log" | "md" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Whether a preview is decoded for this media type.
fn previewable(content_type: &str) -> bool {
    matches!(
        content_type,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// An image pasted from the clipboard, as a file to attach: its bytes in memory, named
/// `image` with the extension of its format. BMP and TIFF are turned into PNG in
/// memory. `Err` says why an image cannot be attached (SVG and the like, or a picture
/// that is too large or cannot be read).
pub fn pasted_file(image: &gpui::Image) -> Result<FileToSend, String> {
    let bytes = image.bytes();
    if bytes.is_empty() {
        return Err(t!("composer-paste-unreadable"));
    }
    let (ext, data) = match image.format() {
        gpui::ImageFormat::Png => ("png", Bytes::copy_from_slice(bytes)),
        gpui::ImageFormat::Jpeg => ("jpg", Bytes::copy_from_slice(bytes)),
        gpui::ImageFormat::Webp => ("webp", Bytes::copy_from_slice(bytes)),
        gpui::ImageFormat::Gif => ("gif", Bytes::copy_from_slice(bytes)),
        gpui::ImageFormat::Bmp => ("png", to_png(bytes, image::ImageFormat::Bmp)?),
        gpui::ImageFormat::Tiff => ("png", to_png(bytes, image::ImageFormat::Tiff)?),
        _ => return Err(t!("composer-paste-unsupported")),
    };
    let name = format!("image.{ext}");
    Ok(FileToSend {
        content_type: content_type_of(&name).to_owned(),
        name,
        size: data.len() as u64,
        spoiler: false,
        description: None,
        keep_metadata: false,
        source: Source::Memory(data),
    })
}

/// A BMP or TIFF image as PNG, in memory, with limits on its size and on the memory the
/// decoder may take.
fn to_png(bytes: &[u8], format: image::ImageFormat) -> Result<Bytes, String> {
    let unreadable = || t!("composer-paste-unreadable");
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(PASTE_MAX_PIXELS * 4 * 2);
    reader.limits(limits);
    let (w, h) = reader.into_dimensions().map_err(|_| unreadable())?;
    if u64::from(w) * u64::from(h) > PASTE_MAX_PIXELS {
        return Err(t!("composer-paste-too-large"));
    }
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    reader.limits({
        let mut l = image::Limits::default();
        l.max_alloc = Some(PASTE_MAX_PIXELS * 4 * 2);
        l
    });
    let picture = reader.decode().map_err(|_| unreadable())?;
    let mut out = std::io::Cursor::new(Vec::new());
    picture
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|_| unreadable())?;
    Ok(Bytes::from(out.into_inner()))
}

/// A file the person picked or dropped: its cleaned name and size. `Err` with the name
/// for something that is not a regular file (a folder, a device).
fn picked_file(path: &Path) -> Result<FileToSend, String> {
    let shown = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = p::safe_filename(&shown);
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(FileToSend {
            content_type: content_type_of(&name).to_owned(),
            size: meta.len(),
            name,
            spoiler: false,
            description: None,
            keep_metadata: false,
            source: Source::Path(path.to_owned()),
        }),
        _ => Err(name),
    }
}

/// Up to `max` bytes of a file, or `None` when it is larger or unreadable.
fn read_bounded(path: &Path, max: usize) -> Option<Bytes> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > max as u64 {
        return None;
    }
    let mut buf = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut buf).ok()?;
    (buf.len() <= max).then(|| Bytes::from(buf))
}

/// The first bytes of a file, to tell its type.
fn head_of(source: &Source) -> Option<Bytes> {
    match source {
        Source::Memory(b) => Some(b.slice(..b.len().min(32))),
        Source::Path(path) => {
            let mut head = Vec::with_capacity(32);
            std::fs::File::open(path)
                .ok()?
                .take(32)
                .read_to_end(&mut head)
                .ok()?;
            Some(Bytes::from(head))
        }
    }
}

/// What removing metadata would do with a file: one at a time with the previews.
pub fn check_metadata(source: &Source, size: u64) -> Metadata {
    let Some(head) = head_of(source) else {
        return Metadata::Stuck;
    };
    match exif::sniff(&head) {
        exif::Kind::Other => Metadata::Clean,
        exif::Kind::Heif | exif::Kind::Tiff => Metadata::Stuck,
        exif::Kind::Jpeg | exif::Kind::Png | exif::Kind::Webp if size > CHECK_MAX_BYTES => {
            Metadata::Removable
        }
        _ => {
            let _lane = PREVIEW_LANE.lock().unwrap_or_else(|e| e.into_inner());
            let bytes = match source {
                Source::Memory(b) => Some(b.clone()),
                Source::Path(path) => read_bounded(path, CHECK_MAX_BYTES as usize),
            };
            match bytes.map(|b| exif::plan(&b, &AtomicBool::new(false))) {
                Some(Ok(None)) => Metadata::Clean,
                Some(Ok(Some(_))) => Metadata::Removable,
                _ => Metadata::Stuck,
            }
        }
    }
}

/// Decodes a preview through `inlet-media`'s checks: the encoded size first, then the
/// declared size and the memory a decode needs, then a decode with a time budget. One
/// at a time; a still of the first frame.
pub fn decode_preview(source: &Source) -> Option<DecodedImage> {
    let mut limits = KindLimits::for_kind(MediaKind::Attachment);
    limits.max_animation_bytes = PREVIEW_ANIMATION_BYTES;
    limits.max_pixels = limits.max_pixels.min(PREVIEW_MAX_PIXELS);
    let bytes = match source {
        Source::Memory(bytes) => bytes.clone(),
        Source::Path(path) => read_bounded(path, limits.max_encoded_bytes)?,
    };
    let inspected = inlet_media::inspect(bytes, &limits).ok()?;
    if inspected.estimated_peak_bytes() > PREVIEW_MEMORY {
        return None;
    }
    let _lane = PREVIEW_LANE.lock().unwrap_or_else(|e| e.into_inner());
    let control = DecodeControl::with_budget(limits.decode_budget);
    inspected.decode(PREVIEW_FIT, &limits, &control).ok()
}

impl Attachments {
    /// The files attached in `channel`.
    pub fn staged(&self, channel: &str) -> &[Staged] {
        self.by_channel.get(channel).map_or(&[], Vec::as_slice)
    }

    fn pasted_bytes(&self) -> u64 {
        self.by_channel
            .values()
            .flatten()
            .filter_map(|s| match &s.file.source {
                Source::Memory(b) => Some(b.len() as u64),
                Source::Path(_) => None,
            })
            .sum()
    }

    /// Adds files to `channel` if all of them fit the limits together with those there;
    /// otherwise none. Returns their keys.
    pub fn add(&mut self, channel: &str, files: Vec<FileToSend>) -> Result<Vec<u64>, Refused> {
        let there = self.staged(channel);
        self.limits.check(
            there
                .iter()
                .map(|s| (s.file.name.as_str(), s.file.size))
                .chain(files.iter().map(|f| (f.name.as_str(), f.size))),
        )?;
        let pasted: u64 = files
            .iter()
            .filter(|f| matches!(f.source, Source::Memory(_)))
            .map(|f| f.size)
            .sum();
        if self.pasted_bytes().saturating_add(pasted) > MAX_PASTED_BYTES {
            return Err(Refused::Total {
                max: MAX_PASTED_BYTES,
            });
        }
        let list = self.by_channel.entry(channel.to_owned()).or_default();
        let mut keys = Vec::with_capacity(files.len());
        for file in files {
            let key = self.next_key;
            self.next_key += 1;
            keys.push(key);
            list.push(Staged {
                key,
                file,
                preview: None,
                metadata: Metadata::Checking,
            });
        }
        Ok(keys)
    }

    pub fn get_mut(&mut self, channel: &str, key: u64) -> Option<&mut Staged> {
        self.by_channel
            .get_mut(channel)?
            .iter_mut()
            .find(|s| s.key == key)
    }

    pub fn remove(&mut self, channel: &str, key: u64) -> Option<Staged> {
        let list = self.by_channel.get_mut(channel)?;
        let ix = list.iter().position(|s| s.key == key)?;
        let staged = list.remove(ix);
        if list.is_empty() {
            self.by_channel.remove(channel);
        }
        Some(staged)
    }

    /// Everything attached in `channel`, for a send.
    pub fn take(&mut self, channel: &str) -> Vec<Staged> {
        if self.alt.as_ref().is_some_and(|a| a.channel == channel) {
            self.alt = None;
        }
        self.by_channel.remove(channel).unwrap_or_default()
    }

    /// Drops everything (sign-out).
    pub fn clear(&mut self) -> Vec<Staged> {
        self.epoch += 1;
        self.alt = None;
        self.in_flight.clear();
        self.by_channel.drain().flat_map(|(_, v)| v).collect()
    }

    /// The first file of `channel` that must not be sent as it is: its metadata
    /// cannot be removed and the person has not chosen to send it anyway.
    pub fn stuck(&self, channel: &str) -> Option<&Staged> {
        if !self.strip_exif {
            return None;
        }
        self.staged(channel)
            .iter()
            .find(|s| s.metadata == Metadata::Stuck && !s.file.keep_metadata)
    }

    /// A message with files went out: its text is kept until it is sent.
    pub fn sending(&mut self, entry: InFlight) {
        if self.in_flight.len() >= MAX_IN_FLIGHT {
            self.in_flight.remove(0);
        }
        self.in_flight.push(entry);
    }

    /// The message was sent or cancelled.
    pub fn sent(&mut self, nonce: &str) {
        self.in_flight.retain(|f| f.nonce != nonce);
    }

    /// Another account becomes active: every message with files not sent yet becomes a
    /// draft of its channel again, with a line naming its files. Returns how many.
    pub fn back_to_drafts(&mut self, drafts: &mut super::drafts::Drafts) -> usize {
        let count = self.in_flight.len();
        for f in std::mem::take(&mut self.in_flight) {
            // Names only as text: nothing in them can become a mention.
            let names: Vec<String> = f
                .names
                .iter()
                .map(|n| {
                    n.replace('<', "\u{2039}")
                        .replace('>', "\u{203a}")
                        .replace('@', "\u{ff20}")
                })
                .collect();
            let line = t!("composer-upload-draft-files", names = names.join(", "));
            let mut draft = f.draft.unwrap_or_default();
            draft.wire = if draft.wire.is_empty() {
                line
            } else {
                format!("{}\n{line}", draft.wire)
            };
            if let Some(now) = drafts.get(&f.channel_id).cloned() {
                draft.wire = format!("{}\n{}", draft.wire, now.wire);
                draft.labels.extend(now.labels);
            }
            drafts.put(&f.channel_id, Some(draft));
        }
        count
    }
}

impl ComposerView {
    fn open_channel_id(&self) -> Option<String> {
        self.channel.as_ref().map(|c| c.channel_id.clone())
    }

    /// The account's upload limits are known.
    pub fn set_upload_limits(&mut self, limits: Limits) {
        self.attachments.limits = limits;
    }

    /// Whether files may be attached in the open channel: always in direct messages,
    /// in a guild with `ATTACH_FILES` (not known yet counts as no).
    pub(super) fn may_attach(&self) -> bool {
        match &self.channel {
            Some(ch) if ch.guild_id.is_none() => true,
            Some(_) => self.rights().attach_files,
            None => false,
        }
    }

    /// Says why nothing is attached here, when the right is missing.
    fn refuse_attach(&mut self, cx: &mut Context<Self>) -> bool {
        if self.may_attach() {
            return false;
        }
        self.error = Some(t!("composer-attach-no-permission"));
        cx.notify();
        true
    }

    /// The plus button: the system's file dialog, more than one file allowed.
    pub(super) fn pick_files(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.open_channel_id() else {
            return;
        };
        if self.refuse_attach(cx) {
            return;
        }
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(SharedString::from(t!("composer-attach-prompt"))),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.attach_paths(channel, paths, cx));
        })
        .detach();
    }

    /// Files dropped on the composer.
    // Deviation: Fluxer takes drops anywhere over the chat and asks in a dialog; Inlet
    // takes them on the composer, which the message list does not reach into.
    pub(super) fn on_drop_files(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        if self.refuse_attach(cx) {
            return;
        }
        if let Some(channel) = self.open_channel_id() {
            self.attach_paths(channel, paths.paths().to_vec(), cx);
        }
    }

    /// Paste in the box: images and copied files are attached; anything else is left to
    /// the editor (text).
    pub(super) fn on_paste(
        &mut self,
        _: &inlet_editor::actions::Paste,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor.read(cx).marked_range().is_some() {
            return;
        }
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let mut images = Vec::new();
        let mut paths = Vec::new();
        let mut refused = None;
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => match pasted_file(image) {
                    Ok(file) => images.push(file),
                    Err(reason) => refused = Some(reason),
                },
                ClipboardEntry::ExternalPaths(p) => paths.extend(p.paths().iter().cloned()),
                ClipboardEntry::String(_) => {}
            }
        }
        if images.is_empty() && paths.is_empty() && refused.is_none() {
            return;
        }
        cx.stop_propagation();
        if self.refuse_attach(cx) {
            return;
        }
        if let Some(reason) = refused {
            // Never silent: the picture was not attached, and this says why.
            self.error = Some(reason);
            cx.notify();
        }
        let Some(channel) = self.open_channel_id() else {
            return;
        };
        if !images.is_empty() {
            self.attach(&channel, self.attachments.epoch, images, cx);
        }
        if !paths.is_empty() {
            self.attach_paths(channel, paths, cx);
        }
    }

    /// Looks at picked or dropped paths off the UI thread, then attaches the files.
    fn attach_paths(&mut self, channel: String, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let epoch = self.attachments.epoch;
        let looked =
            cx.background_spawn(
                async move { paths.iter().map(|p| picked_file(p)).collect::<Vec<_>>() },
            );
        cx.spawn(async move |this, cx| {
            let looked = looked.await;
            let _ = this.update(cx, |this, cx| {
                let mut files = Vec::new();
                for f in looked {
                    match f {
                        Ok(file) => files.push(file),
                        Err(name) => {
                            this.error = Some(t!("composer-upload-not-a-file", name = name));
                        }
                    }
                }
                if !files.is_empty() {
                    this.attach(&channel, epoch, files, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Puts files in `channel`'s tray, or says why they do not fit.
    fn attach(
        &mut self,
        channel: &str,
        epoch: u64,
        files: Vec<FileToSend>,
        cx: &mut Context<Self>,
    ) {
        if epoch != self.attachments.epoch {
            return;
        }
        let looks: Vec<(Source, u64, bool)> = files
            .iter()
            .map(|f| (f.source.clone(), f.size, previewable(&f.content_type)))
            .collect();
        match self.attachments.add(channel, files) {
            Ok(keys) => {
                for (key, (source, size, wanted)) in keys.into_iter().zip(looks) {
                    self.check_file(channel.to_owned(), key, source.clone(), size, cx);
                    if wanted {
                        self.load_preview(channel.to_owned(), key, source, cx);
                    }
                }
            }
            Err(refused) => self.error = Some(refused.reason()),
        }
        cx.notify();
    }

    /// Looks at what metadata removal would do with a file, off the UI thread.
    fn check_file(
        &mut self,
        channel: String,
        key: u64,
        source: Source,
        size: u64,
        cx: &mut Context<Self>,
    ) {
        let checked = cx.background_spawn(async move { check_metadata(&source, size) });
        cx.spawn(async move |this, cx| {
            let metadata = checked.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(staged) = this.attachments.get_mut(&channel, key) {
                    staged.metadata = metadata;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_preview(&mut self, channel: String, key: u64, source: Source, cx: &mut Context<Self>) {
        let decoded = cx.background_spawn(async move { decode_preview(&source) });
        cx.spawn(async move |this, cx| {
            let Some(decoded) = decoded.await else {
                return;
            };
            // A still: the first frame only.
            let mut frames = decoded.into_gpui_frames();
            frames.truncate(1);
            let image = Arc::new(RenderImage::new(frames));
            let _ = this.update(cx, |this, cx| {
                match this.attachments.get_mut(&channel, key) {
                    Some(staged) => staged.preview = Some(image),
                    // Removed while it decoded.
                    None => cx.drop_image(image, None),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn remove_attachment(&mut self, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.open_channel_id() else {
            return;
        };
        if self.attachments.alt.as_ref().is_some_and(|a| a.key == key) {
            self.close_alt(window, cx);
        }
        if let Some(staged) = self.attachments.remove(&channel, key) {
            drop_previews(vec![staged], Some(window), cx);
        }
        cx.notify();
    }

    /// The files of the open channel, for a send; the tray empties.
    pub(super) fn take_attachments(
        &mut self,
        channel: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<FileToSend> {
        let staged = self.attachments.take(channel);
        let files = staged.iter().map(|s| s.file.clone()).collect();
        drop_previews(staged, Some(window), cx);
        files
    }

    /// Sign-out: every channel's attachments go, pasted images with them.
    pub(super) fn clear_attachments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let staged = self.attachments.clear();
        drop_previews(staged, Some(window), cx);
    }

    pub(super) fn has_attachments(&self) -> bool {
        self.channel
            .as_ref()
            .is_some_and(|c| !self.attachments.staged(&c.channel_id).is_empty())
    }

    // Deviation: Fluxer edits alt text (and the name) in a modal; Inlet opens a field in
    // the tray, so no layer covers the chat.
    fn start_alt(&mut self, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.open_channel_id() else {
            return;
        };
        let current = self
            .attachments
            .get_mut(&channel, key)
            .and_then(|s| s.file.description.clone())
            .unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut e = Editor::new(ALT_MAX, cx);
            e.set_style(editor_style(), cx);
            e
        });
        editor.update(cx, |e, cx| {
            e.set_placeholder(t!("composer-attach-alt-placeholder"), cx);
            let _ = e.insert_text(&current, window, cx);
        });
        let events = cx.subscribe_in(
            &editor,
            window,
            |this, _, event: &EditorEvent, window, cx| {
                if let EditorEvent::Submit = event {
                    this.save_alt(window, cx);
                }
            },
        );
        window.focus(&editor.focus_handle(cx), cx);
        self.attachments.alt = Some(AltEdit {
            channel,
            key,
            editor,
            _events: events,
        });
        cx.notify();
    }

    fn save_alt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(alt) = self.attachments.alt.take() else {
            return;
        };
        let text: String = alt
            .editor
            .read(cx)
            .buffer()
            .text()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(staged) = self.attachments.get_mut(&alt.channel, alt.key) {
            staged.file.description = (!text.is_empty()).then_some(text);
        }
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    /// Escape in the alt text field: closes it unsaved. Whether it was open.
    pub(super) fn close_alt(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let open = self.open_channel_id();
        if !self
            .attachments
            .alt
            .as_ref()
            .is_some_and(|a| Some(&a.channel) == open.as_ref())
        {
            return false;
        }
        self.attachments.alt = None;
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
        true
    }

    /// The tray above the box: one tile per file, the metadata switch, and the alt text
    /// field when it is open.
    pub(super) fn render_attachments(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let channel = self.channel.as_ref()?;
        let staged = self.attachments.staged(&channel.channel_id);
        if staged.is_empty() {
            return None;
        }
        let tiles: Vec<AnyElement> = staged.iter().map(|s| self.render_tile(s, cx)).collect();
        let strip = self.attachments.strip_exif;
        let alt = self
            .attachments
            .alt
            .as_ref()
            .filter(|a| a.channel == channel.channel_id)
            .map(|a| {
                let name = staged
                    .iter()
                    .find(|s| s.key == a.key)
                    .map(|s| s.file.name.clone())
                    .unwrap_or_default();
                div()
                    .mt_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_secondary())
                            .child(t!("composer-attach-alt-title", name = name)),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .rounded(px(4.))
                            .bg(theme::bg_textarea())
                            .border_1()
                            .border_color(theme::ring())
                            .text_size(px(14.))
                            .child(a.editor.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_tertiary())
                            .child(t!("composer-attach-alt-hint")),
                    )
            });
        Some(
            div()
                .mx(px(6.))
                .px(px(10.))
                .pt(px(12.))
                .pb(px(10.))
                .when(self.reply.is_none(), |d| d.rounded_t(px(8.)))
                .bg(theme::bg_textarea())
                .border_1()
                .border_b_0()
                .border_color(theme::ring())
                .flex()
                .flex_col()
                .min_w_0()
                .child(
                    // The tiles scroll sideways. They sit in an absolute layer of a box of
                    // their height, so they never widen the composer or the chat.
                    div().relative().w_full().h(px(TILE)).child(
                        div()
                            .id("composer-attachments")
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .overflow_x_scroll()
                            .flex()
                            .flex_row()
                            .gap(px(24.))
                            .children(tiles),
                    ),
                )
                .child(
                    div()
                        .id("composer-strip-exif")
                        .mt_2()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(theme::text_secondary())
                        .child(
                            div()
                                .size(px(14.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(3.))
                                .border_1()
                                .border_color(theme::text_tertiary())
                                .when(strip, |d| {
                                    d.bg(theme::brand())
                                        .border_color(theme::brand())
                                        .text_color(theme::on_brand())
                                        .child(t!("composer-attach-check"))
                                }),
                        )
                        .child(t!("composer-attach-strip-exif"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.attachments.strip_exif = !this.attachments.strip_exif;
                            cx.notify();
                        })),
                )
                .children(alt)
                .into_any_element(),
        )
    }

    fn render_tile(&self, s: &Staged, cx: &mut Context<Self>) -> AnyElement {
        let key = s.key;
        let spoiler = s.file.spoiler;
        let has_alt = s.file.description.is_some();
        let ext = s
            .file
            .name
            .rsplit_once('.')
            .map(|(_, e)| e.to_uppercase())
            .unwrap_or_default();
        let media = div()
            .relative()
            .w_full()
            .h(px(PREVIEW_H))
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .rounded(px(4.))
            .bg(theme::media_placeholder())
            .child(match &s.preview {
                Some(image) => img(image.clone())
                    .w_full()
                    .h(px(PREVIEW_H))
                    .object_fit(ObjectFit::Contain)
                    .when(spoiler, |i| i.opacity(0.25))
                    .into_any_element(),
                None => div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_tertiary())
                    .child(ext)
                    .into_any_element(),
            })
            .when(spoiler, |d| {
                d.child(
                    div()
                        .absolute()
                        .px_3()
                        .py_1()
                        .rounded(px(20.))
                        .bg(theme::media_overlay())
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_strong())
                        .child(t!("composer-attach-spoiler-on")),
                )
            });
        let button = |id: String, label: String, on: bool| {
            div()
                .id(SharedString::from(id))
                .px_2()
                .h(px(24.))
                .flex()
                .items_center()
                .cursor_pointer()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if on {
                    theme::link()
                } else {
                    theme::text_secondary()
                })
                .hover(|st| {
                    st.bg(theme::floating_hover())
                        .text_color(theme::text_strong())
                })
                .child(label)
        };
        let actions = div()
            .absolute()
            .top(px(4.))
            .right(px(4.))
            .flex()
            .flex_row()
            .rounded(px(4.))
            .bg(theme::floating_bg())
            .border_1()
            .border_color(theme::accent_border())
            .shadow_sm()
            .child(
                button(
                    format!("attach-spoiler-{key}"),
                    t!("composer-attach-spoiler"),
                    spoiler,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(channel) = this.open_channel_id() {
                        if let Some(s) = this.attachments.get_mut(&channel, key) {
                            s.file.spoiler = !s.file.spoiler;
                        }
                    }
                    cx.notify();
                })),
            )
            .child(
                button(
                    format!("attach-alt-{key}"),
                    t!("composer-attach-alt"),
                    has_alt,
                )
                .on_click(cx.listener(move |this, _, window, cx| this.start_alt(key, window, cx))),
            )
            .child(
                button(
                    format!("attach-remove-{key}"),
                    t!("composer-attach-remove"),
                    false,
                )
                .on_click(
                    cx.listener(move |this, _, window, cx| this.remove_attachment(key, window, cx)),
                ),
            );
        div()
            .relative()
            .flex_none()
            .w(px(TILE))
            .h(px(TILE))
            .p_2()
            .flex()
            .flex_col()
            .justify_between()
            .rounded(px(4.))
            .bg(theme::panel_bg())
            .child(media)
            .child(
                div()
                    .mt_2()
                    .text_size(px(14.))
                    .text_color(theme::text())
                    .truncate()
                    .child(s.file.name.clone()),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_tertiary())
                    .child(crate::ui::media::file_size(s.file.size)),
            )
            .children(self.render_metadata_note(s, cx))
            .child(actions)
            .into_any_element()
    }

    /// Under a photo whose metadata cannot be removed: a warning and the choice to
    /// send it as it is (or, once chosen, a note and the way back).
    fn render_metadata_note(&self, s: &Staged, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.attachments.strip_exif || s.metadata != Metadata::Stuck {
            return None;
        }
        let key = s.key;
        let keep = s.file.keep_metadata;
        Some(
            div()
                .absolute()
                .left(px(8.))
                .right(px(8.))
                .bottom(px(54.))
                .p_1()
                .rounded(px(4.))
                .bg(theme::floating_bg())
                .flex()
                .flex_col()
                .gap_1()
                .text_size(px(11.))
                .text_color(if keep {
                    theme::text_secondary()
                } else {
                    theme::danger()
                })
                .child(if keep {
                    t!("composer-attach-metadata-kept")
                } else {
                    t!("composer-attach-metadata-stuck")
                })
                .child(
                    div()
                        .id(SharedString::from(format!("attach-keep-{key}")))
                        .cursor_pointer()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::link())
                        .child(if keep {
                            t!("composer-attach-metadata-undo")
                        } else {
                            t!("composer-attach-send-anyway")
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(channel) = this.open_channel_id() {
                                if let Some(s) = this.attachments.get_mut(&channel, key) {
                                    s.file.keep_metadata = !s.file.keep_metadata;
                                }
                            }
                            this.error = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }
}

#[cfg(feature = "demo")]
impl ComposerView {
    /// The `composer-attachments` scene: its files in the tray and its upload on the way,
    /// once, when the composer opens its first channel.
    pub(super) fn demo_attachments(&mut self, cx: &mut Context<Self>) {
        use crate::demo::scenes::{current, Scene, DEMO_UPLOAD_NONCE};
        if self.attachments.demo_shown || current() != Scene::ComposerAttachments {
            return;
        }
        let Some(channel) = self.open_channel_id() else {
            return;
        };
        self.attachments.demo_shown = true;
        self.attach(&channel, self.attachments.epoch, demo::files(), cx);
        let total = 48 * MIB;
        self.uploads.started(
            DEMO_UPLOAD_NONCE.into(),
            channel,
            vec!["holiday-video.mp4".into()],
            total,
        );
        self.uploads
            .progress(DEMO_UPLOAD_NONCE, total * 2 / 5, total);
    }
}

/// Frees the textures of previews that are gone.
fn drop_previews(staged: Vec<Staged>, mut window: Option<&mut Window>, cx: &mut gpui::App) {
    for image in staged.into_iter().filter_map(|s| s.preview) {
        cx.drop_image(image, window.as_deref_mut());
    }
}

#[cfg(any(test, feature = "demo"))]
pub mod demo {
    //! The tray of the `composer-attachments` demo scene: a pasted photo with alt text,
    //! a spoilered picture and a document.

    use super::*;

    pub fn files() -> Vec<FileToSend> {
        let solid = |w: u32, h: u32, rgba: [u8; 4]| {
            let pixels: Vec<u8> = rgba.repeat((w * h) as usize);
            crate::backend::media::png::encode(w, h, &pixels)
        };
        let photo = solid(320, 200, [92, 124, 250, 255]);
        let night = solid(200, 200, [40, 40, 52, 255]);
        let png = |name: &str, bytes: Vec<u8>, spoiler: bool, alt: Option<String>| FileToSend {
            name: name.to_owned(),
            size: bytes.len() as u64,
            content_type: "image/png".into(),
            spoiler,
            description: alt,
            keep_metadata: false,
            source: Source::Memory(Bytes::from(bytes)),
        };
        vec![
            png("image.png", photo, false, Some(t!("composer-demo-alt"))),
            png("night-sky.png", night, true, None),
            FileToSend {
                name: "release-notes.pdf".into(),
                size: 482_133,
                content_type: "application/pdf".into(),
                spoiler: false,
                description: None,
                keep_metadata: false,
                source: Source::Memory(Bytes::from(vec![0u8; 64])),
            },
        ]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn memory(name: &str, len: usize) -> FileToSend {
        FileToSend {
            name: name.into(),
            size: len as u64,
            content_type: content_type_of(name).into(),
            spoiler: false,
            description: None,
            keep_metadata: false,
            source: Source::Memory(Bytes::from(vec![0u8; len])),
        }
    }

    #[test]
    fn the_tray_keeps_to_the_limits_and_to_its_channel() {
        let mut a = Attachments::default();
        let keys = a
            .add("1", vec![memory("a.png", 10), memory("b.png", 10)])
            .unwrap();
        assert_eq!(keys.len(), 2);
        assert!(a.staged("2").is_empty(), "another channel has its own tray");
        // Eleven files in one message are refused, and none of the new ones is added.
        let nine: Vec<FileToSend> = (0..9).map(|_| memory("c.png", 1)).collect();
        assert_eq!(a.add("1", nine), Err(Refused::Count { max: 10 }));
        assert_eq!(a.staged("1").len(), 2);
        // Pasted images in memory have a ceiling of their own.
        let big = memory("big.png", (MAX_PASTED_BYTES - 10) as usize);
        a.limits.max_file_size = u64::MAX;
        a.limits.max_total = u64::MAX;
        assert!(matches!(a.add("2", vec![big]), Err(Refused::Total { .. })));
        // A send takes them; sign-out drops the rest.
        assert_eq!(a.take("1").len(), 2);
        a.add("3", vec![memory("d.png", 1)]).unwrap();
        assert_eq!(a.clear().len(), 1);
        assert!(a.staged("3").is_empty());
    }

    #[test]
    fn picked_files_are_named_by_their_last_component_only() {
        let dir = std::env::temp_dir().join(format!("inlet-pick-{}", p::new_nonce()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report\u{202e}fdp.exe");
        std::fs::write(&path, b"hello").unwrap();
        let f = picked_file(&path).unwrap();
        assert_eq!(f.name, "reportfdp.exe");
        assert_eq!(f.size, 5);
        assert!(matches!(f.source, Source::Path(_)));
        // A folder is no file.
        assert!(picked_file(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_demo_tray_fits_the_limits() {
        let mut a = Attachments::default();
        assert_eq!(a.add("1", demo::files()).map(|k| k.len()), Ok(3));
    }

    #[test]
    fn previews_decode_within_the_media_limits() {
        let png = crate::backend::media::png::solid(640, 480, [1, 2, 3, 255]);
        let decoded = decode_preview(&Source::Memory(Bytes::from(png))).expect("a preview");
        assert!(
            decoded.byte_size() <= 368 * 296 * 4,
            "scaled down to the tile"
        );
        // Larger than the encoded limit: refused before anything is decoded.
        let huge = Bytes::from(vec![0u8; 21 * MIB as usize]);
        assert!(decode_preview(&Source::Memory(huge)).is_none());
        assert!(decode_preview(&Source::Memory(Bytes::from_static(b"not an image"))).is_none());
    }

    #[test]
    fn previews_stay_small_and_still() {
        // A 24 MP picture is never decoded for a preview.
        let header = crate::backend::media::png::claiming(6000, 4000);
        assert!(decode_preview(&Source::Memory(Bytes::from(header))).is_none());
    }

    pub(crate) fn bmp(w: u32, h: u32) -> Vec<u8> {
        let pixels: Vec<u8> = (0..w * h * 3).map(|i| (i % 200) as u8).collect();
        let mut out = Vec::new();
        image::codecs::bmp::BmpEncoder::new(&mut out)
            .encode(&pixels, w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    #[test]
    fn a_pasted_bmp_becomes_a_png_in_memory_and_svg_is_refused() {
        let pasted = pasted_file(&gpui::Image::from_bytes(gpui::ImageFormat::Bmp, bmp(8, 4)))
            .expect("attached");
        assert_eq!(pasted.name, "image.png");
        assert_eq!(pasted.content_type, "image/png");
        let Source::Memory(bytes) = &pasted.source else {
            panic!("in memory");
        };
        assert!(bytes.starts_with(b"\x89PNG"));
        let decoded = decode_preview(&pasted.source).expect("a real PNG");
        assert!(decoded.byte_size() > 0);
        // Formats that are not sent as images say so.
        let image = gpui::Image::from_bytes;
        assert!(pasted_file(&image(gpui::ImageFormat::Svg, b"<svg/>".to_vec())).is_err());
        assert!(pasted_file(&image(gpui::ImageFormat::Bmp, b"BMjunk".to_vec())).is_err());
    }

    #[test]
    fn photos_are_checked_by_their_bytes() {
        let jpeg = crate::backend::composer::exif::tests::jpeg_with_exif(1);
        assert_eq!(
            check_metadata(
                &Source::Memory(Bytes::from(jpeg.clone())),
                jpeg.len() as u64
            ),
            Metadata::Removable
        );
        let mut broken = jpeg.clone();
        broken.truncate(broken.len() - 19);
        broken.extend_from_slice(&[0xFF, 0xDB, 0x40, 0x00]);
        assert_eq!(
            check_metadata(&Source::Memory(Bytes::from(broken)), 10),
            Metadata::Stuck
        );
        let mut a = Attachments::default();
        a.add("1", vec![memory("a.png", 3)]).unwrap();
        a.by_channel.get_mut("1").unwrap()[0].metadata = Metadata::Stuck;
        assert!(a.stuck("1").is_some(), "not sent as it is");
        a.by_channel.get_mut("1").unwrap()[0].file.keep_metadata = true;
        assert!(a.stuck("1").is_none(), "the person chose to send it");
    }
}
