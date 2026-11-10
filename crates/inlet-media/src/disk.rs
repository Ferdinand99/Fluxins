//! Optional on-disk cache of the *encoded* images, in the OS cache directory.
//!
//! - File names are the SHA-256 of the URL in hex plus `.bin`, never anything
//!   from the network. Fluxer's media URLs contain the content hash already, so
//!   this is content-addressed for practical purposes.
//! - Files are plain data: created with mode 0600 (no executable bit) on Unix,
//!   written to a temporary name and renamed, so a crash leaves no half file.
//! - Total size is capped. The least recently used files go first.
//! - Entries are re-checked and re-decoded with all limits on every read, so a
//!   file that was changed on disk is no more dangerous than a download. The
//!   client deletes an entry that does not decode and fetches it again.
//! - The order for eviction is kept in memory. After a restart it is the order of
//!   the file times, which are the times of writing.
//! - Only files with our own naming scheme are ever read, counted or deleted.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

/// Where and how much to cache on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskCacheConfig {
    /// The directory. It is created if missing.
    pub dir: PathBuf,
    /// Size cap of all files together. Default 256 MiB.
    pub max_bytes: u64,
}

impl DiskCacheConfig {
    /// Default size cap: 256 MiB.
    pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

    /// `<OS cache dir>/inlet/media`, or `None` when the OS has no cache directory.
    pub fn in_os_cache_dir() -> Option<Self> {
        Some(Self {
            dir: dirs::cache_dir()?.join("inlet").join("media"),
            max_bytes: Self::DEFAULT_MAX_BYTES,
        })
    }
}

struct Meta {
    size: u64,
    /// Position in the use order. Higher is more recent.
    tick: u64,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Meta>,
    used: u64,
    tick: u64,
}

impl State {
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }
}

pub(crate) struct DiskCache {
    dir: PathBuf,
    cap: u64,
    state: Mutex<State>,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The cache key of a URL: 64 lowercase hex characters.
pub(crate) fn key_for(url: &str) -> String {
    hex(&Sha256::digest(url.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn is_key(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn file_name(key: &str) -> String {
    format!("{key}.bin")
}

impl DiskCache {
    /// Opens the directory and counts what is already there.
    pub fn open(config: &DiskCacheConfig) -> io::Result<Self> {
        create_private_dir(&config.dir)?;
        let mut state = State::default();
        let mut found: Vec<(SystemTime, String, u64)> = Vec::new();
        for item in fs::read_dir(&config.dir)? {
            let Ok(item) = item else { continue };
            let name = item.file_name();
            let Some(name) = name.to_str() else { continue };
            // Leftovers of an interrupted write.
            if name.ends_with(".tmp") && name.get(..64).is_some_and(is_key) {
                let _ = fs::remove_file(item.path());
                continue;
            }
            let Some(key) = name.strip_suffix(".bin").filter(|k| is_key(k)) else {
                continue;
            };
            let Ok(meta) = fs::symlink_metadata(item.path()) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            found.push((
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                key.to_owned(),
                meta.len(),
            ));
        }
        // The file times of an earlier run give the order to start from.
        found.sort();
        for (_, key, size) in found {
            let tick = state.next_tick();
            state.used += size;
            state.entries.insert(key, Meta { size, tick });
        }
        let cache = Self {
            dir: config.dir.clone(),
            cap: config.max_bytes,
            state: Mutex::new(state),
        };
        cache.evict(&mut cache.lock());
        Ok(cache)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(file_name(key))
    }

    /// Reads an entry, at most `max_len` bytes. An entry that is larger than the
    /// caller accepts is left alone (another kind of image may accept it). A file
    /// that is gone or not a plain file is forgotten. The lock is not held while
    /// the file is read.
    pub fn get(&self, key: &str, max_len: usize) -> Option<Vec<u8>> {
        if !is_key(key) {
            return None;
        }
        let size = self.lock().entries.get(key)?.size;
        if size > max_len as u64 {
            return None;
        }
        let path = self.path(key);
        let read = (|| -> io::Result<Option<Vec<u8>>> {
            if !fs::symlink_metadata(&path)?.is_file() {
                return Ok(None);
            }
            let mut data = Vec::with_capacity(size as usize);
            File::open(&path)?
                .take(max_len as u64 + 1)
                .read_to_end(&mut data)?;
            Ok((data.len() <= max_len).then_some(data))
        })();
        match read {
            Ok(Some(data)) => {
                let mut state = self.lock();
                let tick = state.next_tick();
                if let Some(meta) = state.entries.get_mut(key) {
                    meta.tick = tick;
                }
                Some(data)
            }
            _ => {
                self.remove(key);
                None
            }
        }
    }

    /// Forgets an entry and deletes its file.
    pub fn remove(&self, key: &str) {
        if !is_key(key) {
            return;
        }
        let mut state = self.lock();
        if let Some(meta) = state.entries.remove(key) {
            state.used -= meta.size;
        }
        let _ = fs::remove_file(self.path(key));
    }

    /// Stores an entry and evicts old ones. Data larger than the whole cap is
    /// skipped. The file is written and synced under a temporary name without the
    /// lock, then renamed into place.
    pub fn put(&self, key: &str, data: &[u8]) -> io::Result<()> {
        if !is_key(key) || data.len() as u64 > self.cap {
            return Ok(());
        }
        let tmp = self.dir.join(format!(
            "{key}.{}.{}.tmp",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let write = (|| -> io::Result<()> {
            let mut file = create_private_file(&tmp)?;
            file.write_all(data)?;
            file.sync_all()?;
            Ok(())
        })();
        let mut state = self.lock();
        if let Err(err) = write.and_then(|()| fs::rename(&tmp, self.path(key))) {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        let size = data.len() as u64;
        let tick = state.next_tick();
        if let Some(old) = state.entries.insert(key.to_owned(), Meta { size, tick }) {
            state.used -= old.size;
        }
        state.used += size;
        self.evict(&mut state);
        Ok(())
    }

    /// Deletes least recently used files until the total is within the cap.
    fn evict(&self, state: &mut State) {
        if state.used <= self.cap {
            return;
        }
        let mut by_age: Vec<(u64, String)> = state
            .entries
            .iter()
            .map(|(k, m)| (m.tick, k.clone()))
            .collect();
        by_age.sort();
        for (_, key) in by_age {
            if state.used <= self.cap {
                break;
            }
            if let Some(meta) = state.entries.remove(&key) {
                state.used -= meta.size;
            }
            let _ = fs::remove_file(self.path(&key));
        }
    }

    #[cfg(test)]
    pub fn used(&self) -> u64 {
        self.lock().used
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // A directory that was there before may be more open. Close it if it is ours.
        if let Ok(meta) = fs::metadata(dir) {
            if meta.permissions().mode() & 0o077 != 0 {
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(dir: &Path, cap: u64) -> DiskCache {
        DiskCache::open(&DiskCacheConfig {
            dir: dir.to_owned(),
            max_bytes: cap,
        })
        .unwrap()
    }

    #[test]
    fn keys_are_hashes_and_never_contain_url_text() {
        let key = key_for("https://cdn.example/../../etc/passwd?x=1");
        assert!(is_key(&key));
        assert_eq!(key.len(), 64);
        assert!(!key.contains(['/', '\\', '.', ':']));
        assert!(!is_key("../evil"));
    }

    #[test]
    fn stores_and_returns_data() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path(), 1000);
        let key = key_for("https://a.example/1.png");
        assert!(cache.get(&key, 100).is_none());
        cache.put(&key, b"hello").unwrap();
        assert_eq!(cache.get(&key, 100).unwrap(), b"hello");
        assert_eq!(cache.used(), 5);
        // A caller with a lower limit gets nothing, and the file stays for the others.
        assert!(cache.get(&key, 2).is_none());
        assert_eq!(cache.len(), 1);
        assert!(dir.path().join(file_name(&key)).exists());
        assert_eq!(cache.get(&key, 100).unwrap(), b"hello");
        // Removing deletes the file.
        cache.remove(&key);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.used(), 0);
        assert!(!dir.path().join(file_name(&key)).exists());
    }

    #[test]
    fn evicts_the_least_recently_used_file_when_over_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path(), 250);
        let keys: Vec<String> = (0..3).map(|i| key_for(&format!("u{i}"))).collect();
        cache.put(&keys[0], &[0; 100]).unwrap();
        cache.put(&keys[1], &[1; 100]).unwrap();
        // Reading the first makes the second the oldest.
        assert!(cache.get(&keys[0], 1000).is_some());
        cache.put(&keys[2], &[2; 100]).unwrap();
        assert!(cache.used() <= 250);
        assert!(cache.get(&keys[0], 1000).is_some());
        assert!(cache.get(&keys[1], 1000).is_none());
        assert!(cache.get(&keys[2], 1000).is_some());
        assert!(!dir.path().join(file_name(&keys[1])).exists());
    }

    #[test]
    fn data_larger_than_the_whole_cache_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path(), 10);
        let key = key_for("big");
        cache.put(&key, &[0; 11]).unwrap();
        assert_eq!(cache.len(), 0);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn reopening_counts_existing_files_and_enforces_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        {
            let cache = open(dir.path(), 1000);
            for i in 0..4 {
                cache.put(&key_for(&format!("u{i}")), &[0; 100]).unwrap();
            }
        }
        let cache = open(dir.path(), 250);
        assert_eq!(cache.len(), 2);
        assert!(cache.used() <= 250);
    }

    #[test]
    fn foreign_files_are_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), b"keep me").unwrap();
        let cache = open(dir.path(), 10);
        cache.put(&key_for("a"), &[0; 10]).unwrap();
        cache.put(&key_for("b"), &[0; 10]).unwrap();
        assert!(dir.path().join("notes.txt").exists());
        assert_eq!(cache.dir(), dir.path());
    }

    #[test]
    fn leftover_temporary_files_are_removed_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join(format!("{}.1.2.tmp", key_for("a")));
        fs::write(&tmp, b"half").unwrap();
        let _cache = open(dir.path(), 100);
        assert!(!tmp.exists());
    }

    #[cfg(unix)]
    #[test]
    fn files_have_no_executable_bit_and_only_the_owner_can_use_them() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cache = open(&dir.path().join("media"), 100);
        let key = key_for("a");
        cache.put(&key, b"data").unwrap();
        let mode = fs::metadata(cache.dir().join(file_name(&key)))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o177, 0, "mode is {mode:o}");
        let dir_mode = fs::metadata(cache.dir()).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o077, 0, "dir mode is {dir_mode:o}");
    }
}
