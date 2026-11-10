//! The client the UI talks to: memory cache, disk cache, deduplication,
//! cancellation, and the capped pools for fetching and decoding.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::runtime::Handle;
use tokio::sync::{watch, Semaphore};
use tokio::task::AbortHandle;
use url::Url;

use crate::decode::{decode, DecodeControl};
use crate::disk::{key_for, DiskCache, DiskCacheConfig};
use crate::memory::ByteLru;
use crate::{DecodedImage, Fetcher, Fit, Limits, MediaError, MediaKind};

/// Settings of a [`MediaClient`]. `Default` gives the values the app uses.
#[derive(Debug, Clone)]
pub struct MediaConfig {
    /// Byte budget of decoded images in memory. Default 128 MiB.
    pub memory_cache_bytes: u64,
    /// Encoded images on disk. `None` turns the disk cache off (the default;
    /// use [`DiskCacheConfig::in_os_cache_dir`] to switch it on).
    pub disk_cache: Option<DiskCacheConfig>,
    /// Hard limits per kind of image.
    pub limits: Limits,
    /// Decodes that may run at the same time. Default: half the cores, 1 to 4.
    pub decode_concurrency: usize,
    /// Fetches that may run at the same time. Default 8. The app's shared rate
    /// limiter still decides when each request leaves.
    pub fetch_concurrency: usize,
    /// Longest time one fetch may take. Default 30 s.
    pub fetch_timeout: Duration,
    /// Longest time one decode may take. Default 10 s.
    pub decode_budget: Duration,
    /// How long a permanent failure (too large, corrupt, ...) is remembered so
    /// that a message list does not retry it on every repaint. Default 5 minutes.
    pub failure_memory: Duration,
}

impl Default for MediaConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        Self {
            memory_cache_bytes: 128 * 1024 * 1024,
            disk_cache: None,
            limits: Limits::default(),
            decode_concurrency: (cores / 2).clamp(1, 4),
            fetch_concurrency: 8,
            fetch_timeout: Duration::from_secs(30),
            decode_budget: Duration::from_secs(10),
            failure_memory: Duration::from_secs(300),
        }
    }
}

/// What the UI wants: one image at one size.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MediaRequest {
    /// Where to fetch it. The app builds it (CDN helpers in the protocol crate).
    pub url: Url,
    /// Decides the limits.
    pub kind: MediaKind,
    /// The size to downscale to.
    pub fit: Fit,
}

impl MediaRequest {
    /// A request.
    pub fn new(url: Url, kind: MediaKind, fit: Fit) -> Self {
        Self { url, kind, fit }
    }
}

/// Counters, for the debug view and for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaStats {
    /// Images in the memory cache.
    pub memory_entries: usize,
    /// Bytes in the memory cache.
    pub memory_bytes: u64,
    /// Requests in progress.
    pub in_flight: usize,
    /// Calls to the fetcher so far.
    pub fetches: u64,
    /// Decodes so far.
    pub decodes: u64,
    /// Requests answered from memory.
    pub memory_hits: u64,
    /// Requests whose bytes came from the disk cache.
    pub disk_hits: u64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    url: String,
    kind: MediaKind,
    fit: Fit,
}

impl Key {
    fn new(req: &MediaRequest) -> Self {
        Self {
            url: req.url.as_str().to_owned(),
            kind: req.kind,
            fit: req.fit.sanitized(),
        }
    }
}

type Outcome = Result<Arc<DecodedImage>, MediaError>;

struct Flight {
    id: u64,
    tx: watch::Sender<Option<Outcome>>,
    waiters: usize,
    cancel: Arc<AtomicBool>,
    abort: Option<AbortHandle>,
}

struct Inner {
    handle: Handle,
    fetcher: Arc<dyn Fetcher>,
    config: MediaConfig,
    disk: Option<Arc<DiskCache>>,
    memory: Mutex<ByteLru<Key, Arc<DecodedImage>>>,
    flights: Mutex<HashMap<Key, Flight>>,
    failures: Mutex<HashMap<Key, (Instant, MediaError)>>,
    fetch_slots: Arc<Semaphore>,
    decode_slots: Arc<Semaphore>,
    next_id: AtomicU64,
    fetches: AtomicU64,
    decodes: AtomicU64,
    memory_hits: AtomicU64,
    disk_hits: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Loads images for the UI. Cheap to clone; all clones share the caches.
///
/// ```no_run
/// # use std::sync::Arc;
/// # use inlet_media::*;
/// # async fn demo(fetcher: Arc<dyn Fetcher>, url: url::Url) {
/// let media = MediaClient::new(tokio::runtime::Handle::current(), fetcher, MediaConfig::default());
/// let req = MediaRequest::new(url, MediaKind::Avatar, Fit::square(88));
/// match media.load(req).await {
///     Ok(image) => { /* RenderImage::new(image.to_bgra_frames()) */ }
///     Err(err) => { /* draw the placeholder, err says why */ }
/// }
/// # }
/// ```
#[derive(Clone)]
pub struct MediaClient {
    inner: Arc<Inner>,
}

impl MediaClient {
    /// Creates a client. Work runs on `handle`'s runtime; decoding runs on its
    /// blocking pool, at most `decode_concurrency` at a time.
    pub fn new(handle: Handle, fetcher: Arc<dyn Fetcher>, config: MediaConfig) -> Self {
        let disk = config
            .disk_cache
            .as_ref()
            .and_then(|cfg| match DiskCache::open(cfg) {
                Ok(cache) => Some(Arc::new(cache)),
                Err(err) => {
                    tracing::warn!("image disk cache is off: {err}");
                    None
                }
            });
        Self {
            inner: Arc::new(Inner {
                handle,
                fetcher,
                disk,
                memory: Mutex::new(ByteLru::new(config.memory_cache_bytes)),
                flights: Mutex::new(HashMap::new()),
                failures: Mutex::new(HashMap::new()),
                fetch_slots: Arc::new(Semaphore::new(config.fetch_concurrency.max(1))),
                decode_slots: Arc::new(Semaphore::new(config.decode_concurrency.max(1))),
                next_id: AtomicU64::new(1),
                fetches: AtomicU64::new(0),
                decodes: AtomicU64::new(0),
                memory_hits: AtomicU64::new(0),
                disk_hits: AtomicU64::new(0),
                config,
            }),
        }
    }

    /// The image if it is already in memory. Never waits. Use it while painting,
    /// and call [`load`](Self::load) when it returns `None`.
    pub fn cached(&self, request: &MediaRequest) -> Option<Arc<DecodedImage>> {
        self.inner.memory_get(&Key::new(request))
    }

    /// Loads an image: from memory, else disk, else the network. Requests for the
    /// same URL, kind and size share one fetch and one decode.
    ///
    /// **Cancelling:** drop the returned future when the UI no longer needs the
    /// image (the row scrolled away, the view closed). When the last waiter of a
    /// request is dropped, its fetch is aborted and its decode stops at the next
    /// check. Waiters do not affect each other: one dropped future leaves the
    /// others running.
    pub async fn load(&self, request: MediaRequest) -> Result<Arc<DecodedImage>, MediaError> {
        validate_url(&request.url)?;
        let key = Key::new(&request);
        if let Some(image) = self.inner.memory_get(&key) {
            return Ok(image);
        }
        if let Some(err) = self.inner.known_failure(&key) {
            return Err(err);
        }
        let (mut rx, _guard) = self.inner.join(&key, &request);
        let outcome = match rx.wait_for(|v| v.is_some()).await {
            Ok(value) => (*value).clone(),
            Err(_) => None,
        };
        outcome.unwrap_or(Err(MediaError::Cancelled))
    }

    /// Drops every decoded image from memory. The disk cache is kept.
    pub fn clear_memory(&self) {
        let cap = self.inner.config.memory_cache_bytes;
        *lock(&self.inner.memory) = ByteLru::new(cap);
    }

    /// Counters.
    pub fn stats(&self) -> MediaStats {
        let (memory_entries, memory_bytes) = {
            let memory = lock(&self.inner.memory);
            (memory.len(), memory.used())
        };
        MediaStats {
            memory_entries,
            memory_bytes,
            in_flight: lock(&self.inner.flights).len(),
            fetches: self.inner.fetches.load(Ordering::Relaxed),
            decodes: self.inner.decodes.load(Ordering::Relaxed),
            memory_hits: self.inner.memory_hits.load(Ordering::Relaxed),
            disk_hits: self.inner.disk_hits.load(Ordering::Relaxed),
        }
    }
}

fn validate_url(url: &Url) -> Result<(), MediaError> {
    let plain = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none();
    if plain {
        Ok(())
    } else {
        Err(MediaError::InvalidUrl(
            "only http(s) URLs without credentials are allowed".into(),
        ))
    }
}

/// Removes this waiter from its flight when dropped, and stops the flight when
/// it was the last one.
struct WaiterGuard {
    inner: Arc<Inner>,
    key: Key,
    id: u64,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let mut flights = lock(&self.inner.flights);
        let Some(flight) = flights.get_mut(&self.key) else {
            return;
        };
        if flight.id != self.id {
            return;
        }
        flight.waiters -= 1;
        if flight.waiters == 0 {
            if let Some(flight) = flights.remove(&self.key) {
                flight.cancel.store(true, Ordering::Relaxed);
                if let Some(abort) = flight.abort {
                    abort.abort();
                }
            }
        }
    }
}

impl Inner {
    fn memory_get(&self, key: &Key) -> Option<Arc<DecodedImage>> {
        let hit = lock(&self.memory).get(key);
        if hit.is_some() {
            self.memory_hits.fetch_add(1, Ordering::Relaxed);
        }
        hit
    }

    fn known_failure(&self, key: &Key) -> Option<MediaError> {
        let mut failures = lock(&self.failures);
        match failures.get(key) {
            Some((at, err)) if at.elapsed() < self.config.failure_memory => Some(err.clone()),
            Some(_) => {
                failures.remove(key);
                None
            }
            None => None,
        }
    }

    /// Joins the running request for `key`, or starts one.
    fn join(
        self: &Arc<Self>,
        key: &Key,
        request: &MediaRequest,
    ) -> (watch::Receiver<Option<Outcome>>, WaiterGuard) {
        let mut flights = lock(&self.flights);
        if let Some(flight) = flights.get_mut(key) {
            flight.waiters += 1;
            let guard = WaiterGuard {
                inner: self.clone(),
                key: key.clone(),
                id: flight.id,
            };
            return (flight.tx.subscribe(), guard);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = watch::channel(None);
        let cancel = Arc::new(AtomicBool::new(false));
        let task = self.handle.spawn(run_flight(
            self.clone(),
            key.clone(),
            request.clone(),
            id,
            cancel.clone(),
        ));
        flights.insert(
            key.clone(),
            Flight {
                id,
                tx,
                waiters: 1,
                cancel,
                abort: Some(task.abort_handle()),
            },
        );
        let guard = WaiterGuard {
            inner: self.clone(),
            key: key.clone(),
            id,
        };
        (rx, guard)
    }

    fn finish(&self, key: &Key, id: u64, outcome: Outcome) {
        match &outcome {
            Ok(image) => {
                lock(&self.memory).insert(key.clone(), image.clone(), image.byte_size());
            }
            Err(err) if err.is_permanent() => {
                let mut failures = lock(&self.failures);
                if failures.len() >= 4096 {
                    failures.clear();
                }
                failures.insert(key.clone(), (Instant::now(), err.clone()));
            }
            Err(_) => {}
        }
        let mut flights = lock(&self.flights);
        if flights.get(key).is_some_and(|f| f.id == id) {
            if let Some(flight) = flights.remove(key) {
                flight.tx.send_replace(Some(outcome));
            }
        }
    }

    async fn produce(
        &self,
        key: &Key,
        request: &MediaRequest,
        cancel: &Arc<AtomicBool>,
    ) -> Outcome {
        // A request that finished just before this one started has filled the cache.
        if let Some(image) = self.memory_get(key) {
            return Ok(image);
        }
        let limits = self.config.limits.get(request.kind).clone();
        let disk_key = key_for(&key.url);

        let mut from_disk = false;
        let mut encoded: Option<Bytes> = None;
        if let Some(disk) = &self.disk {
            let disk = disk.clone();
            let name = disk_key.clone();
            let max = limits.max_encoded_bytes;
            let found = self
                .handle
                .spawn_blocking(move || disk.get(&name, max))
                .await
                .ok()
                .flatten();
            if let Some(data) = found {
                self.disk_hits.fetch_add(1, Ordering::Relaxed);
                from_disk = true;
                encoded = Some(Bytes::from(data));
            }
        }
        let encoded = match encoded {
            Some(data) => data,
            None => self.fetch(&request.url, limits.max_encoded_bytes).await?,
        };

        let permit = self
            .decode_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| MediaError::Cancelled)?;
        let budget = self.config.decode_budget;
        let fit = request.fit;
        let data = encoded.clone();
        let flag = cancel.clone();
        self.decodes.fetch_add(1, Ordering::Relaxed);
        let decoded = self
            .handle
            .spawn_blocking(move || {
                let _permit = permit;
                let control = DecodeControl {
                    deadline: Instant::now() + budget,
                    cancel: flag,
                };
                decode(&data, fit, &limits, &control)
            })
            .await
            .map_err(|_| MediaError::DecoderPanic)??;

        // Only data that decoded is kept on disk.
        if !from_disk && !cancel.load(Ordering::Relaxed) {
            if let Some(disk) = &self.disk {
                let disk = disk.clone();
                let _ = self
                    .handle
                    .spawn_blocking(move || {
                        if let Err(err) = disk.put(&disk_key, &encoded) {
                            tracing::debug!("image disk cache write failed: {err}");
                        }
                    })
                    .await;
            }
        }
        Ok(Arc::new(decoded))
    }

    async fn fetch(&self, url: &Url, max_bytes: usize) -> Result<Bytes, MediaError> {
        let _slot = self
            .fetch_slots
            .acquire()
            .await
            .map_err(|_| MediaError::Cancelled)?;
        self.fetches.fetch_add(1, Ordering::Relaxed);
        let bytes = tokio::time::timeout(
            self.config.fetch_timeout,
            self.fetcher.fetch(url, max_bytes),
        )
        .await
        .map_err(|_| MediaError::FetchTimeout)??;
        if bytes.len() > max_bytes {
            return Err(MediaError::TooLargeEncoded {
                size: bytes.len(),
                max: max_bytes,
            });
        }
        Ok(bytes)
    }
}

async fn run_flight(
    inner: Arc<Inner>,
    key: Key,
    request: MediaRequest,
    id: u64,
    cancel: Arc<AtomicBool>,
) {
    let outcome = inner.produce(&key, &request, &cancel).await;
    inner.finish(&key, id, outcome);
}
