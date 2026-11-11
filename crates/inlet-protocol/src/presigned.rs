//! Fluxer's pre-signed attachment uploads (`features.presigned_attachment_uploads`,
//! see [`crate::Discovery::presigned_attachment_uploads`]).
//!
//! 1. [`Client::presign_attachments`]: `POST /channels/{id}/attachments` with each
//!    file's name, size and type. The server answers with one plan per file: a single
//!    `PUT` URL for a file up to 10 MiB, or a multipart upload with one URL per part
//!    (parts of at least 10 MiB) above that.
//! 2. [`Client::put_upload`]: the bytes (a whole file or one part) go straight to the
//!    storage behind the URL.
//! 3. [`Client::complete_multipart_uploads`]: `POST /channels/{id}/attachments/complete`
//!    for every multipart upload.
//! 4. [`Client::send_with_uploads`]: Create message with the uploaded files referenced
//!    by their `upload_filename`.
//!
//! The upload URLs carry a signature, so they are secrets of their own:
//! [`UploadUrl`] never prints them, the token never goes with them (storage gets no
//! `Authorization` header and none of the instance's headers), redirects are never
//! followed, and the answer is read up to 64 KiB. Every `PUT` waits for the account's
//! rate limiter like an API call. A plan is checked before anything is uploaded: one
//! plan per file, the sizes asked for, and parts that cover the file exactly.

use std::ops::Range;
use std::time::Duration;

use bytes::Bytes;
use reqwest::Method;
use serde_json::{json, Value};
use url::Url;

use crate::error::{Error, Result};
use crate::instance::{read_capped, require_scheme};
use crate::models::{Message, OutgoingMessage};
use crate::rest::{id, Client};
use crate::upload::{safe_content_type, safe_filename, single_body, ProgressFn};

/// The largest file the server uploads with a single `PUT` (Fluxer's
/// `ATTACHMENT_UPLOAD_CHUNK_THRESHOLD`); larger ones come in parts.
pub const SINGLEPART_MAX: u64 = 10 * 1024 * 1024;
/// The smallest part a plan may ask for (every part but the last). Fluxer uses at
/// least 10 MiB; storage refuses parts under 5 MiB.
pub const MIN_PART_SIZE: u64 = 5 * 1024 * 1024;
/// The largest part a plan may ask for. The part is held in memory while it is sent.
pub const MAX_PART_SIZE: u64 = 64 * 1024 * 1024;
/// Most parts of one file (Fluxer's `ATTACHMENT_UPLOAD_MAX_CHUNKS`).
pub const MAX_PARTS: usize = 10_000;
/// Most bytes read of the storage's answer to a `PUT`.
const MAX_PUT_ANSWER: usize = 64 * 1024;
/// Longest upload URL accepted.
const MAX_URL_LEN: usize = 8192;
/// The limiter route of every storage `PUT`.
const PUT_ROUTE: &str = "PUT /attachments/storage";

/// A signed upload URL. Its `Debug` shows only the host: the query is a credential.
#[derive(Clone, PartialEq, Eq)]
pub struct UploadUrl(Url);

impl UploadUrl {
    /// Checks a URL from a plan: `https`, a host, no user name or password, at most
    /// 8 KiB. Plain `http` to a loopback host is accepted only in tests and in
    /// builds with `test-extra-ca` (never in a release build), so an instance
    /// cannot make the client send a file to a service on this machine.
    pub fn parse(raw: &str) -> Result<UploadUrl> {
        if raw.len() > MAX_URL_LEN {
            return Err(Error::Url("an upload URL is too long".into()));
        }
        let insecure = if cfg!(any(test, feature = "test-extra-ca")) {
            "http"
        } else {
            "https"
        };
        let url = require_scheme(raw, "https", insecure, "upload URL")
            .map_err(|_| Error::Url("an upload URL is not https".into()))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::Url("an upload URL carries a user name".into()));
        }
        Ok(UploadUrl(url))
    }

    /// The host the bytes go to.
    pub fn host(&self) -> &str {
        self.0.host_str().unwrap_or("")
    }
}

impl std::fmt::Debug for UploadUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UploadUrl({}, signature redacted)", self.host())
    }
}

/// One file to plan an upload for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresignFile {
    /// The file's index in the message (0, 1, …).
    pub id: u32,
    /// Sent through [`safe_filename`].
    pub filename: String,
    /// The exact number of bytes that will be uploaded.
    pub file_size: u64,
    /// Sent through [`safe_content_type`].
    pub content_type: String,
}

/// How one file goes to storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadMode {
    /// The whole file with one `PUT`.
    Single(UploadUrl),
    /// Parts of `part_size` bytes (the last one shorter), in order, then
    /// [`Client::complete_multipart_uploads`].
    Multipart {
        upload_id: String,
        part_size: u64,
        parts: Vec<UploadUrl>,
    },
}

/// The server's plan for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadPlan {
    pub id: u32,
    /// The storage key; the message refers to the file by it.
    pub upload_filename: String,
    pub file_size: u64,
    /// What the server derived from the name.
    pub content_type: String,
    pub mode: UploadMode,
}

impl UploadPlan {
    /// The byte ranges of the parts, in order: one range for a single upload.
    pub fn part_ranges(&self) -> Vec<Range<u64>> {
        match &self.mode {
            UploadMode::Single(_) => std::iter::once(0..self.file_size).collect(),
            UploadMode::Multipart {
                part_size, parts, ..
            } => (0..parts.len() as u64)
                .map(|i| {
                    let start = i * part_size;
                    start..(start + part_size).min(self.file_size)
                })
                .collect(),
        }
    }

    /// The URL of part `index` (0-based).
    pub fn part_url(&self, index: usize) -> Option<&UploadUrl> {
        match &self.mode {
            UploadMode::Single(url) => (index == 0).then_some(url),
            UploadMode::Multipart { parts, .. } => parts.get(index),
        }
    }
}

/// A file in storage, ready to be attached to a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedFile {
    /// The file's index in the message.
    pub id: u32,
    /// The name shown in the message (sent through [`safe_filename`]).
    pub filename: String,
    /// From the plan.
    pub upload_filename: String,
    pub file_size: u64,
    pub content_type: String,
    /// Alt text.
    pub description: Option<String>,
    pub spoiler: bool,
}

impl UploadedFile {
    /// The attachment for a plan and the file's own name, alt text and spoiler flag.
    pub fn of(
        plan: &UploadPlan,
        filename: &str,
        description: Option<String>,
        spoiler: bool,
    ) -> Self {
        UploadedFile {
            id: plan.id,
            filename: filename.to_owned(),
            upload_filename: plan.upload_filename.clone(),
            file_size: plan.file_size,
            content_type: plan.content_type.clone(),
            description,
            spoiler,
        }
    }

    fn to_json(&self) -> Value {
        let mut v = json!({
            "id": self.id,
            "filename": safe_filename(&self.filename),
            "upload_filename": self.upload_filename,
            "file_size": self.file_size,
            "content_type": safe_content_type(Some(&self.content_type)),
        });
        if let Some(d) = self
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            v["description"] = json!(d.chars().take(4096).collect::<String>());
        }
        if self.spoiler {
            v["flags"] = json!(crate::models::attachment_flags::IS_SPOILER);
        }
        v
    }
}

fn bad(why: &str) -> Error {
    Error::Decode(format!("attachment upload plan: {why}"))
}

fn u64_of(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn short_text(v: &Value, max: usize) -> Option<String> {
    v.as_str()
        .filter(|s| !s.is_empty() && s.len() <= max)
        .map(str::to_owned)
}

/// Reads and checks one plan against the file it is for.
fn parse_plan(v: &Value, asked: &PresignFile) -> Result<UploadPlan> {
    let upload_filename =
        short_text(&v["upload_filename"], 4096).ok_or_else(|| bad("no upload_filename"))?;
    let file_size = u64_of(&v["file_size"]).ok_or_else(|| bad("no file_size"))?;
    if file_size != asked.file_size {
        return Err(bad("the size differs from the file"));
    }
    let content_type = v["content_type"]
        .as_str()
        .map(|ct| safe_content_type(Some(ct)))
        .unwrap_or_else(|| asked.content_type.clone());
    let mode = match v["upload_mode"].as_str() {
        Some("singlepart") => {
            // One request holds the whole file: only for a part-sized file.
            if file_size > MAX_PART_SIZE {
                return Err(bad("a single upload for a file larger than a part"));
            }
            let url = v["upload_url"]
                .as_str()
                .ok_or_else(|| bad("no upload_url"))?;
            UploadMode::Single(UploadUrl::parse(url)?)
        }
        Some("multipart") => {
            let upload_id = short_text(&v["upload_id"], 1024).ok_or_else(|| bad("no upload_id"))?;
            let part_size = u64_of(&v["part_size"]).ok_or_else(|| bad("no part_size"))?;
            if !(MIN_PART_SIZE..=MAX_PART_SIZE).contains(&part_size) {
                return Err(bad("the part size is out of range"));
            }
            let list = v["parts"].as_array().ok_or_else(|| bad("no parts"))?;
            let expected = file_size.div_ceil(part_size);
            if list.is_empty() || list.len() > MAX_PARTS || list.len() as u64 != expected {
                return Err(bad("the parts do not cover the file"));
            }
            let mut parts = Vec::with_capacity(list.len());
            for (i, part) in list.iter().enumerate() {
                if u64_of(&part["part_number"]) != Some(i as u64 + 1) {
                    return Err(bad("the parts are not numbered in order"));
                }
                let url = part["upload_url"]
                    .as_str()
                    .ok_or_else(|| bad("a part has no upload_url"))?;
                parts.push(UploadUrl::parse(url)?);
            }
            UploadMode::Multipart {
                upload_id,
                part_size,
                parts,
            }
        }
        _ => return Err(bad("unknown upload_mode")),
    };
    Ok(UploadPlan {
        id: asked.id,
        upload_filename,
        file_size,
        content_type,
        mode,
    })
}

/// One plan per file asked for, in the order asked.
fn parse_plans(body: &Value, asked: &[PresignFile]) -> Result<Vec<UploadPlan>> {
    let list = body["attachments"]
        .as_array()
        .ok_or_else(|| bad("no attachments"))?;
    if list.len() != asked.len() {
        return Err(bad("not one plan per file"));
    }
    asked
        .iter()
        .map(|file| {
            let mut matching = list
                .iter()
                .filter(|v| u64_of(&v["id"]) == Some(u64::from(file.id)));
            match (matching.next(), matching.next()) {
                (Some(v), None) => parse_plan(v, file),
                _ => Err(bad("not one plan per file")),
            }
        })
        .collect()
}

/// Time allowed for one `PUT`: 2 minutes plus the time at 1 Mbit/s.
fn put_timeout(len: u64) -> Duration {
    Duration::from_secs(120u64.saturating_add(len / (128 * 1024)).min(90 * 60))
}

impl Client {
    /// `POST /channels/{id}/attachments`: plans the upload of `files`. Checks the
    /// count and sizes against [`Client::upload_limits`] first, and every plan before
    /// it is returned (see the module docs).
    pub async fn presign_attachments(
        &self,
        channel_id: &str,
        files: &[PresignFile],
    ) -> Result<Vec<UploadPlan>> {
        let channel_id = id(channel_id)?;
        let limits = self.upload_limits();
        if files.is_empty() || files.len() as u64 > limits.max_files {
            return Err(Error::InvalidInput(format!(
                "{} files; this account can attach 1 to {} per message",
                files.len(),
                limits.max_files
            )));
        }
        if files.iter().any(|f| f.file_size > limits.max_file_size) {
            return Err(Error::TooLarge {
                limit: usize::try_from(limits.max_file_size).unwrap_or(usize::MAX),
            });
        }
        let mut asked: Vec<PresignFile> = files.to_vec();
        for f in &mut asked {
            f.filename = safe_filename(&f.filename);
            f.content_type = safe_content_type(Some(&f.content_type));
        }
        let body = json!({
            "attachments": asked.iter().map(|f| json!({
                "id": f.id,
                "filename": f.filename,
                "file_size": f.file_size,
                "content_type": f.content_type,
            })).collect::<Vec<_>>()
        });
        let answer = self
            .request_json(
                Method::POST,
                &format!("/channels/{channel_id}/attachments"),
                Some(&body),
            )
            .await?;
        parse_plans(&answer, &asked)
    }

    /// `PUT`s `data` (a whole file or one part) to a signed storage URL, after the
    /// account's rate limiter. No token and no instance headers go with it, a
    /// redirect is an error, and the answer is read up to 64 KiB. A 429 comes back as
    /// [`Error::RateLimited`] and is not retried here. `progress` gets
    /// `(sent, total)` bytes of this request.
    pub async fn put_upload(
        &self,
        url: &UploadUrl,
        data: Bytes,
        content_type: &str,
        progress: Option<ProgressFn>,
    ) -> Result<()> {
        self.rate_limiter().acquire(PUT_ROUTE).await;
        let len = data.len() as u64;
        let resp = self
            .instance()
            .inner
            .http
            .request(Method::PUT, url.0.clone())
            .header("Content-Type", safe_content_type(Some(content_type)))
            .header("Content-Length", len)
            .timeout(put_timeout(len))
            .body(single_body(data, progress))
            .send()
            .await?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<f64>().ok());
        let body = read_capped(resp, MAX_PUT_ANSWER).await.unwrap_or_default();
        match status {
            200..=299 => Ok(()),
            300..=399 => Err(Error::Decode(format!(
                "storage answered with a redirect (HTTP {status}); not following it"
            ))),
            429 => Err(Error::RateLimited {
                retry_after: crate::ratelimit::clamp_server_secs(
                    retry_after.unwrap_or(1.0),
                    Duration::from_secs(60),
                )
                .max(Duration::from_secs(1)),
            }),
            _ => {
                // Storage errors are XML or text; keep a short excerpt only.
                let message: String = String::from_utf8_lossy(&body).chars().take(200).collect();
                Err(Error::Api(Box::new(crate::ApiError {
                    status,
                    code: String::new(),
                    message,
                    errors: None,
                })))
            }
        }
    }

    /// `POST /channels/{id}/attachments/complete`: finishes multipart uploads, given
    /// as `(upload_filename, upload_id)`.
    pub async fn complete_multipart_uploads(
        &self,
        channel_id: &str,
        uploads: &[(String, String)],
    ) -> Result<()> {
        let channel_id = id(channel_id)?;
        if uploads.is_empty() {
            return Ok(());
        }
        let body = json!({
            "uploads": uploads.iter().map(|(name, upload_id)| json!({
                "upload_filename": name,
                "upload_id": upload_id,
            })).collect::<Vec<_>>()
        });
        self.request_json(
            Method::POST,
            &format!("/channels/{channel_id}/attachments/complete"),
            Some(&body),
        )
        .await?;
        Ok(())
    }

    /// `POST /channels/{id}/messages` with files already in storage. Like
    /// [`Client::send`] otherwise (same limiter, same nonce handling).
    pub async fn send_with_uploads(
        &self,
        channel_id: &str,
        message: &OutgoingMessage,
        files: &[UploadedFile],
    ) -> Result<Message> {
        let channel_id = id(channel_id)?;
        let limits = self.upload_limits();
        if files.len() as u64 > limits.max_files {
            return Err(Error::InvalidInput(format!(
                "{} files; this account can attach {} per message",
                files.len(),
                limits.max_files
            )));
        }
        let attachments = Value::Array(files.iter().map(UploadedFile::to_json).collect());
        let body = message.to_json(Some(attachments))?;
        self.rate_limiter().coalesce.reset_typing(channel_id);
        self.request_json(
            Method::POST,
            &format!("/channels/{channel_id}/messages"),
            Some(&body),
        )
        .await
        .and_then(|v| serde_json::from_value(v).map_err(|e| Error::Decode(e.to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn asked(size: u64) -> PresignFile {
        PresignFile {
            id: 0,
            filename: "big.bin".into(),
            file_size: size,
            content_type: "application/octet-stream".into(),
        }
    }

    fn multipart(size: u64, part_size: u64, parts: usize) -> Value {
        json!({"attachments": [{
            "upload_mode": "multipart", "id": "0", "filename": "big.bin",
            "upload_filename": "k", "file_size": size, "content_type": "application/octet-stream",
            "upload_id": "u", "part_size": part_size,
            "parts": (1..=parts).map(|n| json!({"part_number": n, "upload_url": format!("https://s3.example/k?partNumber={n}&X-Amz-Signature=secret")})).collect::<Vec<_>>()
        }]})
    }

    #[test]
    fn a_plan_must_cover_the_file_exactly() {
        let size = 25 * MIB;
        let plans = parse_plans(&multipart(size, 10 * MIB, 3), &[asked(size)]).unwrap();
        assert_eq!(
            plans[0].part_ranges(),
            vec![0..10 * MIB, 10 * MIB..20 * MIB, 20 * MIB..25 * MIB]
        );
        // Too few or too many parts, a tiny part size, a size that differs.
        assert!(parse_plans(&multipart(size, 10 * MIB, 2), &[asked(size)]).is_err());
        assert!(parse_plans(&multipart(size, 10 * MIB, 4), &[asked(size)]).is_err());
        assert!(parse_plans(&multipart(size, MIB, 25), &[asked(size)]).is_err());
        assert!(parse_plans(&multipart(size, 10 * MIB, 3), &[asked(size + 1)]).is_err());
        // Two plans for the same file, or none.
        let twice = json!({"attachments": [
            {"upload_mode": "singlepart", "id": 0, "upload_filename": "a", "file_size": 3, "upload_url": "https://s3.example/a"},
            {"upload_mode": "singlepart", "id": 0, "upload_filename": "b", "file_size": 3, "upload_url": "https://s3.example/b"}
        ]});
        assert!(parse_plans(&twice, &[asked(3), PresignFile { id: 1, ..asked(3) }]).is_err());
    }

    #[test]
    fn upload_urls_are_checked_and_never_printed() {
        let url = UploadUrl::parse("https://s3.example/k?X-Amz-Signature=secret").unwrap();
        let shown = format!("{url:?}");
        assert!(
            !shown.contains("secret") && shown.contains("s3.example"),
            "{shown}"
        );
        for bad in [
            "http://s3.example/k",
            "https://user:pw@s3.example/k",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(UploadUrl::parse(bad).is_err(), "{bad}");
        }
        assert!(
            UploadUrl::parse("http://127.0.0.1:9000/k").is_ok(),
            "loopback for tests"
        );
    }

    #[test]
    fn uploaded_files_carry_the_key_alt_text_and_spoiler() {
        let f = UploadedFile {
            id: 1,
            filename: "C:\\Users\\me\\cat\u{202e}.png".into(),
            upload_filename: "key".into(),
            file_size: 3,
            content_type: "image/png".into(),
            description: Some("  a cat ".into()),
            spoiler: true,
        };
        let v = f.to_json();
        assert_eq!(v["filename"], "cat.png");
        assert_eq!(v["upload_filename"], "key");
        assert_eq!(v["description"], "a cat");
        assert_eq!(v["flags"], 8);
    }

    #[test]
    fn a_single_upload_is_only_planned_for_a_part_sized_file() {
        let size = 200 * MIB;
        let single = json!({"attachments": [{
            "upload_mode": "singlepart", "id": 0, "upload_filename": "k",
            "upload_url": "https://s3.example/k", "file_size": size
        }]});
        assert!(parse_plans(&single, &[asked(size)]).is_err());
    }

    /// The calls against a mock instance and a mock storage.
    mod wire {
        use bytes::Bytes;
        use serde_json::{json, Value};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        use crate::{
            Client, ClientInfo, Error, Instance, OutgoingMessage, PresignFile, Token, UploadUrl,
            UploadedFile,
        };

        async fn instance() -> (MockServer, Instance) {
            let server = MockServer::start().await;
            let base = server.uri();
            let doc = json!({
                "api_code_version": 1,
                "endpoints": {
                    "api": format!("{base}/api"), "api_client": format!("{base}/api"),
                    "api_public": format!("{base}/api"), "gateway": "ws://127.0.0.1:1/gateway",
                    "media": format!("{base}/media"), "webapp": base,
                    "invite": format!("{base}/invite")
                },
                "captcha": {"provider": "altcha"},
                "features": {"account_identity": "email"},
                "limits": {"version": 2, "rules": [{"id": "default", "overrides": {}}]},
                "app_public": {"branding": {"product_name": "Mock"}},
                "sso": {"enabled": false},
                "domain_migration": {"enabled": false}
            });
            Mock::given(method("GET"))
                .and(path("/.well-known/fluxer"))
                .respond_with(ResponseTemplate::new(200).set_body_json(doc))
                .mount(&server)
                .await;
            let info = ClientInfo {
                installation_id: "00000000-0000-4000-a000-000000000000".into(),
                ..ClientInfo::default()
            };
            let instance = Instance::discover_with(&server.uri(), info)
                .await
                .expect("discovery");
            (server, instance)
        }

        fn file(id: u32, name: &str, size: u64) -> PresignFile {
            PresignFile {
                id,
                filename: name.into(),
                file_size: size,
                content_type: "image/png".into(),
            }
        }

        #[tokio::test]
        async fn a_file_goes_to_storage_without_the_token_and_the_message_names_its_key() {
            let (server, instance) = instance().await;
            let storage = format!("{}/storage/key-1?X-Amz-Signature=secret", server.uri());
            Mock::given(method("POST"))
                .and(path("/api/v1/channels/301/attachments"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"attachments": [{
                        "upload_mode": "singlepart", "id": 0, "filename": "cat.png",
                        "upload_filename": "key-1", "upload_url": storage,
                        "file_size": 3, "content_type": "image/png"
                    }]})),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path("/storage/key-1"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v1/channels/301/messages"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"id": "900", "channel_id": "301", "author": {"id": "10", "username": "ana"}, "content": "look"})),
                )
                .expect(1)
                .mount(&server)
                .await;

            let client = Client::new(instance, Token::new("tok-presigned-1"));
            // A full local path is never sent.
            let plans = client
                .presign_attachments("301", &[file(0, "C:\\Users\\me\\Pictures\\cat.png", 3)])
                .await
                .unwrap();
            assert_eq!(plans.len(), 1);
            let url = plans[0].part_url(0).unwrap();
            assert!(!format!("{url:?}").contains("secret"));
            client
                .put_upload(url, Bytes::from_static(b"abc"), "image/png", None)
                .await
                .unwrap();
            let uploaded = UploadedFile::of(&plans[0], "cat.png", Some("a cat".into()), true);
            let message = OutgoingMessage::text("look").with_nonce("777");
            client
                .send_with_uploads("301", &message, &[uploaded])
                .await
                .unwrap();

            let requests: Vec<Request> = server.received_requests().await.unwrap();
            let presign = requests
                .iter()
                .find(|r| r.url.path() == "/api/v1/channels/301/attachments")
                .unwrap();
            let asked: Value = serde_json::from_slice(&presign.body).unwrap();
            assert_eq!(asked["attachments"][0]["filename"], "cat.png");
            assert_eq!(asked["attachments"][0]["file_size"], 3);
            let put = requests
                .iter()
                .find(|r| r.method.as_str() == "PUT")
                .unwrap();
            assert_eq!(put.body, b"abc");
            assert!(
                put.headers.get("authorization").is_none(),
                "no token to storage"
            );
            assert!(
                put.headers.get("x-fluxer-client-installation-id").is_none(),
                "no instance headers to storage"
            );
            let post = requests
                .iter()
                .find(|r| r.url.path() == "/api/v1/channels/301/messages")
                .unwrap();
            let sent: Value = serde_json::from_slice(&post.body).unwrap();
            assert_eq!(sent["nonce"], "777");
            assert_eq!(
                sent["attachments"],
                json!([{"id": 0, "filename": "cat.png", "upload_filename": "key-1", "file_size": 3,
                        "content_type": "image/png", "description": "a cat", "flags": 8}])
            );
        }

        #[tokio::test]
        async fn storage_redirects_are_not_followed_and_multipart_is_completed() {
            let (server, instance) = instance().await;
            Mock::given(method("PUT"))
                .and(path("/storage/moved"))
                .respond_with(
                    ResponseTemplate::new(307)
                        .insert_header("Location", format!("{}/elsewhere", server.uri())),
                )
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v1/channels/301/attachments/complete"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"uploads": [{"upload_filename": "k"}]})),
                )
                .expect(1)
                .mount(&server)
                .await;
            let client = Client::new(instance, Token::new("tok-presigned-2"));
            let url = UploadUrl::parse(&format!("{}/storage/moved", server.uri())).unwrap();
            let err = client
                .put_upload(&url, Bytes::from_static(b"x"), "image/png", None)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Decode(_)), "{err:?}");
            let requests = server.received_requests().await.unwrap();
            assert!(
                !requests.iter().any(|r| r.url.path() == "/elsewhere"),
                "the redirect was not followed"
            );
            client
                .complete_multipart_uploads("301", &[("k".into(), "u".into())])
                .await
                .unwrap();
            let complete = server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .find(|r| r.url.path().ends_with("/complete"))
                .unwrap();
            let body: Value = serde_json::from_slice(&complete.body).unwrap();
            assert_eq!(
                body,
                json!({"uploads": [{"upload_filename": "k", "upload_id": "u"}]})
            );
        }

        #[tokio::test]
        async fn a_plan_over_the_limits_is_refused_before_any_request() {
            let (server, instance) = instance().await;
            let client = Client::new(instance, Token::new("tok-presigned-3"));
            let too_big = client.upload_limits().max_file_size + 1;
            let err = client
                .presign_attachments("301", &[file(0, "huge.bin", too_big)])
                .await
                .unwrap_err();
            assert!(matches!(err, Error::TooLarge { .. }), "{err:?}");
            let many: Vec<PresignFile> = (0..11).map(|i| file(i, "a.png", 1)).collect();
            assert!(client.presign_attachments("301", &many).await.is_err());
            let requests = server.received_requests().await.unwrap();
            assert!(
                requests
                    .iter()
                    .all(|r| r.url.path() == "/.well-known/fluxer"),
                "nothing but discovery was asked"
            );
        }
    }
}
