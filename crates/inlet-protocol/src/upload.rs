//! Attachment uploads: a `multipart/form-data` Create message with
//! `payload_json` and `files[N]`, streamed in 64 KiB chunks with a progress
//! callback.
//!
//! Limits are checked before anything is sent: the number of files
//! (`max_attachments_per_message`), each file's size
//! (`max_attachment_file_size`, at most [`crate::limits::UPLOAD_HARD_CAP`])
//! and the total (at most [`crate::limits::MAX_UPLOAD_TOTAL`], 512 MiB),
//! resolved from the discovery document and the account's traits. The
//! request goes through the account's rate limiter like every other call,
//! and is never sent twice: a 429 comes back as [`crate::Error::RateLimited`].
//!
//! Only the last path component of a file name is sent, so a full local path
//! never shows up in the channel.
//!
//! A file's bytes can be in memory or on disk ([`UploadData`]): a part on disk
//! is read 64 KiB at a time while the body goes out, so a large file is never
//! held in memory. Its length is checked when it is opened and when it ends.
//!
//! Fluxer's pre-signed uploads (each file straight to storage, large ones in
//! parts) are in [`crate::presigned`].

use std::future::Future;
use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Frame, SizeHint};
use serde_json::{json, Value};
use tokio::sync::watch;

/// Called with `(bytes_sent, total_bytes)` while the request body goes out:
/// about every 1 % and at the end. It runs on a separate tokio task, never
/// inside the HTTP body, so a slow callback cannot stall the upload, and a
/// panic in it is caught (and stops further calls). Keep it quick and
/// non-blocking anyway, for example by sending to a channel the UI reads.
pub type ProgressFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// One piece of a file's bytes: in memory, or a range of a file on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Bytes(Bytes),
    /// `range` of the file at `path`, which must be `file_len` bytes long
    /// (checked when it is opened). Read while the body goes out.
    File {
        path: PathBuf,
        range: Range<u64>,
        file_len: u64,
    },
}

impl Segment {
    pub fn len(&self) -> u64 {
        match self {
            Segment::Bytes(b) => b.len() as u64,
            Segment::File { range, .. } => range.end.saturating_sub(range.start),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A file's bytes, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UploadData(pub Vec<Segment>);

impl UploadData {
    /// A whole file on disk of `len` bytes.
    pub fn file(path: impl Into<PathBuf>, len: u64) -> Self {
        UploadData(vec![Segment::File {
            path: path.into(),
            range: 0..len,
            file_len: len,
        }])
    }

    pub fn len(&self) -> u64 {
        self.0.iter().map(Segment::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl From<Bytes> for UploadData {
    fn from(b: Bytes) -> Self {
        UploadData(vec![Segment::Bytes(b)])
    }
}

impl From<Vec<u8>> for UploadData {
    fn from(b: Vec<u8>) -> Self {
        Bytes::from(b).into()
    }
}

/// One file to attach.
#[derive(Clone)]
pub struct UploadFile {
    /// File name shown in the message.
    pub filename: String,
    /// The bytes.
    pub data: UploadData,
    /// Media type, for example `image/png`. `None` sends `application/octet-stream`.
    pub content_type: Option<String>,
    /// Alt text.
    pub description: Option<String>,
    /// Mark as a spoiler.
    pub spoiler: bool,
}

impl std::fmt::Debug for UploadFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadFile")
            .field("filename", &self.filename)
            .field("len", &self.data.len())
            .field("content_type", &self.content_type)
            .field("spoiler", &self.spoiler)
            .finish()
    }
}

impl UploadFile {
    /// A file from bytes (or from [`UploadData`]).
    pub fn new(filename: impl Into<String>, data: impl Into<UploadData>) -> Self {
        UploadFile {
            filename: filename.into(),
            data: data.into(),
            content_type: None,
            description: None,
            spoiler: false,
        }
    }

    /// Sets the media type.
    pub fn content_type(mut self, ct: impl Into<String>) -> Self {
        self.content_type = Some(ct.into());
        self
    }
}

/// Limits that apply to one upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadLimits {
    /// Largest file in bytes.
    pub max_file_size: u64,
    /// Most files in one message.
    pub max_files: u64,
    /// Most bytes in one message, all files together.
    pub max_total_size: u64,
}

/// Fluxer's limit for a file name, in UTF-16 code units (JavaScript `length`).
const MAX_NAME_UTF16: usize = 255;
/// Longest extension kept whole when a name is shortened.
const MAX_EXTENSION_UTF16: usize = 16;

/// A file name safe to send: the last path component only (`/` and `\` both
/// separate), with quotes and control characters replaced, format characters
/// (direction overrides, zero-width and tag characters, soft hyphens) dropped,
/// at most 255 UTF-16 units with the extension kept when it is shortened, and
/// `file` for a name with nothing left. Every upload path uses it, so a full
/// local path never reaches the server.
pub fn safe_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|c| !is_format_char(*c))
        .map(|c| match c {
            '"' | '\\' | '/' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return "file".to_owned();
    }
    shorten(trimmed)
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The first characters of `s` that fit in `max` UTF-16 units.
fn utf16_prefix(s: &str, max: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        used += c.len_utf16();
        if used > max {
            return &s[..i];
        }
    }
    s
}

/// `name` cut to [`MAX_NAME_UTF16`], keeping a short extension.
fn shorten(name: &str) -> String {
    if utf16_len(name) <= MAX_NAME_UTF16 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty() && !ext.is_empty() && utf16_len(ext) < MAX_EXTENSION_UTF16 =>
        {
            let room = MAX_NAME_UTF16 - 1 - utf16_len(ext);
            format!("{}.{ext}", utf16_prefix(stem, room).trim_end())
        }
        _ => utf16_prefix(name, MAX_NAME_UTF16).to_owned(),
    }
}

/// Unicode format characters (category Cf): they can make `photo\u{202e}gnp.exe`
/// read as `photoexe.png`, or hide text, so names never keep them.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

/// A media type, or `application/octet-stream` when it is not a plain `type/subtype`.
pub fn safe_content_type(ct: Option<&str>) -> String {
    let ok = |s: &str| {
        let mut parts = s.split('/');
        let token = |t: &str| {
            !t.is_empty()
                && t.len() <= 127
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))
        };
        matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None) if token(a) && token(b))
    };
    match ct.map(str::trim) {
        Some(s) if ok(s) => s.to_ascii_lowercase(),
        _ => "application/octet-stream".to_owned(),
    }
}

/// A prepared multipart body. Cheap to turn into a request body more than once.
pub(crate) struct Multipart {
    boundary: String,
    parts: Arc<[Segment]>,
    total: u64,
    progress: Option<ProgressFn>,
}

impl Multipart {
    /// Builds the body for Create message with `payload` (without `attachments`) and `files`.
    pub(crate) fn message(
        mut payload: Value,
        files: &[UploadFile],
        progress: Option<ProgressFn>,
    ) -> Multipart {
        let boundary = format!("inlet-{:032x}", rand::random::<u128>());
        let meta: Vec<Value> = files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let mut m = json!({ "id": i, "filename": safe_filename(&f.filename) });
                if let Some(d) = &f.description {
                    m["description"] = json!(d);
                }
                if f.spoiler {
                    m["flags"] = json!(crate::models::attachment_flags::IS_SPOILER);
                }
                m
            })
            .collect();
        if let Value::Object(map) = &mut payload {
            map.insert("attachments".into(), Value::Array(meta));
        }
        let text = |s: String| Segment::Bytes(Bytes::from(s));
        let mut parts: Vec<Segment> = Vec::with_capacity(files.len() * 3 + 2);
        parts.push(text(format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"payload_json\"\r\nContent-Type: application/json\r\n\r\n{payload}\r\n"
        )));
        for (i, f) in files.iter().enumerate() {
            parts.push(text(format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"files[{i}]\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                safe_filename(&f.filename),
                safe_content_type(f.content_type.as_deref())
            )));
            parts.extend(f.data.0.iter().cloned());
            parts.push(Segment::Bytes(Bytes::from_static(b"\r\n")));
        }
        parts.push(text(format!("--{boundary}--\r\n")));
        let total = parts.iter().map(Segment::len).sum();
        Multipart {
            boundary,
            parts: parts.into(),
            total,
            progress,
        }
    }

    pub(crate) fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    pub(crate) fn len(&self) -> u64 {
        self.total
    }

    /// A fresh request body that reports progress to a reporter task.
    pub(crate) fn body(&self) -> reqwest::Body {
        let progress = self
            .progress
            .clone()
            .and_then(|cb| spawn_reporter(cb, self.total));
        reqwest::Body::wrap(ProgressBody::new(self.parts.clone(), self.total, progress))
    }

    #[cfg(test)]
    fn bytes(&self) -> Vec<u8> {
        self.parts
            .iter()
            .flat_map(|p| match p {
                Segment::Bytes(b) => b.to_vec(),
                Segment::File { path, range, .. } => {
                    let all = std::fs::read(path).unwrap();
                    all[range.start as usize..range.end as usize].to_vec()
                }
            })
            .collect()
    }
}

/// A request body of one buffer that reports progress like [`Multipart::body`].
pub(crate) fn single_body(data: Bytes, progress: Option<ProgressFn>) -> reqwest::Body {
    let total = data.len() as u64;
    let progress = progress.and_then(|cb| spawn_reporter(cb, total));
    reqwest::Body::wrap(ProgressBody::new(
        Arc::from(vec![Segment::Bytes(data)]),
        total,
        progress,
    ))
}

/// Size of one body chunk.
const CHUNK: usize = 64 * 1024;

/// Starts the task that calls the callback with the latest progress. `None`
/// outside a tokio runtime (then there is no progress).
fn spawn_reporter(cb: ProgressFn, total: u64) -> Option<watch::Sender<u64>> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let (tx, mut rx) = watch::channel(0u64);
    handle.spawn(async move {
        let mut last = None;
        loop {
            let ended = rx.changed().await.is_err();
            let sent = *rx.borrow_and_update();
            if last != Some(sent) {
                last = Some(sent);
                if std::panic::catch_unwind(AssertUnwindSafe(|| cb(sent, total))).is_err() {
                    tracing::warn!("the upload progress callback panicked; no more progress");
                    return;
                }
            }
            if ended {
                return;
            }
        }
    });
    Some(tx)
}

/// A file segment being read.
struct FileRead {
    /// Opening it (and seeking) on the blocking pool.
    open: Option<tokio::task::JoinHandle<std::io::Result<std::fs::File>>>,
    file: Option<tokio::fs::File>,
    left: u64,
    buf: Vec<u8>,
}

impl FileRead {
    fn start(path: PathBuf, range: Range<u64>, file_len: u64) -> FileRead {
        let start = range.start;
        let open = tokio::task::spawn_blocking(move || {
            use std::io::{Seek, SeekFrom};
            let mut file = std::fs::File::open(&path)?;
            let meta = file.metadata()?;
            if !meta.is_file() || meta.len() != file_len {
                return Err(std::io::Error::other(
                    "the file changed after it was attached",
                ));
            }
            file.seek(SeekFrom::Start(start))?;
            Ok(file)
        });
        FileRead {
            open: Some(open),
            file: None,
            left: range.end.saturating_sub(range.start),
            buf: Vec::new(),
        }
    }
}

struct ProgressBody {
    parts: Arc<[Segment]>,
    index: usize,
    /// Position in an in-memory part.
    offset: usize,
    /// The file part being read.
    reading: Option<FileRead>,
    sent: u64,
    total: u64,
    /// Bytes at the last report.
    reported: u64,
    progress: Option<watch::Sender<u64>>,
}

impl ProgressBody {
    fn new(parts: Arc<[Segment]>, total: u64, progress: Option<watch::Sender<u64>>) -> Self {
        ProgressBody {
            parts,
            index: 0,
            offset: 0,
            reading: None,
            sent: 0,
            total,
            reported: 0,
            progress,
        }
    }

    fn count(&mut self, chunk: &Bytes) {
        self.sent = self.sent.saturating_add(chunk.len() as u64);
        if let Some(tx) = &self.progress {
            // About every 1 %, and at the end. Never blocks.
            let step = (self.total / 100).max(1);
            if self.sent >= self.total || self.sent - self.reported >= step {
                self.reported = self.sent;
                tx.send_replace(self.sent);
            }
        }
    }
}

impl http_body::Body for ProgressBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = &mut *self;
        loop {
            let Some(part) = this.parts.get(this.index) else {
                return Poll::Ready(None);
            };
            match part {
                Segment::Bytes(bytes) => {
                    if this.offset >= bytes.len() {
                        this.index += 1;
                        this.offset = 0;
                        continue;
                    }
                    let end = (this.offset + CHUNK).min(bytes.len());
                    let chunk = bytes.slice(this.offset..end);
                    this.offset = end;
                    this.count(&chunk);
                    return Poll::Ready(Some(Ok(Frame::data(chunk))));
                }
                Segment::File {
                    path,
                    range,
                    file_len,
                } => {
                    let read = this.reading.get_or_insert_with(|| {
                        FileRead::start(path.clone(), range.clone(), *file_len)
                    });
                    if read.left == 0 {
                        this.reading = None;
                        this.index += 1;
                        continue;
                    }
                    if let Some(open) = &mut read.open {
                        match Pin::new(open).poll(cx) {
                            Poll::Pending => return Poll::Pending,
                            Poll::Ready(Ok(Ok(file))) => {
                                read.file = Some(tokio::fs::File::from_std(file));
                                read.open = None;
                            }
                            Poll::Ready(Ok(Err(e))) => return Poll::Ready(Some(Err(e))),
                            Poll::Ready(Err(e)) => {
                                return Poll::Ready(Some(Err(std::io::Error::other(e))))
                            }
                        }
                    }
                    let want = (read.left as usize).min(CHUNK);
                    read.buf.resize(want, 0);
                    let Some(file) = read.file.as_mut() else {
                        return Poll::Ready(Some(Err(std::io::Error::other("file not open"))));
                    };
                    let mut buf = tokio::io::ReadBuf::new(&mut read.buf[..want]);
                    match tokio::io::AsyncRead::poll_read(Pin::new(file), cx, &mut buf) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                        Poll::Ready(Ok(())) => {
                            let n = buf.filled().len();
                            if n == 0 {
                                return Poll::Ready(Some(Err(std::io::Error::new(
                                    std::io::ErrorKind::UnexpectedEof,
                                    "the file got shorter while it was sent",
                                ))));
                            }
                            read.left -= n as u64;
                            let chunk = Bytes::copy_from_slice(buf.filled());
                            this.count(&chunk);
                            return Poll::Ready(Some(Ok(Frame::data(chunk))));
                        }
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.sent >= self.total
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.total.saturating_sub(self.sent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_types_are_sanitized() {
        assert_eq!(safe_filename("a\"b\r\nc.png"), "a_b__c.png");
        // Only the last path component, from Windows and Unix paths.
        assert_eq!(
            safe_filename("C:\\Users\\ferdi\\Documents\\lønn 2026.pdf"),
            "lønn 2026.pdf"
        );
        assert_eq!(
            safe_filename("/home/ferdi/private/report.txt"),
            "report.txt"
        );
        assert_eq!(safe_filename("..\\..\\x/../y.png"), "y.png");
        assert_eq!(safe_filename("dir/"), "file");
        assert_eq!(safe_filename("  "), "file");
        assert_eq!(safe_filename(".."), "file");
        assert_eq!(safe_filename(&"x".repeat(400)).len(), 255);
        // Lengths count in UTF-16 units, as Fluxer does, and the extension stays.
        let long = format!("{}.png", "\u{1f600}".repeat(200));
        let short = safe_filename(&long);
        assert!(utf16_len(&short) <= 255, "{}", utf16_len(&short));
        assert!(short.ends_with(".png"));
        assert_eq!(safe_filename("a\u{ad}b\u{e0041}.txt"), "ab.txt");
        // Direction overrides and zero-width characters cannot disguise a name.
        assert_eq!(safe_filename("photo\u{202e}gnp.exe"), "photognp.exe");
        assert_eq!(safe_filename("a\u{200b}b\u{feff}.png"), "ab.png");
        assert_eq!(safe_content_type(Some("image/PNG")), "image/png");
        assert_eq!(
            safe_content_type(Some("image/png\r\nX: y")),
            "application/octet-stream"
        );
        assert_eq!(safe_content_type(Some("text")), "application/octet-stream");
        assert_eq!(safe_content_type(None), "application/octet-stream");
    }

    #[test]
    fn multipart_layout() {
        let mut f = UploadFile::new("cat.png", vec![1u8, 2, 3]).content_type("image/png");
        f.spoiler = true;
        f.description = Some("a cat".into());
        let m = Multipart::message(json!({"content": "hi"}), &[f], None);
        let text = String::from_utf8_lossy(&m.bytes()).into_owned();
        let b = &m.boundary;
        assert!(text.starts_with(&format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"payload_json\""
        )));
        assert!(text.contains("\"attachments\":[{\"description\":\"a cat\",\"filename\":\"cat.png\",\"flags\":8,\"id\":0}]")
            || text.contains("\"attachments\":[{\"id\":0,\"filename\":\"cat.png\",\"description\":\"a cat\",\"flags\":8}]"),
            "{text}");
        assert!(text.contains("name=\"files[0]\"; filename=\"cat.png\"\r\nContent-Type: image/png\r\n\r\n\u{1}\u{2}\u{3}\r\n"));
        assert!(text.ends_with(&format!("--{b}--\r\n")));
        assert_eq!(m.len() as usize, m.bytes().len());
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use http_body_util_lite::collect;

    mod http_body_util_lite {
        use super::*;

        /// Reads a whole body.
        pub async fn collect(mut body: ProgressBody) -> std::io::Result<Vec<u8>> {
            let mut out = Vec::new();
            loop {
                let next =
                    std::future::poll_fn(|cx| http_body::Body::poll_frame(Pin::new(&mut body), cx))
                        .await;
                match next {
                    None => return Ok(out),
                    Some(Err(e)) => return Err(e),
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            assert!(data.len() <= CHUNK, "chunks stay small");
                            out.extend_from_slice(&data);
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn file_segments_stream_from_disk_and_check_their_length() {
        let dir = std::env::temp_dir().join(format!("inlet-stream-{}", crate::new_nonce()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.bin");
        let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &content).unwrap();
        let parts: Arc<[Segment]> = Arc::from(vec![
            Segment::Bytes(Bytes::from_static(b"head")),
            Segment::File {
                path: path.clone(),
                range: 10..200_010,
                file_len: content.len() as u64,
            },
        ]);
        let body = ProgressBody::new(parts, 4 + 200_000, None);
        let out = collect(body).await.unwrap();
        assert_eq!(&out[..4], b"head");
        assert_eq!(&out[4..], &content[10..200_010]);
        // A file that changed size since it was attached is refused.
        let parts: Arc<[Segment]> = Arc::from(vec![Segment::File {
            path: path.clone(),
            range: 0..10,
            file_len: 5,
        }]);
        assert!(collect(ProgressBody::new(parts, 10, None)).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
