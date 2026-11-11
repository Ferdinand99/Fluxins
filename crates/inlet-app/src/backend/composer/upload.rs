// SPDX-License-Identifier: AGPL-3.0-or-later
//! Uploading the files the person attached to a message.
//!
//! Only files the person chose reach this module: picked in the file dialog, dropped on
//! the composer, or pasted. A pasted image is only ever in memory ([`Source::Memory`]);
//! nothing here writes a file. Names are cleaned with the protocol's
//! [`p::safe_filename`] (last path component, no control or direction characters).
//!
//! An upload is a send with files: the same nonce, the same pending row in the message
//! list, the same Retry. Steps:
//! 1. The count and sizes are checked against the account's limits before any request.
//! 2. Photo metadata is removed from JPEG, PNG and WebP files when the person left that
//!    on ([`super::exif`]).
//! 3. With `presigned_attachment_uploads`, each file goes straight to storage: one `PUT`
//!    up to 10 MiB, parts of the size the server plans (10 MiB and up) above that, then
//!    the message names the stored files. A file from disk is read one part at a time.
//!    Without the feature, or when planning fails and the files are small enough
//!    (25 MiB together), they go in one `multipart/form-data` message.
//! 4. Only the final message `POST` can leave it unclear whether the message exists;
//!    a failure before it never does. Retry uploads again with the same nonce.
//!
//! Memory: uploads run one at a time. Bytes held for an upload (pasted images, files
//! read for metadata removal or for a form upload, the part on its way) are counted
//! in a [`Gauge`] and freed when the upload ends or is cancelled, or when the session
//! ends. Failed uploads keep their files for Retry up to [`KEEP_BYTES`];
//! older ones then lose theirs, and Retry asks to attach them again.
//!
//! Request budget: one plan request per message, one `PUT` per part (a part that fails
//! for a passing reason is tried twice more, after 1 and 2 s or the wait storage asks
//! for, at most 30 s), one complete request for files in parts, and the message. Every
//! request waits for the account's rate limiter.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::future::BoxFuture;
use inlet_protocol as p;
use tokio::task::AbortHandle;

use super::exif;
use super::send::{failed, not_connected, report_failure, Ctx, Outgoing, SendError};
use super::Ev;
use crate::backend::{Backend, RealActive, Ui};

const MIB: u64 = 1024 * 1024;
/// Files kept for Retry may hold this much memory together.
pub const KEEP_BYTES: u64 = 64 * MIB;
/// Most failed uploads kept for Retry with their files.
const KEEP_UPLOADS: usize = 20;
/// The largest message sent as a form upload when planning a pre-signed upload failed
/// (Fluxer's fallback limit).
const FORM_FALLBACK_MAX: u64 = 25 * MIB;
/// Attempts per part.
const PART_ATTEMPTS: u32 = 3;
/// Longest wait between attempts.
const MAX_PART_WAIT: Duration = Duration::from_secs(30);

/// Where a file's bytes are.
#[derive(Clone, PartialEq, Eq)]
pub enum Source {
    /// A file the person picked or dropped. It is read when it is sent.
    Path(PathBuf),
    /// An image pasted from the clipboard. It is never written to disk.
    Memory(Bytes),
}

/// A file to attach, as the person set it up.
#[derive(Clone, PartialEq, Eq)]
pub struct FileToSend {
    /// Cleaned with [`p::safe_filename`].
    pub name: String,
    /// The size when it was attached.
    pub size: u64,
    pub content_type: String,
    pub spoiler: bool,
    /// Alt text.
    pub description: Option<String>,
    pub source: Source,
}

impl std::fmt::Debug for FileToSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileToSend")
            .field("name", &self.name)
            .field("size", &self.size)
            .field("content_type", &self.content_type)
            .field("spoiler", &self.spoiler)
            .field("pasted", &matches!(self.source, Source::Memory(_)))
            .finish()
    }
}

/// A message with files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    pub message: Outgoing,
    pub files: Vec<FileToSend>,
    /// Remove photo metadata first.
    pub strip_exif: bool,
}

/// The account's upload limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_file_size: u64,
    pub max_files: usize,
    /// All files of one message together.
    pub max_total: u64,
}

impl Default for Limits {
    /// Fluxer's defaults, until the account's are known.
    fn default() -> Self {
        Limits::of(p::UploadLimits {
            max_file_size: 25 * MIB,
            max_files: 10,
            max_total_size: 250 * MIB,
        })
    }
}

/// Why files cannot be attached or sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    Count { max: usize },
    FileSize { name: String, max: u64 },
    Total { max: u64 },
}

impl Refused {
    pub fn reason(&self) -> String {
        let size = crate::ui::media::file_size;
        match self {
            Refused::Count { max } => t!("composer-upload-too-many", max = *max),
            Refused::FileSize { name, max } => {
                t!(
                    "composer-upload-too-large",
                    name = name.as_str(),
                    max = size(*max)
                )
            }
            Refused::Total { max } => t!("composer-upload-too-much", max = size(*max)),
        }
    }
}

impl Limits {
    pub fn of(l: p::UploadLimits) -> Self {
        Limits {
            max_file_size: l.max_file_size,
            max_files: usize::try_from(l.max_files).unwrap_or(usize::MAX),
            max_total: l.max_total_size,
        }
    }

    /// Whether files of these names and sizes may go in one message.
    pub fn check<'a>(
        &self,
        files: impl IntoIterator<Item = (&'a str, u64)>,
    ) -> Result<(), Refused> {
        let mut count = 0usize;
        let mut total = 0u64;
        for (name, size) in files {
            count += 1;
            if size > self.max_file_size {
                return Err(Refused::FileSize {
                    name: name.to_owned(),
                    max: self.max_file_size,
                });
            }
            total = total.saturating_add(size);
        }
        if count > self.max_files {
            return Err(Refused::Count {
                max: self.max_files,
            });
        }
        if total > self.max_total {
            return Err(Refused::Total {
                max: self.max_total,
            });
        }
        Ok(())
    }
}

/// Bytes held by uploads.
#[derive(Clone, Default)]
pub struct Gauge(Arc<AtomicU64>);

impl Gauge {
    #[cfg(test)]
    pub fn bytes(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Bytes counted in a [`Gauge`] while any clone of this lives.
#[derive(Clone)]
struct Held(Arc<HeldInner>);

struct HeldInner {
    data: Bytes,
    gauge: Gauge,
}

impl Held {
    fn new(data: Bytes, gauge: &Gauge) -> Held {
        gauge.0.fetch_add(data.len() as u64, Ordering::SeqCst);
        Held(Arc::new(HeldInner {
            data,
            gauge: gauge.clone(),
        }))
    }

    fn data(&self) -> &Bytes {
        &self.0.data
    }
}

impl Drop for HeldInner {
    fn drop(&mut self) {
        self.gauge
            .0
            .fetch_sub(self.data.len() as u64, Ordering::SeqCst);
    }
}

/// Reading picked files: the only access to the disk this module has. It reads, it
/// never writes.
pub trait Files: Send + Sync {
    /// The size of a regular file (not a folder, device or pipe).
    fn len(&self, path: &Path) -> std::io::Result<u64>;
    /// `range` of a file that must still be `len` bytes long.
    fn read(&self, path: &Path, len: u64, range: Range<u64>) -> std::io::Result<Bytes>;
}

/// The real disk.
pub struct Disk;

impl Files for Disk {
    fn len(&self, path: &Path) -> std::io::Result<u64> {
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        Ok(meta.len())
    }

    fn read(&self, path: &Path, len: u64, range: Range<u64>) -> std::io::Result<Bytes> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() != len || range.end > len || range.start > range.end {
            return Err(std::io::Error::other("the file changed"));
        }
        file.seek(SeekFrom::Start(range.start))?;
        let mut buf = vec![0; (range.end - range.start) as usize];
        file.read_exact(&mut buf)?;
        Ok(Bytes::from(buf))
    }
}

/// A file's bytes inside an upload job.
#[derive(Clone)]
enum Data {
    Disk(PathBuf),
    Memory(Held),
}

struct JobFile {
    file: FileToSend,
    data: Data,
}

/// An upload as the backend keeps it (for its task, and for Retry).
pub(super) struct Job {
    pub message: Outgoing,
    files: Vec<JobFile>,
    strip_exif: bool,
}

impl Job {
    fn names(&self) -> Vec<String> {
        self.files.iter().map(|f| f.file.name.clone()).collect()
    }
}

/// A file ready to go: in memory, or read part by part from disk.
struct Ready {
    name: String,
    content_type: String,
    spoiler: bool,
    description: Option<String>,
    len: u64,
    data: Data,
}

enum Kept {
    Job(Arc<Job>),
    /// Its files were let go to save memory: Retry asks to attach them again.
    Gone,
}

struct Entry {
    nonce: String,
    gen: u64,
    /// The file names, for the row of an upload whose files are gone.
    names: Vec<String>,
    kept: Kept,
    task: Option<AbortHandle>,
    running: bool,
}

#[derive(Default)]
struct Book {
    entries: Vec<Entry>,
}

/// The backend's uploads: running ones, and failed ones kept for Retry.
#[derive(Clone)]
pub struct Uploads {
    book: Arc<Mutex<Book>>,
    gauge: Gauge,
    turn: Arc<tokio::sync::Semaphore>,
    files: Arc<dyn Files>,
}

impl Default for Uploads {
    fn default() -> Self {
        Uploads::with_files(Arc::new(Disk))
    }
}

/// What a Retry of an upload finds.
pub(super) enum Retry {
    Job(Arc<Job>),
    /// The files were let go; these were their names.
    Gone(Vec<String>),
}

impl Uploads {
    pub fn with_files(files: Arc<dyn Files>) -> Self {
        Uploads {
            book: Arc::default(),
            gauge: Gauge::default(),
            turn: Arc::new(tokio::sync::Semaphore::new(1)),
            files,
        }
    }

    fn book(&self) -> std::sync::MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Bytes held by uploads now.
    #[cfg(test)]
    pub fn held_bytes(&self) -> u64 {
        self.gauge.bytes()
    }

    /// Takes an upload in for the account session `gen`.
    pub(super) fn take_in(&self, gen: u64, upload: Upload) -> Arc<Job> {
        let files = upload
            .files
            .into_iter()
            .map(|mut file| {
                let source = std::mem::replace(&mut file.source, Source::Memory(Bytes::new()));
                let data = match source {
                    Source::Path(path) => Data::Disk(path),
                    // The job holds the counted handle only.
                    Source::Memory(bytes) => Data::Memory(Held::new(bytes, &self.gauge)),
                };
                JobFile { file, data }
            })
            .collect();
        let job = Arc::new(Job {
            message: upload.message,
            files,
            strip_exif: upload.strip_exif,
        });
        let mut book = self.book();
        book.entries.retain(|e| e.nonce != job.message.nonce);
        book.entries.push(Entry {
            nonce: job.message.nonce.clone(),
            gen,
            names: job.names(),
            kept: Kept::Job(job.clone()),
            task: None,
            running: true,
        });
        job
    }

    /// The upload a Retry of `nonce` sends again in session `gen`, if `nonce` was an
    /// upload. An upload of another session is `Gone`: it is never sent from another
    /// account, and never without its files.
    pub(super) fn retry_of(&self, gen: u64, nonce: &str) -> Option<Retry> {
        let mut book = self.book();
        let e = book.entries.iter_mut().find(|e| e.nonce == nonce)?;
        if e.gen != gen {
            return Some(Retry::Gone(e.names.clone()));
        }
        Some(match &e.kept {
            Kept::Job(job) => {
                e.running = true;
                Retry::Job(job.clone())
            }
            Kept::Gone => Retry::Gone(e.names.clone()),
        })
    }

    /// Runs `job` as a task made by `spawn`, and remembers the task for Cancel.
    pub(super) fn launch(
        &self,
        ctx: Ctx,
        job: Arc<Job>,
        look_since_ms: Option<u64>,
        spawn: impl FnOnce(BoxFuture<'static, ()>) -> AbortHandle,
    ) {
        let nonce = job.message.nonce.clone();
        let task = spawn(Box::pin(self.clone().run(ctx, job, look_since_ms)));
        if let Some(e) = self.book().entries.iter_mut().find(|e| e.nonce == nonce) {
            if e.running {
                e.task = Some(task);
            }
        }
    }

    /// Cancel: the upload stops where it is and lets go of its files. Whether one was
    /// there.
    pub fn cancel(&self, nonce: &str) -> bool {
        let entry = {
            let mut book = self.book();
            let ix = book.entries.iter().position(|e| e.nonce == nonce);
            ix.map(|ix| book.entries.remove(ix))
        };
        match entry {
            Some(e) => {
                if let Some(t) = &e.task {
                    t.abort();
                }
                true
            }
            None => false,
        }
    }

    /// The account of session `gen` is no longer the active one: its running uploads
    /// stop where they are (they never go on behind another account). Their files are
    /// kept for Retry while the limits allow. Returns the stopped messages and their
    /// file names, to show them as failed.
    pub(super) fn stop_gen(&self, gen: u64) -> Vec<(Outgoing, Vec<String>)> {
        let mut book = self.book();
        let mut stopped = Vec::new();
        for e in book
            .entries
            .iter_mut()
            .filter(|e| e.gen == gen && e.running)
        {
            if let Some(t) = e.task.take() {
                t.abort();
            }
            e.running = false;
            if let Kept::Job(job) = &e.kept {
                stopped.push((job.message.clone(), e.names.clone()));
            }
        }
        trim(&mut book);
        stopped
    }

    /// The session `gen` ended (sign-out, the token ended): its uploads stop and let go
    /// of their files; a Retry of one asks to attach the files again.
    pub fn forget_gen(&self, gen: u64) {
        let mut book = self.book();
        for e in book.entries.iter_mut().filter(|e| e.gen == gen) {
            if let Some(t) = e.task.take() {
                t.abort();
            }
            e.running = false;
            e.kept = Kept::Gone;
        }
        trim(&mut book);
    }

    /// The upload ended. A sent one is forgotten; a failed one is kept for Retry while
    /// the limits allow, newest first.
    fn finished(&self, nonce: &str, sent: bool) {
        let mut book = self.book();
        if sent {
            book.entries.retain(|e| e.nonce != nonce);
            return;
        }
        if let Some(e) = book.entries.iter_mut().find(|e| e.nonce == nonce) {
            e.running = false;
            e.task = None;
        }
        trim(&mut book);
    }

    /// The task of one upload: reports start, progress and the outcome.
    async fn run(self, ctx: Ctx, job: Arc<Job>, look_since_ms: Option<u64>) {
        let nonce = job.message.nonce.clone();
        let total = job.files.iter().map(|f| f.file.size).sum();
        let _ = ctx.events.unbounded_send(Ev::UploadStarted {
            nonce: nonce.clone(),
            channel_id: job.message.channel_id.clone(),
            names: job.files.iter().map(|f| f.file.name.clone()).collect(),
            total,
        });
        let outcome = {
            // One upload at a time keeps the memory of uploads to one message.
            let _turn = self.turn.clone().acquire_owned().await;
            self.attempt(&ctx, &job, look_since_ms).await
        };
        match outcome {
            Ok(()) => {
                self.finished(&nonce, true);
                ctx.sent(nonce.clone());
            }
            Err(Some(e)) => {
                self.finished(&nonce, false);
                ctx.failed_with_files(&job.message, e, job.names());
            }
            Err(None) => self.finished(&nonce, false),
        }
        let _ = ctx.events.unbounded_send(Ev::UploadDone { nonce });
    }

    async fn attempt(
        &self,
        ctx: &Ctx,
        job: &Job,
        look_since_ms: Option<u64>,
    ) -> Result<(), Option<SendError>> {
        let limits = Limits::of(ctx.client.upload_limits());
        limits
            .check(
                job.files
                    .iter()
                    .map(|f| (f.file.name.as_str(), f.file.size)),
            )
            .map_err(refused)?;
        if let Some(since) = look_since_ms {
            match ctx.already_sent(&job.message, since, job.files.len()).await {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(err) => {
                    // Still not known whether the first attempt arrived.
                    return Err(failed(&ctx.internal, ctx.gen, err).map(|mut e| {
                        e.ambiguous = true;
                        e
                    }));
                }
            }
        }
        let presigned = ctx
            .client
            .instance()
            .discovery()
            .presigned_attachment_uploads();
        let ready = self.prepare(job, &limits, presigned).await?;
        let total = ready.iter().map(|r| r.len).sum::<u64>();
        let progress = Progress::new(ctx.events.clone(), job.message.nonce.clone(), total);
        if presigned {
            match self.to_storage(ctx, job, &ready, &progress).await {
                Ok(uploaded) => return post_uploaded(ctx, job, &uploaded).await,
                Err(Step::Plan(err)) if total <= FORM_FALLBACK_MAX && !ends_session(&err) => {
                    log::warn!("planning the upload failed ({err}); sending it as a form");
                    progress.reset();
                }
                Err(Step::Plan(err) | Step::Upload(err)) => return Err(before_post(ctx, err)),
                Err(Step::Local(reason)) => return Err(Some(local(reason))),
            }
        }
        self.as_form(ctx, job, ready, &progress).await
    }

    /// Reads what must be in memory and removes metadata. A file from disk that goes
    /// to storage in parts stays on disk.
    async fn prepare(
        &self,
        job: &Job,
        limits: &Limits,
        presigned: bool,
    ) -> Result<Vec<Ready>, Option<SendError>> {
        let mut out = Vec::with_capacity(job.files.len());
        for jf in &job.files {
            let f = &jf.file;
            let strip = job.strip_exif && exif::applies_to(&f.content_type);
            let data = match &jf.data {
                Data::Memory(held) => Data::Memory(held.clone()),
                Data::Disk(path) => {
                    let len = self.disk_len(path).await?;
                    if len > limits.max_file_size {
                        return Err(refused(Refused::FileSize {
                            name: f.name.clone(),
                            max: limits.max_file_size,
                        }));
                    }
                    if strip || !presigned {
                        Data::Memory(self.read(path, len, 0..len).await?)
                    } else {
                        Data::Disk(path.clone())
                    }
                }
            };
            let data = match data {
                Data::Memory(held) if strip => {
                    let input = held.data().clone();
                    let stripped = tokio::task::spawn_blocking(move || exif::strip(&input))
                        .await
                        .ok()
                        .flatten();
                    match stripped {
                        Some(bytes) => Data::Memory(Held::new(bytes, &self.gauge)),
                        None => Data::Memory(held),
                    }
                }
                other => other,
            };
            let len = match &data {
                Data::Memory(h) => h.data().len() as u64,
                Data::Disk(path) => self.disk_len(path).await?,
            };
            out.push(Ready {
                name: f.name.clone(),
                content_type: f.content_type.clone(),
                spoiler: f.spoiler,
                description: f.description.clone(),
                len,
                data,
            });
        }
        limits
            .check(out.iter().map(|r| (r.name.as_str(), r.len)))
            .map_err(refused)?;
        Ok(out)
    }

    async fn disk_len(&self, path: &Path) -> Result<u64, Option<SendError>> {
        let files = self.files.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || files.len(&path))
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| Some(local(t!("composer-upload-unreadable"))))
    }

    async fn read(
        &self,
        path: &Path,
        len: u64,
        range: Range<u64>,
    ) -> Result<Held, Option<SendError>> {
        let files = self.files.clone();
        let path = path.to_owned();
        let bytes = tokio::task::spawn_blocking(move || files.read(&path, len, range))
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| Some(local(t!("composer-upload-file-changed"))))?;
        Ok(Held::new(bytes, &self.gauge))
    }

    /// The bytes of `range` of a ready file.
    async fn part(&self, ready: &Ready, range: Range<u64>) -> Result<Held, Step> {
        match &ready.data {
            Data::Memory(held) => {
                let end = (range.end as usize).min(held.data().len());
                let start = (range.start as usize).min(end);
                // The slice shares the held buffer, which stays counted while it lives.
                Ok(Held::new(held.data().slice(start..end), &Gauge::default()))
            }
            Data::Disk(path) => self
                .read(path, ready.len, range)
                .await
                .map_err(|e| Step::Local(e.map_or_else(String::new, |e| e.reason))),
        }
    }

    /// Plans the upload, sends every file (or part) to storage and completes the
    /// multipart ones.
    async fn to_storage(
        &self,
        ctx: &Ctx,
        job: &Job,
        ready: &[Ready],
        progress: &Progress,
    ) -> Result<Vec<p::UploadedFile>, Step> {
        let channel = &job.message.channel_id;
        let asked: Vec<p::PresignFile> = ready
            .iter()
            .enumerate()
            .map(|(i, r)| p::PresignFile {
                id: i as u32,
                filename: r.name.clone(),
                file_size: r.len,
                content_type: r.content_type.clone(),
            })
            .collect();
        let plans = ctx
            .client
            .presign_attachments(channel, &asked)
            .await
            .map_err(Step::Plan)?;
        let mut multipart = Vec::new();
        for (r, plan) in ready.iter().zip(&plans) {
            for (i, range) in plan.part_ranges().into_iter().enumerate() {
                let url = plan
                    .part_url(i)
                    .ok_or_else(|| Step::Upload(p::Error::Decode("a part has no URL".into())))?;
                let len = range.end - range.start;
                let part = self.part(r, range).await?;
                put_part(ctx, url, &part, &plan.content_type, progress).await?;
                drop(part);
                progress.add(len);
            }
            if let p::UploadMode::Multipart { upload_id, .. } = &plan.mode {
                multipart.push((plan.upload_filename.clone(), upload_id.clone()));
            }
        }
        ctx.client
            .complete_multipart_uploads(channel, &multipart)
            .await
            .map_err(Step::Upload)?;
        Ok(ready
            .iter()
            .zip(&plans)
            .map(|(r, plan)| p::UploadedFile::of(plan, &r.name, r.description.clone(), r.spoiler))
            .collect())
    }

    /// One `multipart/form-data` message with every file in it.
    async fn as_form(
        &self,
        ctx: &Ctx,
        job: &Job,
        ready: Vec<Ready>,
        progress: &Progress,
    ) -> Result<(), Option<SendError>> {
        let mut held = Vec::with_capacity(ready.len());
        let mut files = Vec::with_capacity(ready.len());
        for r in ready {
            let data = match &r.data {
                Data::Memory(h) => h.clone(),
                Data::Disk(path) => self.read(path, r.len, 0..r.len).await?,
            };
            files.push(p::UploadFile {
                filename: r.name.clone(),
                data: data.data().clone(),
                content_type: Some(r.content_type.clone()),
                description: r.description.clone(),
                spoiler: r.spoiler,
            });
            held.push(data);
        }
        let message = job
            .message
            .to_message()
            .map_err(|e| failed(&ctx.internal, ctx.gen, e.into()))?;
        let result = ctx
            .client
            .send_with_files(
                &job.message.channel_id,
                &message,
                &files,
                Some(progress.callback(0)),
            )
            .await;
        drop(files);
        drop(held);
        // The gateway's MESSAGE_CREATE brings the message into the list.
        result
            .map(|_| ())
            .map_err(|err| failed(&ctx.internal, ctx.gen, err))
    }
}

impl Backend {
    /// Sends a message with files. The UI has shown it as pending.
    pub(super) fn upload(&mut self, upload: Upload) {
        #[cfg(feature = "demo")]
        if let Some(crate::backend::Active::Demo(m)) = &mut self.active {
            // The demo posts the text; its files stay on this side.
            let nonce = upload.message.nonce.clone();
            m.resend(
                upload.message.channel_id.clone(),
                nonce.clone(),
                upload.message.content.clone(),
            );
            self.emit(Ev::UploadDone { nonce });
            return;
        }
        let Some(ctx) = self.send_ctx() else {
            let names = upload.files.iter().map(|f| f.name.clone()).collect();
            report_failure(&self.events, &upload.message, not_connected(), names);
            return;
        };
        let job = self.uploads.take_in(ctx.gen, upload);
        ctx.state.remember_send(ctx.gen, &job.message);
        self.uploads.launch(ctx, job, None, |fut| {
            self.spawn_guarded("upload", None, fut)
        });
    }

    /// The account of `r` stops being the active one: its uploads stop where they are,
    /// and their rows fail with Retry (which uploads them again once the account is
    /// active again). A stopped upload may have been posting: Retry looks first when it
    /// comes late.
    pub(in crate::backend) fn stop_uploads(&self, r: &RealActive) {
        let ctx = Ctx {
            client: r.session.client().clone(),
            events: r.events.clone(),
            internal: self.internal.clone(),
            gen: r.gen,
            state: r.composer.clone(),
        };
        for (message, names) in self.uploads.stop_gen(r.gen) {
            let mut e = local(t!("composer-upload-switched"));
            e.ambiguous = true;
            ctx.failed_with_files(&message, e, names);
        }
    }

    /// Cancel in the composer's upload list: the upload stops and its files are let go.
    pub(super) fn cancel_upload(&mut self, nonce: String) {
        self.uploads.cancel(&nonce);
        if let Some(ctx) = self.send_ctx() {
            ctx.state.forget_send(ctx.gen, &nonce);
        }
    }

    /// Retry of a failed upload: uploads it again with its first nonce. `false` when
    /// `nonce` was no upload.
    pub(super) fn upload_resend(
        &self,
        ctx: &Ctx,
        channel_id: &str,
        nonce: &str,
        text: &str,
    ) -> bool {
        match self.uploads.retry_of(ctx.gen, nonce) {
            None => false,
            Some(Retry::Gone(names)) => {
                let plan =
                    ctx.retry_plan(channel_id.into(), nonce.into(), text.into(), Instant::now());
                let gone = local(t!("composer-upload-gone"));
                report_failure(&self.events, &plan.message, gone, names);
                true
            }
            Some(Retry::Job(job)) => {
                let plan =
                    ctx.retry_plan(channel_id.into(), nonce.into(), text.into(), Instant::now());
                ctx.state.remember_send(ctx.gen, &job.message);
                self.uploads
                    .launch(ctx.clone(), job, plan.look_since_ms, |fut| {
                        self.spawn_guarded("upload", None, fut)
                    });
                true
            }
        }
    }
}

/// Failed uploads keep their files newest first, up to [`KEEP_UPLOADS`] and
/// [`KEEP_BYTES`]; older ones keep only their names, and only so many of those.
fn trim(book: &mut Book) {
    let mut kept = 0usize;
    let mut bytes = 0u64;
    for e in book.entries.iter_mut().rev() {
        if e.running {
            continue;
        }
        let Kept::Job(job) = &e.kept else {
            continue;
        };
        kept += 1;
        bytes += job
            .files
            .iter()
            .map(|f| match &f.data {
                Data::Memory(h) => h.data().len() as u64,
                Data::Disk(_) => 0,
            })
            .sum::<u64>();
        if kept > KEEP_UPLOADS || bytes > KEEP_BYTES {
            e.kept = Kept::Gone;
        }
    }
    // Rows whose files are gone are remembered a while longer, then forgotten.
    let gone = book
        .entries
        .iter()
        .filter(|e| matches!(e.kept, Kept::Gone))
        .count();
    if gone > KEEP_UPLOADS {
        let mut extra = gone - KEEP_UPLOADS;
        book.entries.retain(|e| {
            let forget = extra > 0 && matches!(e.kept, Kept::Gone);
            if forget {
                extra -= 1;
            }
            !forget
        });
    }
}

/// Why sending to storage stopped.
enum Step {
    /// The plan request failed: a form upload may still work.
    Plan(p::Error),
    /// A part, or completing the parts, failed.
    Upload(p::Error),
    /// The file could not be read.
    Local(String),
}

fn ends_session(err: &p::Error) -> bool {
    matches!(err, p::Error::Unauthorized | p::Error::GlobalRateLimited)
}

fn refused(r: Refused) -> Option<SendError> {
    Some(local(r.reason()))
}

/// A failure on this side: nothing was posted.
fn local(reason: String) -> SendError {
    SendError {
        reason,
        retry_after_secs: None,
        ambiguous: false,
    }
}

/// A failure before the message `POST`: the message does not exist.
fn before_post(ctx: &Ctx, err: p::Error) -> Option<SendError> {
    failed(&ctx.internal, ctx.gen, err).map(|mut e| {
        e.ambiguous = false;
        // A wait storage asked for is no slowmode.
        e.retry_after_secs = None;
        e
    })
}

/// Whether a part may succeed when it is sent again, and the least wait first.
fn passing(err: &p::Error) -> Option<Duration> {
    match err {
        p::Error::Http(_) | p::Error::Network(_) => Some(Duration::ZERO),
        p::Error::RateLimited { retry_after } => Some((*retry_after).min(MAX_PART_WAIT)),
        p::Error::Api(api) if api.status >= 500 || api.status == 408 => Some(Duration::ZERO),
        _ => None,
    }
}

// Deviation: Fluxer sends up to four parts at once; Inlet sends one at a time, so an
// upload holds one part in memory.
async fn put_part(
    ctx: &Ctx,
    url: &p::UploadUrl,
    part: &Held,
    content_type: &str,
    progress: &Progress,
) -> Result<(), Step> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let report = progress.callback(progress.done());
        let result = ctx
            .client
            .put_upload(url, part.data().clone(), content_type, Some(report))
            .await;
        match result {
            Ok(()) => return Ok(()),
            Err(err) => match passing(&err) {
                Some(wait) if attempt < PART_ATTEMPTS => {
                    let backoff = Duration::from_secs(u64::from(attempt));
                    tokio::time::sleep(wait.max(backoff)).await;
                }
                _ => return Err(Step::Upload(err)),
            },
        }
    }
}

/// The message itself, naming the stored files.
async fn post_uploaded(
    ctx: &Ctx,
    job: &Job,
    uploaded: &[p::UploadedFile],
) -> Result<(), Option<SendError>> {
    let message = job
        .message
        .to_message()
        .map_err(|e| failed(&ctx.internal, ctx.gen, e.into()))?;
    // The gateway's MESSAGE_CREATE brings the message into the list.
    ctx.client
        .send_with_uploads(&job.message.channel_id, &message, uploaded)
        .await
        .map(|_| ())
        .map_err(|err| failed(&ctx.internal, ctx.gen, err))
}

/// Progress of one upload, for the composer's upload list: about every 1 %.
#[derive(Clone)]
struct Progress {
    events: Ui,
    nonce: String,
    total: u64,
    /// Bytes of finished parts.
    done: Arc<AtomicU64>,
    /// The last percentage reported.
    last: Arc<AtomicU64>,
}

impl Progress {
    fn new(events: Ui, nonce: String, total: u64) -> Self {
        Progress {
            events,
            nonce,
            total,
            done: Arc::default(),
            last: Arc::new(AtomicU64::new(u64::MAX)),
        }
    }

    fn done(&self) -> u64 {
        self.done.load(Ordering::SeqCst)
    }

    fn add(&self, bytes: u64) {
        let now = self.done.fetch_add(bytes, Ordering::SeqCst) + bytes;
        self.report(now);
    }

    fn reset(&self) {
        self.done.store(0, Ordering::SeqCst);
        self.report(0);
    }

    fn report(&self, sent: u64) {
        let sent = sent.min(self.total);
        let step = (sent * 100).checked_div(self.total).unwrap_or(100);
        if self.last.swap(step, Ordering::SeqCst) != step {
            let _ = self.events.unbounded_send(Ev::UploadProgress {
                nonce: self.nonce.clone(),
                sent,
                total: self.total,
            });
        }
    }

    /// The protocol's callback for a request that starts at `base` bytes.
    fn callback(&self, base: u64) -> p::ProgressFn {
        let this = self.clone();
        Arc::new(move |sent, _| this.report(base.saturating_add(sent)))
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::mpsc::UnboundedReceiver;
    use serde_json::{json, Value};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::backend::composer::send::tests::wire::fixture_with;
    use crate::backend::composer::Mentions;
    use crate::backend::{messages, Event};

    const CHANNEL: &str = "301";
    const PLAN: &str = "/api/v1/channels/301/attachments";
    const POSTS: &str = "/api/v1/channels/301/messages";

    fn message() -> Outgoing {
        Outgoing {
            channel_id: CHANNEL.into(),
            nonce: "777".into(),
            content: "look".into(),
            mentions: Mentions::default(),
            reply_to: None,
            silent: false,
        }
    }

    fn pasted(name: &str, bytes: Bytes) -> FileToSend {
        FileToSend {
            name: name.into(),
            size: bytes.len() as u64,
            content_type: "image/png".into(),
            spoiler: false,
            description: None,
            source: Source::Memory(bytes),
        }
    }

    fn presigned() -> Value {
        json!({"presigned_attachment_uploads": true})
    }

    fn created(id: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": id, "channel_id": CHANNEL, "author": {"id": "10", "username": "me"},
            "content": "look", "nonce": "777"
        }))
    }

    fn single_plan(server: &MockServer, key: &str, size: u64, ct: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"attachments": [{
            "upload_mode": "singlepart", "id": 0, "upload_filename": key,
            "upload_url": format!("{}/storage/{key}?X-Amz-Signature=s", server.uri()),
            "file_size": size, "content_type": ct
        }]}))
    }

    async fn requests(server: &MockServer, verb: &str, route: &str) -> Vec<Request> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method.as_str() == verb && r.url.path() == route)
            .collect()
    }

    fn events(rx: &mut UnboundedReceiver<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    fn failure(events: &[Event]) -> Option<String> {
        events.iter().find_map(|e| match e {
            Event::Messages(messages::Ev::Pending(p)) => p.failed.clone(),
            _ => None,
        })
    }

    fn sent(events: &[Event]) -> bool {
        events
            .iter()
            .any(|e| matches!(e, Event::Messages(messages::Ev::Sent { nonce }) if nonce == "777"))
    }

    /// Runs an upload to its end the way the backend does.
    async fn run(uploads: &Uploads, ctx: &Ctx, upload: Upload) {
        let job = uploads.take_in(ctx.gen, upload);
        ctx.state.remember_send(ctx.gen, &job.message);
        uploads.clone().run(ctx.clone(), job, None).await;
    }

    /// Disk access that is only recorded: for uploads that must never touch the disk.
    #[derive(Default)]
    struct NoDisk(Mutex<Vec<String>>);

    impl Files for NoDisk {
        fn len(&self, path: &Path) -> std::io::Result<u64> {
            self.0
                .lock()
                .unwrap()
                .push(format!("len {}", path.display()));
            Err(std::io::Error::other("no disk in this test"))
        }

        fn read(&self, path: &Path, _: u64, _: Range<u64>) -> std::io::Result<Bytes> {
            self.0
                .lock()
                .unwrap()
                .push(format!("read {}", path.display()));
            Err(std::io::Error::other("no disk in this test"))
        }
    }

    #[tokio::test]
    async fn file_over_the_limit_is_refused_before_upload() {
        // The composer refuses it when it is attached …
        let limits = Limits::default();
        assert_eq!(
            limits.check([("big.mov", 25 * MIB + 1)]),
            Err(Refused::FileSize {
                name: "big.mov".into(),
                max: 25 * MIB
            })
        );
        assert!(limits.check([("ok.png", 25 * MIB)]).is_ok());
        let eleven: Vec<(&str, u64)> = (0..11).map(|_| ("a.png", 1)).collect();
        assert_eq!(limits.check(eleven), Err(Refused::Count { max: 10 }));

        // … and the backend before any request, with the account's own limit.
        let (server, ctx, mut rx) = fixture_with(presigned()).await;
        let uploads = Uploads::default();
        let big = pasted("big.png", Bytes::from(vec![0u8; (25 * MIB + 1) as usize]));
        run(
            &uploads,
            &ctx,
            Upload {
                message: message(),
                files: vec![big],
                strip_exif: false,
            },
        )
        .await;
        let api: Vec<String> = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.url.path().to_owned())
            .filter(|p| p != "/.well-known/fluxer")
            .collect();
        assert!(api.is_empty(), "nothing was sent: {api:?}");
        let events = events(&mut rx);
        let reason = failure(&events).expect("the row fails with the reason");
        assert!(reason.contains("big.png"), "{reason}");
        // A refusal is certain: a Retry does not need to look for a copy.
        let plan = ctx.retry_plan(CHANNEL.into(), "777".into(), "look".into(), Instant::now());
        assert!(plan.look_since_ms.is_none());
        assert_eq!(uploads.held_bytes(), 25 * MIB + 1, "kept for Retry");
        uploads.forget_gen(ctx.gen);
        assert_eq!(uploads.held_bytes(), 0, "the session's end lets go of it");
        assert!(
            matches!(uploads.retry_of(ctx.gen, "777"), Some(Retry::Gone(_))),
            "a Retry asks for the files again"
        );
    }

    #[tokio::test]
    async fn cancel_stops_the_upload_and_frees_memory() {
        let (server, ctx, mut rx) = fixture_with(presigned()).await;
        Mock::given(method("POST"))
            .and(path(PLAN))
            .respond_with(single_plan(&server, "k1", 3 * MIB, "image/png"))
            .mount(&server)
            .await;
        // Storage takes its time.
        Mock::given(method("PUT"))
            .and(path("/storage/k1"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(POSTS))
            .respond_with(created("900"))
            .mount(&server)
            .await;
        let uploads = Uploads::default();
        let job = uploads.take_in(
            ctx.gen,
            Upload {
                message: message(),
                files: vec![pasted(
                    "photo.png",
                    Bytes::from(vec![7u8; (3 * MIB) as usize]),
                )],
                strip_exif: false,
            },
        );
        assert_eq!(uploads.held_bytes(), 3 * MIB, "the pasted image is counted");
        ctx.state.remember_send(ctx.gen, &job.message);
        uploads.launch(ctx.clone(), job, None, |fut| {
            tokio::spawn(fut).abort_handle()
        });

        // Wait until the bytes are on their way to storage.
        let started = Instant::now();
        while requests(&server, "PUT", "/storage/k1").await.is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the upload started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(uploads.held_bytes() >= 3 * MIB);

        assert!(uploads.cancel("777"), "the upload was running");
        let started = Instant::now();
        while uploads.held_bytes() > 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "memory was freed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Nothing more happens: no message, nothing kept, no outcome reported.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            requests(&server, "POST", POSTS).await.is_empty(),
            "no message"
        );
        assert!(uploads.retry_of(ctx.gen, "777").is_none());
        assert!(!uploads.cancel("777"));
        let events = events(&mut rx);
        assert!(failure(&events).is_none() && !sent(&events));
    }

    #[tokio::test]
    async fn multipart_chunks_are_10_mib() {
        let (server, ctx, mut rx) = fixture_with(presigned()).await;
        let size = 25 * MIB;
        let parts: Vec<Value> = (1..=3)
            .map(|n| json!({"part_number": n, "upload_url": format!("{}/storage/big?partNumber={n}&X-Amz-Signature=s", server.uri())}))
            .collect();
        Mock::given(method("POST"))
            .and(path(PLAN))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"attachments": [{
                    "upload_mode": "multipart", "id": 0, "upload_filename": "big-key",
                    "file_size": size, "content_type": "video/mp4", "upload_id": "up-1",
                    "part_size": 10 * MIB, "parts": parts
                }]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/storage/big"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/channels/301/attachments/complete"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"uploads": [{"upload_filename": "big-key"}]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(POSTS))
            .respond_with(created("900"))
            .expect(1)
            .mount(&server)
            .await;

        // A picked file, read from disk one part at a time.
        let dir = std::env::temp_dir().join(format!("inlet-upload-{}", p::new_nonce()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mp4");
        let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &content).unwrap();
        let uploads = Uploads::default();
        run(
            &uploads,
            &ctx,
            Upload {
                message: message(),
                files: vec![FileToSend {
                    name: "clip.mp4".into(),
                    size,
                    content_type: "video/mp4".into(),
                    spoiler: false,
                    description: None,
                    source: Source::Path(file.clone()),
                }],
                strip_exif: true,
            },
        )
        .await;
        let _ = std::fs::remove_dir_all(&dir);

        let mut puts = requests(&server, "PUT", "/storage/big").await;
        puts.sort_by_key(|r| r.url.query().unwrap_or_default().to_owned());
        let lens: Vec<u64> = puts.iter().map(|r| r.body.len() as u64).collect();
        assert_eq!(
            lens,
            [10 * MIB, 10 * MIB, 5 * MIB],
            "10 MiB parts, the last one shorter"
        );
        for (i, put) in puts.iter().enumerate() {
            let start = i * (10 * MIB) as usize;
            assert!(
                put.body[..] == content[start..start + put.body.len()],
                "part {i}"
            );
            assert!(
                put.headers.get("authorization").is_none(),
                "no token to storage"
            );
        }
        let post = &requests(&server, "POST", POSTS).await[0];
        let body: Value = serde_json::from_slice(&post.body).unwrap();
        assert_eq!(body["nonce"], "777");
        assert_eq!(body["attachments"][0]["upload_filename"], "big-key");
        assert_eq!(body["attachments"][0]["filename"], "clip.mp4");
        assert!(sent(&events(&mut rx)), "the row goes");
        assert_eq!(uploads.held_bytes(), 0, "nothing held after the upload");
    }

    #[tokio::test]
    async fn pasted_image_is_never_written_to_disk() {
        // What the composer makes of a clipboard image: bytes in memory, no path.
        let clipboard = gpui::Image::from_bytes(
            gpui::ImageFormat::Jpeg,
            super::exif::tests::jpeg_with_exif(1),
        );
        let staged = crate::ui::composer::attachments::pasted_file(&clipboard)
            .expect("an image is attached");
        assert!(matches!(staged.source, Source::Memory(_)));
        assert!(staged.name.ends_with(".jpg"), "{}", staged.name);
        let stripped = exif::strip(clipboard.bytes()).unwrap();

        let (server, ctx, mut rx) = fixture_with(presigned()).await;
        // The plan asks for the size after metadata removal.
        Mock::given(method("POST"))
            .and(path(PLAN))
            .respond_with(single_plan(
                &server,
                "k2",
                stripped.len() as u64,
                "image/jpeg",
            ))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/storage/k2"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(POSTS))
            .respond_with(created("901"))
            .mount(&server)
            .await;

        let disk = Arc::new(NoDisk::default());
        let uploads = Uploads::with_files(disk.clone());
        run(
            &uploads,
            &ctx,
            Upload {
                message: message(),
                files: vec![staged],
                strip_exif: true,
            },
        )
        .await;
        assert!(disk.0.lock().unwrap().is_empty(), "the disk was never used");
        let put = &requests(&server, "PUT", "/storage/k2").await[0];
        assert_eq!(
            put.body,
            stripped.to_vec(),
            "from memory, without its metadata"
        );
        assert!(sent(&events(&mut rx)));
        assert_eq!(uploads.held_bytes(), 0);
    }

    #[tokio::test]
    async fn a_failed_upload_retries_with_the_same_nonce_and_never_twice() {
        let (server, ctx, mut rx) = fixture_with(presigned()).await;
        Mock::given(method("POST"))
            .and(path(PLAN))
            .respond_with(single_plan(&server, "k3", 3, "image/png"))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/storage/k3"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        // The message POST fails once with a server error: it may exist.
        Mock::given(method("POST"))
            .and(path(POSTS))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(POSTS))
            .respond_with(created("902"))
            .mount(&server)
            .await;
        let uploads = Uploads::default();
        let upload = Upload {
            message: message(),
            files: vec![pasted("a.png", Bytes::from_static(b"abc"))],
            strip_exif: false,
        };
        run(&uploads, &ctx, upload).await;
        let first = events(&mut rx);
        assert!(failure(&first).is_some(), "the row shows Retry");
        let names = first.iter().find_map(|e| match e {
            Event::Messages(messages::Ev::Pending(p)) => Some(p.files.clone()),
            _ => None,
        });
        assert_eq!(
            names,
            Some(vec!["a.png".to_owned()]),
            "the row keeps the file name"
        );

        // Retry: the whole upload again, with the first nonce, so the server can drop a
        // copy; it is sent once more, not twice.
        let Some(Retry::Job(job)) = uploads.retry_of(ctx.gen, "777") else {
            panic!("the upload was kept for Retry");
        };
        let plan = ctx.retry_plan(CHANNEL.into(), "777".into(), "look".into(), Instant::now());
        assert!(plan.look_since_ms.is_none(), "within the nonce window");
        uploads
            .clone()
            .run(ctx.clone(), job, plan.look_since_ms)
            .await;
        let posts = requests(&server, "POST", POSTS).await;
        assert_eq!(posts.len(), 2);
        for post in &posts {
            let body: Value = serde_json::from_slice(&post.body).unwrap();
            assert_eq!(body["nonce"], "777");
            assert_eq!(body["attachments"][0]["upload_filename"], "k3");
        }
        assert!(sent(&events(&mut rx)));
        assert!(
            uploads.retry_of(ctx.gen, "777").is_none(),
            "forgotten once sent"
        );
        assert_eq!(uploads.held_bytes(), 0);
    }

    #[tokio::test]
    async fn a_late_retry_finds_the_upload_with_its_files_and_posts_nothing() {
        let (server, ctx, mut rx) = fixture_with(presigned()).await;
        let since = super::super::directory::unix_ms();
        // The first attempt did arrive: an own message with the text and one file.
        let copy = json!({
            "id": p::models::snowflake_at_ms(since + 500), "channel_id": CHANNEL,
            "author": {"id": "10", "username": "me"}, "content": "look",
            "attachments": [{"id": "5", "filename": "a.png", "size": 3}]
        });
        let text_only = json!({
            "id": p::models::snowflake_at_ms(since + 600), "channel_id": CHANNEL,
            "author": {"id": "10", "username": "me"}, "content": "look"
        });
        Mock::given(method("GET"))
            .and(path(POSTS))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([text_only, copy])))
            .mount(&server)
            .await;
        let uploads = Uploads::default();
        let job = uploads.take_in(
            ctx.gen,
            Upload {
                message: message(),
                files: vec![pasted("a.png", Bytes::from_static(b"abc"))],
                strip_exif: false,
            },
        );
        uploads.clone().run(ctx.clone(), job, Some(since)).await;
        let api: Vec<String> = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method.as_str() != "GET")
            .map(|r| r.url.path().to_owned())
            .collect();
        assert!(api.is_empty(), "nothing uploaded or posted again: {api:?}");
        assert!(sent(&events(&mut rx)), "the row counts as sent");
    }

    #[tokio::test]
    async fn switching_accounts_stops_the_upload_and_keeps_it_for_retry() {
        let (server, ctx, _rx) = fixture_with(presigned()).await;
        Mock::given(method("POST"))
            .and(path(PLAN))
            .respond_with(single_plan(&server, "k4", 3, "image/png"))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/storage/k4"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(&server)
            .await;
        let uploads = Uploads::default();
        let job = uploads.take_in(
            ctx.gen,
            Upload {
                message: message(),
                files: vec![pasted("a.png", Bytes::from_static(b"abc"))],
                strip_exif: false,
            },
        );
        uploads.launch(ctx.clone(), job, None, |fut| {
            tokio::spawn(fut).abort_handle()
        });
        let started = Instant::now();
        while requests(&server, "PUT", "/storage/k4").await.is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the upload started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The account is switched away from: its upload stops and is reported.
        let stopped = uploads.stop_gen(ctx.gen);
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].0.nonce, "777");
        assert_eq!(stopped[0].1, vec!["a.png".to_owned()]);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            requests(&server, "POST", POSTS).await.is_empty(),
            "nothing posted"
        );
        // Back on that account Retry has the files; another account never sends them.
        assert!(matches!(
            uploads.retry_of(ctx.gen + 1, "777"),
            Some(Retry::Gone(_))
        ));
        assert!(matches!(
            uploads.retry_of(ctx.gen, "777"),
            Some(Retry::Job(_))
        ));
        assert_eq!(uploads.held_bytes(), 3, "kept for Retry");
        uploads.forget_gen(ctx.gen);
        assert_eq!(uploads.held_bytes(), 0);
    }
}
