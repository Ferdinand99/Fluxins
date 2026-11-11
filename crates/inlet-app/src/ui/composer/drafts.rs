// SPDX-License-Identifier: AGPL-3.0-or-later
//! Drafts: the unsent text of each channel, kept when the person switches channels and
//! across restarts. They are the only message text Inlet writes to disk: one file per
//! account in the config folder (only the account can read it on Unix), deleted when the
//! account signs out. A draft is the wire text plus the labels of its pills, so it comes
//! back with the same pills.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use inlet_editor::{Buffer, Id, Resolve, Token};
use serde::{Deserialize, Serialize};

/// Drafts kept per account; the least recently changed go first.
const MAX_DRAFTS: usize = 50;
/// The largest draft file read back.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// The longest draft kept, in bytes of wire text (a 4000-unit message fits).
const MAX_DRAFT_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LabelKind {
    User,
    Role,
    Channel,
}

/// The name a pill showed, without its `@` or `#`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub kind: LabelKind,
    pub id: u64,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    /// What [`Buffer::to_wire`] gave.
    pub wire: String,
    pub labels: Vec<Label>,
    /// Bumped on every change, for dropping the oldest.
    #[serde(default)]
    pub seq: u64,
    /// A message with files that came back as this draft at an account switch. It may
    /// have reached the server after all; then the draft goes (see `SentCheck`).
    #[serde(default)]
    pub sent_check: Option<SentCheck>,
}

/// How to tell that a message that came back as a draft was sent after all: its nonce
/// (in a message the gateway delivered), or an own message with its text and number of
/// files, created after it was queued (lists carry no nonce).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentCheck {
    pub nonce: String,
    /// The wire text that was sent.
    pub content: String,
    pub files: usize,
    /// When it was queued, in Unix milliseconds.
    pub queued_ms: u64,
    /// The draft's first bytes that are this message; the rest was a draft already.
    pub len: usize,
}

impl Draft {
    /// The draft of an editor's buffer, or `None` when it holds nothing.
    pub fn of(buffer: &Buffer) -> Option<Draft> {
        if buffer.text().trim().is_empty() {
            return None;
        }
        let labels = buffer
            .tokens()
            .iter()
            .filter_map(|span| {
                let (kind, id) = match span.token {
                    Token::User(id) => (LabelKind::User, id),
                    Token::Role(id) => (LabelKind::Role, id),
                    Token::Channel(id) => (LabelKind::Channel, id),
                    _ => return None,
                };
                let shown = buffer.text().get(span.range.clone())?;
                // Drop the sigil the pill shows.
                let name = shown.get(1..).unwrap_or_default().to_owned();
                Some(Label {
                    kind,
                    id: id.0,
                    name,
                })
            })
            .collect();
        Some(Draft {
            wire: buffer.to_wire(),
            labels,
            seq: 0,
            sent_check: None,
        })
    }

    fn name(&self, kind: LabelKind, id: Id) -> Option<String> {
        self.labels
            .iter()
            .find(|l| l.kind == kind && l.id == id.0)
            .map(|l| l.name.clone())
    }
}

/// The labels of a draft, for [`inlet_editor::Editor::set_wire`].
impl Resolve for Draft {
    fn user(&self, id: Id) -> Option<String> {
        self.name(LabelKind::User, id)
    }
    fn role(&self, id: Id) -> Option<String> {
        self.name(LabelKind::Role, id)
    }
    fn channel(&self, id: Id) -> Option<String> {
        self.name(LabelKind::Channel, id)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    drafts: BTreeMap<String, Draft>,
}

/// The drafts of the signed-in account.
#[derive(Default)]
pub struct Drafts {
    file: Option<PathBuf>,
    drafts: BTreeMap<String, Draft>,
    seq: u64,
    /// Bumped on sign-out. A write prepared before it is not done.
    generation: Arc<AtomicU64>,
    /// Held while a file is written or deleted, so a late write cannot bring a deleted
    /// file back.
    disk: Arc<Mutex<()>>,
    /// Files of accounts that signed out while another account was active; a write
    /// still on its way to one of them is dropped.
    deleted: Arc<Mutex<HashSet<PathBuf>>>,
    dirty: bool,
}

/// A prepared write: run it off the UI thread.
pub struct PendingWrite {
    path: PathBuf,
    bytes: Vec<u8>,
    generation: u64,
    current: Arc<AtomicU64>,
    disk: Arc<Mutex<()>>,
    deleted: Arc<Mutex<HashSet<PathBuf>>>,
}

impl PendingWrite {
    pub fn run(self) {
        let _guard = self.disk.lock().unwrap_or_else(|e| e.into_inner());
        if self.current.load(Ordering::SeqCst) != self.generation {
            return;
        }
        if self
            .deleted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&self.path)
        {
            return;
        }
        if let Err(err) = write_private(&self.path, &self.bytes) {
            log::warn!("drafts were not saved: {err}");
        }
    }
}

impl Drafts {
    /// Ties the drafts to the account's file and reads what it holds. Drafts written in
    /// this session win over the file.
    pub fn attach(&mut self, path: PathBuf) {
        if self.file.as_deref() == Some(path.as_path()) {
            return;
        }
        self.deleted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&path);
        let stored = read_file(&path);
        for (channel, draft) in stored.drafts {
            self.seq = self.seq.max(draft.seq);
            self.drafts.entry(channel).or_insert(draft);
        }
        self.file = Some(path);
        self.trim();
    }

    pub fn get(&self, channel: &str) -> Option<&Draft> {
        self.drafts.get(channel)
    }

    /// Keeps `draft` for `channel`; `None` removes it.
    pub fn put(&mut self, channel: &str, draft: Option<Draft>) {
        match draft.filter(|d| d.wire.len() <= MAX_DRAFT_BYTES) {
            Some(mut draft) => {
                if self.drafts.get(channel).map(|d| (&d.wire, &d.labels))
                    == Some((&draft.wire, &draft.labels))
                {
                    return;
                }
                self.seq += 1;
                draft.seq = self.seq;
                self.drafts.insert(channel.to_owned(), draft);
                self.trim();
            }
            None => {
                if self.drafts.remove(channel).is_none() {
                    return;
                }
            }
        }
        self.dirty = true;
    }

    /// The write to do, if anything changed since the last one.
    pub fn prepare_write(&mut self) -> Option<PendingWrite> {
        if !self.dirty {
            return None;
        }
        let path = self.file.clone()?;
        self.dirty = false;
        let bytes = serde_json::to_vec(&FileFormat {
            version: 1,
            drafts: self.drafts.clone(),
        })
        .ok()?;
        Some(PendingWrite {
            path,
            bytes,
            generation: self.generation.load(Ordering::SeqCst),
            current: self.generation.clone(),
            disk: self.disk.clone(),
            deleted: self.deleted.clone(),
        })
    }

    /// Another account becomes active: every draft so far goes to this account's file
    /// (the returned write; run it off the UI thread), then the drafts are forgotten here
    /// but not deleted, so the next account's file is read on its own. Writes prepared
    /// before are dropped; the returned one holds all of them.
    pub fn switch_account(&mut self) -> Option<PendingWrite> {
        self.dirty = true;
        let write = self.prepare_write();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.file = None;
        self.drafts.clear();
        self.dirty = false;
        write.map(|mut w| {
            w.generation = generation;
            w
        })
    }

    /// An account that is not the active one signed out: its drafts file goes (the
    /// returned job; run it off the UI thread), and no write still on its way can bring
    /// it back.
    pub fn forget_file(&self, path: PathBuf) -> impl FnOnce() + Send + 'static {
        self.deleted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.clone());
        let disk = self.disk.clone();
        move || {
            let _guard = disk.lock().unwrap_or_else(|e| e.into_inner());
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => log::warn!("the drafts file could not be deleted: {err}"),
            }
        }
    }

    /// Sign-out: forgets every draft and deletes the file.
    pub fn sign_out(&mut self) {
        let _guard = self.disk.lock().unwrap_or_else(|e| e.into_inner());
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(path) = self.file.take() {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => log::warn!("the drafts file could not be deleted: {err}"),
            }
        }
        self.drafts.clear();
        self.dirty = false;
    }

    /// Bumped on every sign-out.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn trim(&mut self) {
        while self.drafts.len() > MAX_DRAFTS {
            let oldest = self
                .drafts
                .iter()
                .min_by_key(|(_, d)| d.seq)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.drafts.remove(&k);
                    self.dirty = true;
                }
                None => break,
            }
        }
    }
}

/// The drafts file of an account: `<config>/inlet/drafts/<instance host>-<user id>.json`.
pub fn path_for(config_dir: &Path, instance: &str, user_id: &str) -> Option<PathBuf> {
    if user_id.is_empty() || !user_id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let host: String = instance
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' => c.to_ascii_lowercase(),
            _ => '_',
        })
        .take(100)
        .collect();
    Some(
        config_dir
            .join("inlet")
            .join("drafts")
            .join(format!("{host}-{user_id}.json")),
    )
}

fn read_file(path: &Path) -> FileFormat {
    let Ok(meta) = std::fs::metadata(path) else {
        return FileFormat::default();
    };
    if meta.len() > MAX_FILE_BYTES {
        log::warn!("the drafts file is too large and was not read");
        return FileFormat::default();
    }
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<FileFormat>(&bytes).ok())
        .map(|mut f| {
            f.drafts.retain(|_, d| d.wire.len() <= MAX_DRAFT_BYTES);
            f
        })
        .unwrap_or_default()
}

/// Writes through a temporary file and a rename, readable only by the account on Unix.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlet_editor::NoNames;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "inlet-drafts-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn draft(wire: &str) -> Option<Draft> {
        let mut buf = Buffer::from_wire(wire, &NoNames).with_max_len(2000);
        // A pill with a real label, as autocomplete makes it.
        let end = buf.len();
        buf.insert_token(end, Token::User(Id(42)), "@ada");
        Draft::of(&buf)
    }

    #[test]
    fn a_draft_keeps_its_pills_and_their_names() {
        let d = draft("hi ").unwrap();
        assert_eq!(d.wire, "hi <@42>");
        assert_eq!(d.user(Id(42)).as_deref(), Some("ada"));
        let back = Buffer::from_wire(&d.wire, &d);
        assert_eq!(back.text(), "hi @ada");
        assert_eq!(back.tokens().len(), 1);
        assert!(Draft::of(&Buffer::new(2000)).is_none(), "nothing to keep");
    }

    #[test]
    fn drafts_are_written_and_read_back_per_account() {
        let dir = temp_dir("roundtrip");
        let path = path_for(&dir, "https://Fluxer.Example", "123").unwrap();
        assert!(path.ends_with("inlet/drafts/fluxer.example-123.json"));
        let mut drafts = Drafts::default();
        drafts.attach(path.clone());
        drafts.put("1", draft("one "));
        drafts.prepare_write().unwrap().run();
        assert!(drafts.prepare_write().is_none(), "nothing new to write");

        let mut again = Drafts::default();
        again.attach(path.clone());
        assert_eq!(again.get("1").map(|d| d.wire.as_str()), Some("one <@42>"));
        assert_eq!(
            path_for(&dir, "x", "../1"),
            None,
            "only numeric account IDs"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_write_prepared_before_sign_out_does_not_bring_the_file_back() {
        let dir = temp_dir("late-write");
        let path = path_for(&dir, "fluxer.example", "9").unwrap();
        let mut drafts = Drafts::default();
        drafts.attach(path.clone());
        drafts.put("1", draft("secret "));
        let late = drafts.prepare_write().unwrap();
        drafts.sign_out();
        late.run();
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn switching_accounts_keeps_each_accounts_drafts_apart() {
        let dir = temp_dir("switch");
        let a = path_for(&dir, "fluxer.example", "1").unwrap();
        let b = path_for(&dir, "fluxer.example", "2").unwrap();
        let mut drafts = Drafts::default();
        drafts.attach(a.clone());
        drafts.put("10", draft("from a "));
        // A write of account A still on its way is replaced by the switch's own write.
        let late = drafts.prepare_write().unwrap();
        drafts
            .switch_account()
            .expect("a's drafts are written")
            .run();
        late.run();
        assert!(drafts.get("10").is_none(), "nothing of a stays in memory");
        drafts.attach(b.clone());
        assert!(drafts.get("10").is_none(), "b does not see a's draft");
        drafts.put("20", draft("from b "));
        drafts.prepare_write().unwrap().run();
        // Back to a: its draft is there, b's is not.
        drafts.switch_account().unwrap().run();
        drafts.attach(a.clone());
        assert_eq!(
            drafts.get("10").map(|d| d.wire.as_str()),
            Some("from a <@42>")
        );
        assert!(drafts.get("20").is_none());
        // B signs out while a is active: its file goes, and the write the switch away
        // from b left on its way cannot bring it back.
        drafts.switch_account().unwrap().run();
        drafts.attach(b.clone());
        drafts.put("22", draft("late "));
        let late_b = drafts.switch_account().expect("b's write");
        drafts.forget_file(b.clone())();
        late_b.run();
        assert!(!b.exists(), "a signed-out account's drafts stay gone");
        assert!(a.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_the_newest_drafts_are_kept() {
        let mut drafts = Drafts::default();
        for i in 0..(MAX_DRAFTS + 5) {
            drafts.put(&i.to_string(), draft("x "));
        }
        assert_eq!(drafts.drafts.len(), MAX_DRAFTS);
        assert!(drafts.get("0").is_none());
        assert!(drafts.get(&(MAX_DRAFTS + 4).to_string()).is_some());
    }
}
