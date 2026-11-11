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
//! Fluxer's pre-signed uploads (each file straight to storage, large ones in
//! parts) are in [`crate::presigned`].

use std::convert::Infallible;
use std::panic::AssertUnwindSafe;
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

/// One file to attach.
#[derive(Clone)]
pub struct UploadFile {
    /// File name shown in the message.
    pub filename: String,
    /// The bytes.
    pub data: Bytes,
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
    /// A file from bytes.
    pub fn new(filename: impl Into<String>, data: impl Into<Bytes>) -> Self {
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

/// A file name safe to send: the last path component only (`/` and `\` both
/// separate), with quotes and control characters replaced, at most 255
/// characters, and `file` for a name with nothing left. Every upload path uses
/// it, so a full local path never reaches the server.
pub fn safe_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|c| !is_invisible_format(*c))
        .map(|c| match c {
            '"' | '\\' | '/' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .take(255)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "file".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Bidirectional controls and zero-width characters: they can make
/// `photo\u{202e}gnp.exe` read as `photoexe.png`, so names never keep them.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{feff}'
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
    parts: Arc<[Bytes]>,
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
        let mut parts: Vec<Bytes> = Vec::with_capacity(files.len() * 2 + 2);
        parts.push(Bytes::from(format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"payload_json\"\r\nContent-Type: application/json\r\n\r\n{payload}\r\n"
        )));
        for (i, f) in files.iter().enumerate() {
            parts.push(Bytes::from(format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"files[{i}]\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                safe_filename(&f.filename),
                safe_content_type(f.content_type.as_deref())
            )));
            parts.push(f.data.clone());
            parts.push(Bytes::from_static(b"\r\n"));
        }
        parts.push(Bytes::from(format!("--{boundary}--\r\n")));
        let total = parts.iter().map(|p| p.len() as u64).sum();
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
        reqwest::Body::wrap(ProgressBody {
            parts: self.parts.clone(),
            index: 0,
            offset: 0,
            sent: 0,
            total: self.total,
            reported: 0,
            progress,
        })
    }

    #[cfg(test)]
    fn bytes(&self) -> Vec<u8> {
        self.parts.iter().flat_map(|p| p.iter().copied()).collect()
    }
}

/// A request body of one buffer that reports progress like [`Multipart::body`].
pub(crate) fn single_body(data: Bytes, progress: Option<ProgressFn>) -> reqwest::Body {
    let total = data.len() as u64;
    let progress = progress.and_then(|cb| spawn_reporter(cb, total));
    reqwest::Body::wrap(ProgressBody {
        parts: Arc::from(vec![data]),
        index: 0,
        offset: 0,
        sent: 0,
        total,
        reported: 0,
        progress,
    })
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

struct ProgressBody {
    parts: Arc<[Bytes]>,
    index: usize,
    offset: usize,
    sent: u64,
    total: u64,
    /// Bytes at the last report.
    reported: u64,
    progress: Option<watch::Sender<u64>>,
}

impl http_body::Body for ProgressBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = &mut *self;
        while let Some(part) = this.parts.get(this.index) {
            if this.offset >= part.len() {
                this.index += 1;
                this.offset = 0;
                continue;
            }
            let end = (this.offset + CHUNK).min(part.len());
            let chunk = part.slice(this.offset..end);
            this.offset = end;
            this.sent = this.sent.saturating_add(chunk.len() as u64);
            if let Some(tx) = &this.progress {
                // About every 1 %, and at the end. Never blocks.
                let step = (this.total / 100).max(1);
                if this.sent >= this.total || this.sent - this.reported >= step {
                    this.reported = this.sent;
                    tx.send_replace(this.sent);
                }
            }
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        Poll::Ready(None)
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
