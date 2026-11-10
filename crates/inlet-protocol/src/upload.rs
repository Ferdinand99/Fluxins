//! Attachment uploads: a `multipart/form-data` Create message with
//! `payload_json` and `files[N]`, streamed in 64 KiB chunks with a progress
//! callback.
//!
//! Limits are checked before anything is sent: the number of files
//! (`max_attachments_per_message`) and each file's size
//! (`max_attachment_file_size`, at most [`crate::limits::UPLOAD_HARD_CAP`]),
//! resolved from the discovery document and the account's traits. The
//! request goes through the account's rate limiter like every other call.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Frame, SizeHint};
use serde_json::{json, Value};

/// Called with `(bytes_sent, total_bytes)` as the request body goes out. It
/// can be called again from 0 if the request is retried after a 429.
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
}

/// A file name safe inside a quoted `Content-Disposition` parameter.
fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
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

/// A media type, or `application/octet-stream` when it is not a plain `type/subtype`.
fn safe_content_type(ct: Option<&str>) -> String {
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

    /// A fresh request body that reports progress.
    pub(crate) fn body(&self) -> reqwest::Body {
        reqwest::Body::wrap(ProgressBody {
            parts: self.parts.clone(),
            index: 0,
            offset: 0,
            sent: 0,
            total: self.total,
            progress: self.progress.clone(),
        })
    }

    #[cfg(test)]
    fn bytes(&self) -> Vec<u8> {
        self.parts.iter().flat_map(|p| p.iter().copied()).collect()
    }
}

/// Size of one body chunk.
const CHUNK: usize = 64 * 1024;

struct ProgressBody {
    parts: Arc<[Bytes]>,
    index: usize,
    offset: usize,
    sent: u64,
    total: u64,
    progress: Option<ProgressFn>,
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
            this.sent += chunk.len() as u64;
            if let Some(p) = &this.progress {
                p(this.sent, this.total);
            }
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        Poll::Ready(None)
    }

    fn is_end_stream(&self) -> bool {
        self.sent >= self.total
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.total - self.sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_types_are_sanitized() {
        assert_eq!(safe_filename("a\"b\r\nc/d\\e.png"), "a_b__c_d_e.png");
        assert_eq!(safe_filename("  "), "file");
        assert_eq!(safe_filename(".."), "file");
        assert_eq!(safe_filename(&"x".repeat(400)).len(), 255);
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
