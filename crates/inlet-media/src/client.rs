//! The client the UI talks to: memory cache, disk cache, deduplication,
//! cancellation, and the capped pools for fetching and decoding.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures_util::future::{select, Either};
use futures_util::StreamExt;
use tokio::runtime::Handle;
use tokio::sync::{watch, OnceCell, OwnedSemaphorePermit, Semaphore};
use tokio::task::AbortHandle;
use url::{Host, Url};

use crate::clock::{Clock, SystemClock};
use crate::decode::{inspect, DecodeControl};
use crate::disk::{key_for, DiskCache, DiskCacheConfig};
use crate::fetch::FetchResponse;
use crate::memory::ByteLru;
use crate::{Fetcher, Fit, ImageSink, KindLimits, Limits, MediaError, MediaKind};

const MIB: u64 = 1024 * 1024;

/// Settings of a [`MediaClient`]. `Default` gives the values the app uses, except
/// for `allowed_hosts`, which the app must fill in.
#[derive(Debug, Clone)]
pub struct MediaConfig {
    /// The hosts images may be fetched from: the instance's media proxy and CDN
    /// hosts from discovery, and any other host the app decides to trust. An entry
    /// is a host name (`media.example.com`) or `*.example.com` for all of its
    /// subdomains. **Empty means nothing is allowed.** Every URL also has to be
    /// `https` (plain `http` only for `localhost` and loopback addresses that are
    /// listed here), and carry no user name or password.
    pub allowed_hosts: Vec<String>,
    /// Byte budget of the memory cache, as counted by [`ImageSink::make`].
    /// Default 128 MiB.
    pub memory_cache_bytes: u64,
    /// Encoded images on disk. `None` turns the disk cache off (the default;
    /// use [`DiskCacheConfig::in_os_cache_dir`] to switch it on).
    pub disk_cache: Option<DiskCacheConfig>,
    /// Hard limits per kind of image.
    pub limits: Limits,
    /// Decodes that may run at the same time for images of any kind. Default: half
    /// the cores, 1 to 4. One more decode may run for emoji and avatars, in a slot
    /// that large attachments cannot take, so that a big picture never blocks them.
    pub decode_concurrency: usize,
    /// Memory the running decodes may need together, by the estimate that
    /// [`inspect`] makes of each. A quarter of it is for emoji and avatars only.
    /// A decode waits until its estimate fits. Default 512 MiB.
    pub decode_memory_bytes: u64,
    /// Requests that may fetch (or read the disk cache) and wait for a decode at
    /// the same time, which also bounds the encoded bytes that wait for a decode:
    /// at most this many times the encoded limit. Emoji and avatars have two more
    /// of their own. Default 8.
    pub fetch_concurrency: usize,
    /// Longest time one fetch, including reading the body, may take. Default 30 s.
    pub fetch_timeout: Duration,
    /// How long a failure that may pass (network, timeout, 5xx, 429) is remembered
    /// after its first occurrence. Every further failure of the same image doubles
    /// it, up to `failure_backoff_max`. A success clears it. Default 1 minute.
    pub failure_backoff: Duration,
    /// The same for failures that cannot pass (4xx, corrupt, too large, ...). Default 5 minutes.
    pub permanent_failure_backoff: Duration,
    /// The longest a failure is remembered. Default 1 hour.
    pub failure_backoff_max: Duration,
    /// The clock that the back-off reads. The system clock by default.
    pub clock: Arc<dyn Clock>,
}

impl Default for MediaConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        Self {
            allowed_hosts: Vec::new(),
            memory_cache_bytes: 128 * MIB,
            disk_cache: None,
            limits: Limits::default(),
            decode_concurrency: (cores / 2).clamp(1, 4),
            decode_memory_bytes: 512 * MIB,
            fetch_concurrency: 8,
            fetch_timeout: Duration::from_secs(30),
            failure_backoff: Duration::from_secs(60),
            permanent_failure_backoff: Duration::from_secs(300),
            failure_backoff_max: Duration::from_secs(3600),
            clock: Arc::new(SystemClock),
        }
    }
}

/// What the UI wants: one image at one size.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MediaRequest {
    /// Where to fetch it. The app builds it, with the media proxy's size
    /// parameters where it can (`size` for avatars, icons and emoji, `width` and
    /// `height` for attachments). See [`snap_proxy_size`](crate::snap_proxy_size).
    pub url: Url,
    /// Decides the limits.
    pub kind: MediaKind,
    /// The size to downscale to.
    pub fit: Fit,
    /// The longest side the app asked the media proxy for in `url`, if it did.
    /// The decode limits then shrink to four times that (at least 512 pixels), so
    /// an answer that is far larger than asked for is refused instead of decoded.
    /// It also tells two URLs of the same image at different sizes apart.
    pub proxy_size: Option<u32>,
}

impl MediaRequest {
    /// A request for the URL as it is.
    pub fn new(url: Url, kind: MediaKind, fit: Fit) -> Self {
        Self {
            url,
            kind,
            fit,
            proxy_size: None,
        }
    }

    /// States that `url` asks the media proxy for `size` pixels on the longest side.
    pub fn with_proxy_size(mut self, size: u32) -> Self {
        self.proxy_size = Some(size);
        self
    }

    fn limits(&self, base: &KindLimits) -> KindLimits {
        match self.proxy_size {
            Some(size) => base.for_proxy_size(size),
            None => base.clone(),
        }
    }
}

/// Counters, for the debug view and for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaStats {
    /// Images in the memory cache.
    pub memory_entries: usize,
    /// Bytes in the memory cache, as counted by the sink.
    pub memory_bytes: u64,
    /// Requests in progress.
    pub in_flight: usize,
    /// Callers waiting for the requests in progress, summed. Two callers asking
    /// for the same image share one request and count twice.
    pub waiters: usize,
    /// Requests that have an image to decode and wait for permission to start.
    pub waiting_to_decode: usize,
    /// Images that are remembered as failed.
    pub failures: usize,
    /// Calls to the fetcher so far.
    pub fetches: u64,
    /// Decodes started so far.
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
    proxy_size: Option<u32>,
}

impl Key {
    fn new(req: &MediaRequest) -> Self {
        Self {
            url: req.url.as_str().to_owned(),
            kind: req.kind,
            fit: req.fit.sanitized(),
            proxy_size: req.proxy_size,
        }
    }
}

type Outcome<H> = Result<H, MediaError>;
type Joined<S> = (
    watch::Receiver<Option<Outcome<<S as ImageSink>::Handle>>>,
    WaiterGuard<S>,
);

struct Flight<H> {
    id: u64,
    tx: watch::Sender<Option<Outcome<H>>>,
    waiters: usize,
    cancel: Arc<AtomicBool>,
    abort: Option<AbortHandle>,
}

struct Failure {
    until: Instant,
    count: u32,
    error: MediaError,
}

/// Decode slots and decode memory of one group of requests.
struct Lane {
    slots: Arc<Semaphore>,
    memory: Arc<Semaphore>,
    memory_units: u32,
}

impl Lane {
    fn new(slots: usize, memory_bytes: u64) -> Arc<Self> {
        let units = (memory_bytes / MIB).clamp(1, 1 << 20) as u32;
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(slots.max(1))),
            memory: Arc::new(Semaphore::new(units as usize)),
            memory_units: units,
        })
    }
}

/// Held while a decode runs.
struct DecodePermit {
    _slot: OwnedSemaphorePermit,
    _memory: OwnedSemaphorePermit,
}

/// A slot first, then the memory. Every caller takes them in this order, so two
/// callers can never wait for each other.
async fn acquire_lane(lane: Arc<Lane>, units: u32) -> Result<DecodePermit, MediaError> {
    let slot = lane
        .slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| MediaError::Cancelled)?;
    let memory = lane
        .memory
        .clone()
        .acquire_many_owned(units.clamp(1, lane.memory_units))
        .await
        .map_err(|_| MediaError::Cancelled)?;
    Ok(DecodePermit {
        _slot: slot,
        _memory: memory,
    })
}

/// Counts one up while it lives.
struct CountGuard<'a>(&'a AtomicU64);

impl<'a> CountGuard<'a> {
    fn new(counter: &'a AtomicU64) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for CountGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

enum Produced<H> {
    /// Found in the cache by the time the request started.
    Cached(H),
    /// New, with the bytes it keeps.
    Fresh(H, u64),
}

struct Inner<S: ImageSink> {
    handle: Handle,
    fetcher: Arc<dyn Fetcher>,
    sink: Arc<S>,
    config: MediaConfig,
    disk: OnceCell<Option<Arc<DiskCache>>>,
    memory: Mutex<ByteLru<Key, S::Handle>>,
    flights: Mutex<HashMap<Key, Flight<S::Handle>>>,
    failures: Mutex<HashMap<Key, Failure>>,
    general: Arc<Lane>,
    reserved: Arc<Lane>,
    pipeline: Arc<Semaphore>,
    pipeline_reserved: Arc<Semaphore>,
    disk_slots: Arc<Semaphore>,
    next_id: AtomicU64,
    fetches: AtomicU64,
    decodes: AtomicU64,
    waiting_to_decode: AtomicU64,
    memory_hits: AtomicU64,
    disk_hits: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn is_small(kind: MediaKind) -> bool {
    matches!(kind, MediaKind::Emoji | MediaKind::Avatar)
}

/// Loads images for the UI. Cheap to clone; all clones share the caches.
///
/// ```no_run
/// # use std::sync::Arc;
/// # use inlet_media::*;
/// # async fn demo(fetcher: Arc<dyn Fetcher>, url: url::Url) {
/// let config = MediaConfig {
///     allowed_hosts: vec!["media.example.com".into(), "*.cdn.example.com".into()],
///     ..MediaConfig::default()
/// };
/// let media = MediaClient::new(tokio::runtime::Handle::current(), fetcher, RawImages, config);
/// let req = MediaRequest::new(url, MediaKind::Avatar, Fit::square(88));
/// match media.load(req).await {
///     Ok(image) => { /* draw it */ }
///     Err(err) => { /* draw the placeholder, err says why */ }
/// }
/// # }
/// ```
pub struct MediaClient<S: ImageSink> {
    inner: Arc<Inner<S>>,
}

impl<S: ImageSink> Clone for MediaClient<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: ImageSink> MediaClient<S> {
    /// Creates a client. Work runs on `handle`'s runtime; decoding runs on its
    /// blocking pool, under the caps of `config`. The disk cache opens on first use.
    pub fn new(handle: Handle, fetcher: Arc<dyn Fetcher>, sink: S, config: MediaConfig) -> Self {
        let total_memory = config.decode_memory_bytes.max(MIB);
        let reserved_memory = total_memory / 4;
        Self {
            inner: Arc::new(Inner {
                handle,
                fetcher,
                sink: Arc::new(sink),
                disk: OnceCell::new(),
                memory: Mutex::new(ByteLru::new(config.memory_cache_bytes)),
                flights: Mutex::new(HashMap::new()),
                failures: Mutex::new(HashMap::new()),
                general: Lane::new(config.decode_concurrency, total_memory - reserved_memory),
                reserved: Lane::new(1, reserved_memory),
                pipeline: Arc::new(Semaphore::new(config.fetch_concurrency.max(1))),
                pipeline_reserved: Arc::new(Semaphore::new(2)),
                disk_slots: Arc::new(Semaphore::new(4)),
                next_id: AtomicU64::new(1),
                fetches: AtomicU64::new(0),
                decodes: AtomicU64::new(0),
                waiting_to_decode: AtomicU64::new(0),
                memory_hits: AtomicU64::new(0),
                disk_hits: AtomicU64::new(0),
                config,
            }),
        }
    }

    /// The image if it is already in memory. Never waits. Use it while painting,
    /// and call [`load`](Self::load) when it returns `None`.
    pub fn cached(&self, request: &MediaRequest) -> Option<S::Handle> {
        self.inner.memory_get(&Key::new(request))
    }

    /// Loads an image: from memory, else disk, else the network. Requests for the
    /// same URL, kind and size share one fetch and one decode.
    ///
    /// A failure is remembered, with a growing back-off (see [`MediaConfig`]), and
    /// returned at once until it ends, so calling `load` on every repaint of a
    /// missing image does not cause a fetch each time. [`forget_failure`](Self::forget_failure)
    /// ends it early.
    ///
    /// **Cancelling:** drop the returned future when the UI no longer needs the
    /// image (the row scrolled away, the view closed). When the last waiter of a
    /// request is dropped, its fetch is aborted and its decode stops at the next
    /// check. Waiters do not affect each other: one dropped future leaves the
    /// others running.
    pub async fn load(&self, request: MediaRequest) -> Result<S::Handle, MediaError> {
        validate_url(&request.url, &self.inner.config.allowed_hosts)?;
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

    /// Ends the back-off of one image, so the next `load` tries again.
    pub fn forget_failure(&self, request: &MediaRequest) {
        lock(&self.inner.failures).remove(&Key::new(request));
    }

    /// Drops every image from the memory cache. The disk cache is kept. The sink
    /// hears about every handle through [`ImageSink::evicted`].
    pub fn clear_memory(&self) {
        let all = lock(&self.inner.memory).drain();
        for handle in all {
            self.inner.notify_evicted(handle);
        }
    }

    /// Counters.
    pub fn stats(&self) -> MediaStats {
        let (memory_entries, memory_bytes) = {
            let memory = lock(&self.inner.memory);
            (memory.len(), memory.used())
        };
        let (in_flight, waiters) = {
            let flights = lock(&self.inner.flights);
            (flights.len(), flights.values().map(|f| f.waiters).sum())
        };
        MediaStats {
            memory_entries,
            memory_bytes,
            in_flight,
            waiters,
            waiting_to_decode: self.inner.waiting_to_decode.load(Ordering::Relaxed) as usize,
            failures: lock(&self.inner.failures).len(),
            fetches: self.inner.fetches.load(Ordering::Relaxed),
            decodes: self.inner.decodes.load(Ordering::Relaxed),
            memory_hits: self.inner.memory_hits.load(Ordering::Relaxed),
            disk_hits: self.inner.disk_hits.load(Ordering::Relaxed),
        }
    }
}

/// `https`, no credentials, host on the list. Plain `http` is only for loopback.
fn validate_url(url: &Url, allowed: &[String]) -> Result<(), MediaError> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(MediaError::InvalidUrl("credentials in the URL".into()));
    }
    let loopback = match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => return Err(MediaError::InvalidUrl("no host".into())),
    };
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err(MediaError::InvalidUrl("only https is allowed".into())),
    }
    let host = url.host_str().unwrap_or_default();
    if host_allowed(allowed, host) {
        Ok(())
    } else {
        Err(MediaError::HostNotAllowed(host.to_owned()))
    }
}

fn host_allowed(allowed: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    allowed.iter().any(|entry| {
        let entry = entry.trim().trim_end_matches('.').to_ascii_lowercase();
        match entry.strip_prefix("*.") {
            Some(suffix) => host
                .strip_suffix(suffix)
                .is_some_and(|rest| rest.ends_with('.')),
            None => host == entry,
        }
    })
}

/// Removes this waiter from its flight when dropped, and stops the flight when
/// it was the last one.
struct WaiterGuard<S: ImageSink> {
    inner: Arc<Inner<S>>,
    key: Key,
    id: u64,
}

impl<S: ImageSink> Drop for WaiterGuard<S> {
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

/// Ends the flight when the task stops without finishing: aborted, or killed by a
/// panic in the app's fetcher or sink. Without it the waiters would hang.
struct FinishGuard<S: ImageSink> {
    inner: Arc<Inner<S>>,
    key: Key,
    id: u64,
    done: bool,
}

impl<S: ImageSink> Drop for FinishGuard<S> {
    fn drop(&mut self) {
        if !self.done {
            let err = if std::thread::panicking() {
                MediaError::TaskFailed
            } else {
                MediaError::Cancelled
            };
            self.inner.finish(&self.key, self.id, Err(err));
        }
    }
}

impl<S: ImageSink> Inner<S> {
    fn memory_get(&self, key: &Key) -> Option<S::Handle> {
        let hit = lock(&self.memory).get(key);
        if hit.is_some() {
            self.memory_hits.fetch_add(1, Ordering::Relaxed);
        }
        hit
    }

    fn known_failure(&self, key: &Key) -> Option<MediaError> {
        let failures = lock(&self.failures);
        let failure = failures.get(key)?;
        (failure.until > self.config.clock.now()).then(|| failure.error.clone())
    }

    fn record_failure(&self, key: &Key, error: &MediaError) {
        let config = &self.config;
        let base = if error.is_permanent() {
            config.permanent_failure_backoff
        } else {
            config.failure_backoff
        };
        let now = self.config.clock.now();
        let mut failures = lock(&self.failures);
        let count = failures.get(key).map_or(0, |f| f.count).saturating_add(1);
        let delay = base
            .saturating_mul(1u32 << (count - 1).min(16))
            .min(config.failure_backoff_max);
        if failures.len() >= 4096 && !failures.contains_key(key) {
            failures.retain(|_, f| f.until > now);
            if failures.len() >= 4096 {
                if let Some(oldest) = failures
                    .iter()
                    .min_by_key(|(_, f)| f.until)
                    .map(|(k, _)| k.clone())
                {
                    failures.remove(&oldest);
                }
            }
        }
        failures.insert(
            key.clone(),
            Failure {
                until: now + delay,
                count,
                error: error.clone(),
            },
        );
    }

    /// Joins the running request for `key`, or starts one.
    fn join(self: &Arc<Self>, key: &Key, request: &MediaRequest) -> Joined<S> {
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

    fn finish(&self, key: &Key, id: u64, outcome: Result<Produced<S::Handle>, MediaError>) {
        let mut released: Vec<S::Handle> = Vec::new();
        let published: Outcome<S::Handle> = match outcome {
            Ok(Produced::Cached(handle)) => Ok(handle),
            Ok(Produced::Fresh(handle, bytes)) => {
                lock(&self.failures).remove(key);
                lock(&self.memory).insert(key.clone(), handle.clone(), bytes, &mut released);
                Ok(handle)
            }
            Err(err) => {
                if err != MediaError::Cancelled {
                    self.record_failure(key, &err);
                }
                Err(err)
            }
        };
        // The app's code runs after every lock is released, and before the waiters
        // are woken: when `load` returns, the evictions it caused have been reported.
        for handle in released {
            self.notify_evicted(handle);
        }
        let mut flights = lock(&self.flights);
        if flights.get(key).is_some_and(|f| f.id == id) {
            if let Some(flight) = flights.remove(key) {
                flight.tx.send_replace(Some(published));
            }
        }
    }

    /// Tells the sink. A panic in the app's code is caught and logged: it must never
    /// stop the request that caused the eviction from being published.
    fn notify_evicted(&self, handle: S::Handle) {
        let sink = &self.sink;
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.evicted(handle))).is_err()
        {
            tracing::error!("ImageSink::evicted panicked; the panic was caught");
        }
    }

    async fn disk(&self) -> Option<Arc<DiskCache>> {
        let config = self.config.disk_cache.clone()?;
        self.disk
            .get_or_init(|| async {
                let opened = self
                    .handle
                    .spawn_blocking(move || DiskCache::open(&config))
                    .await;
                match opened {
                    Ok(Ok(cache)) => Some(Arc::new(cache)),
                    Ok(Err(err)) => {
                        tracing::warn!("image disk cache is off: {err}");
                        None
                    }
                    Err(_) => None,
                }
            })
            .await
            .clone()
    }

    async fn acquire_pipeline(&self, small: bool) -> Result<OwnedSemaphorePermit, MediaError> {
        let general = Box::pin(self.pipeline.clone().acquire_owned());
        let permit = if small {
            let reserved = Box::pin(self.pipeline_reserved.clone().acquire_owned());
            match select(general, reserved).await {
                Either::Left((permit, _)) | Either::Right((permit, _)) => permit,
            }
        } else {
            general.await
        };
        permit.map_err(|_| MediaError::Cancelled)
    }

    async fn acquire_decode(&self, small: bool, units: u32) -> Result<DecodePermit, MediaError> {
        let general = Box::pin(acquire_lane(self.general.clone(), units));
        if !small {
            return general.await;
        }
        let reserved = Box::pin(acquire_lane(self.reserved.clone(), units));
        match select(general, reserved).await {
            Either::Left((permit, _)) | Either::Right((permit, _)) => permit,
        }
    }

    async fn read_disk(&self, disk: &Arc<DiskCache>, name: &str, max: usize) -> Option<Bytes> {
        let _slot = self.disk_slots.clone().acquire_owned().await.ok()?;
        let disk = disk.clone();
        let name = name.to_owned();
        let data = self
            .handle
            .spawn_blocking(move || disk.get(&name, max))
            .await
            .ok()
            .flatten()?;
        Some(Bytes::from(data))
    }

    async fn write_disk(&self, disk: &Arc<DiskCache>, name: String, data: Bytes) {
        let Ok(_slot) = self.disk_slots.clone().acquire_owned().await else {
            return;
        };
        let disk = disk.clone();
        let _ = self
            .handle
            .spawn_blocking(move || {
                if let Err(err) = disk.put(&name, &data) {
                    tracing::debug!("image disk cache write failed: {err}");
                }
            })
            .await;
    }

    async fn produce(
        &self,
        key: &Key,
        request: &MediaRequest,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Produced<S::Handle>, MediaError> {
        // A request that finished just before this one started has filled the cache.
        if let Some(handle) = self.memory_get(key) {
            return Ok(Produced::Cached(handle));
        }
        let small = is_small(request.kind);
        let limits = request.limits(self.config.limits.get(request.kind));
        let name = key_for(&key.url);
        let disk = self.disk().await;

        // The slot is held until the decode permits are taken, so the encoded bytes
        // that wait for a decode are bounded.
        let mut slot = Some(self.acquire_pipeline(small).await?);
        let mut from_disk = false;
        let mut encoded = None;
        if let Some(disk) = &disk {
            if let Some(data) = self.read_disk(disk, &name, limits.max_encoded_bytes).await {
                self.disk_hits.fetch_add(1, Ordering::Relaxed);
                from_disk = true;
                encoded = Some(data);
            }
        }
        let encoded = match encoded {
            Some(data) => data,
            None => {
                self.fetch_bytes(&request.url, limits.max_encoded_bytes)
                    .await?
            }
        };

        let first = self
            .decode_one(encoded.clone(), request, &limits, small, cancel, &mut slot)
            .await;
        let (outcome, to_store) = match first {
            // A file that is not an image we can read is damaged. Delete it and ask
            // the network once. A file that only breaks a limit of this request (a
            // size, a frame count) is fine for another kind or size: keep it, and
            // do not fetch what the same limits will refuse again.
            Err(err)
                if from_disk
                    && matches!(
                        err,
                        MediaError::Corrupt(_) | MediaError::Unsupported | MediaError::DecoderPanic
                    ) =>
            {
                tracing::debug!("cached image does not decode ({err}); fetching it again");
                if let Some(disk) = &disk {
                    let disk = disk.clone();
                    let name = name.clone();
                    let _ = self.handle.spawn_blocking(move || disk.remove(&name)).await;
                }
                drop(slot.take());
                slot = Some(self.acquire_pipeline(small).await?);
                let fresh = self
                    .fetch_bytes(&request.url, limits.max_encoded_bytes)
                    .await?;
                let result = self
                    .decode_one(fresh.clone(), request, &limits, small, cancel, &mut slot)
                    .await;
                (result, Some(fresh))
            }
            other => (other, (!from_disk).then_some(encoded)),
        };
        let (handle, bytes) = outcome?;

        // Only data that decoded is kept on disk.
        if let (Some(disk), Some(data)) = (&disk, to_store) {
            if !cancel.load(Ordering::Relaxed) {
                self.write_disk(disk, name, data).await;
            }
        }
        Ok(Produced::Fresh(handle, bytes))
    }

    /// Inspects, waits for decode permits (and then frees the pipeline slot), and
    /// decodes on a blocking thread.
    async fn decode_one(
        &self,
        encoded: Bytes,
        request: &MediaRequest,
        limits: &KindLimits,
        small: bool,
        cancel: &Arc<AtomicBool>,
        slot: &mut Option<OwnedSemaphorePermit>,
    ) -> Result<(S::Handle, u64), MediaError> {
        let inspected = inspect(encoded, limits)?;
        let units = inspected.estimated_peak_bytes().div_ceil(MIB).min(1 << 20) as u32;
        let permit = {
            // Counted back down even when this task is dropped while it waits.
            let _waiting = CountGuard::new(&self.waiting_to_decode);
            self.acquire_decode(small, units).await?
        };
        drop(slot.take());

        self.decodes.fetch_add(1, Ordering::Relaxed);
        let fit = request.fit;
        let limits = limits.clone();
        let flag = cancel.clone();
        let sink = self.sink.clone();
        self.handle
            .spawn_blocking(move || {
                let _permit = permit;
                let control = DecodeControl {
                    deadline: Instant::now() + limits.decode_budget,
                    cancel: flag,
                };
                let image = inspected.decode(fit, &limits, &control)?;
                Ok(sink.make(image))
            })
            .await
            .map_err(|_| MediaError::TaskFailed)?
    }

    async fn fetch_bytes(&self, url: &Url, max_bytes: usize) -> Result<Bytes, MediaError> {
        self.fetches.fetch_add(1, Ordering::Relaxed);
        let work = async {
            let response = self.fetcher.fetch(url).await?;
            collect(response, max_bytes).await
        };
        tokio::time::timeout(self.config.fetch_timeout, work)
            .await
            .map_err(|_| MediaError::FetchTimeout)?
    }
}

/// Reads the body up to `max` bytes. A longer announced length is refused without
/// reading, and a longer body stops at the first chunk that passes the limit; the
/// stream is dropped there, which cancels the download.
async fn collect(response: FetchResponse, max: usize) -> Result<Bytes, MediaError> {
    let FetchResponse {
        content_length,
        mut body,
    } = response;
    if let Some(length) = content_length {
        if length > max as u64 {
            return Err(MediaError::TooLargeEncoded {
                size: usize::try_from(length).unwrap_or(usize::MAX),
                max,
            });
        }
    }
    let capacity = content_length.map_or(64 * 1024, |l| l as usize).min(max);
    let mut buffer = BytesMut::with_capacity(capacity);
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        let size = buffer.len().saturating_add(chunk.len());
        if size > max {
            return Err(MediaError::TooLargeEncoded { size, max });
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(buffer.freeze())
}

async fn run_flight<S: ImageSink>(
    inner: Arc<Inner<S>>,
    key: Key,
    request: MediaRequest,
    id: u64,
    cancel: Arc<AtomicBool>,
) {
    let mut guard = FinishGuard {
        inner: inner.clone(),
        key: key.clone(),
        id,
        done: false,
    };
    let outcome = inner.produce(&key, &request, &cancel).await;
    inner.finish(&key, id, outcome);
    // Only now: if `finish` stops half way, the guard still publishes an answer.
    guard.done = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn host_matching() {
        let allowed = hosts(&["media.example.com", "*.cdn.example.net", "LOCALHOST"]);
        assert!(host_allowed(&allowed, "media.example.com"));
        assert!(host_allowed(&allowed, "media.example.com."));
        assert!(host_allowed(&allowed, "a.cdn.example.net"));
        assert!(host_allowed(&allowed, "a.b.cdn.example.net"));
        assert!(host_allowed(&allowed, "localhost"));
        assert!(!host_allowed(&allowed, "cdn.example.net"));
        assert!(!host_allowed(&allowed, "evilcdn.example.net"));
        assert!(!host_allowed(&allowed, "media.example.com.evil.net"));
        assert!(!host_allowed(&allowed, "example.com"));
        assert!(!host_allowed(&[], "media.example.com"));
    }

    #[test]
    fn url_rules() {
        let allowed = hosts(&["cdn.example", "localhost", "127.0.0.1"]);
        let ok = |s: &str| validate_url(&s.parse().unwrap(), &allowed);
        assert!(ok("https://cdn.example/a.png").is_ok());
        assert!(matches!(
            ok("http://cdn.example/a.png"),
            Err(MediaError::InvalidUrl(_))
        ));
        assert!(ok("http://localhost:8080/a.png").is_ok());
        assert!(ok("http://127.0.0.1/a.png").is_ok());
        assert!(matches!(
            ok("https://u:p@cdn.example/a.png"),
            Err(MediaError::InvalidUrl(_))
        ));
        assert!(matches!(
            ok("ftp://cdn.example/a.png"),
            Err(MediaError::InvalidUrl(_))
        ));
        assert!(matches!(
            ok("file:///etc/passwd"),
            Err(MediaError::InvalidUrl(_))
        ));
        assert!(matches!(
            ok("https://other.example/a.png"),
            Err(MediaError::HostNotAllowed(_))
        ));
        assert!(matches!(
            ok("https://10.0.0.1/a.png"),
            Err(MediaError::HostNotAllowed(_))
        ));
    }
}
