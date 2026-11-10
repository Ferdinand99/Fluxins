//! Client-side rate limiting.
//!
//! Fluxer counts the global limit of 50 requests per second **per user**
//! (every session token of one account shares it), and it revokes the token
//! that tips it over. So that Inlet can never cause that, even with Fluxer's
//! own client running next to it, every request with a token passes through
//! one [`RateLimiter`] per (instance, user):
//!
//! - a hard cap of [`HARD_CAP_PER_SECOND`] (20) requests in any rolling
//!   one-second window. No configuration can raise it;
//! - inside that, a token bucket ([`RateLimitConfig`], default 10/s with a
//!   burst of 20; both are clamped to 20);
//! - a pause of every request after a 429 with `global: true`;
//! - per-route waits from `X-RateLimit-Remaining`/`X-RateLimit-Reset-After`
//!   and from the `retry_after` of a route 429.
//!
//! Limiters live in a process-wide registry, so a new `Client` for the same
//! account gets the same limiter, with its pauses. An entry is dropped only
//! when no client holds it, it has been idle for 10 minutes, and no global
//! pause is running.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::time::{sleep, Instant};

/// No more than this many requests start in any rolling one-second window.
pub const HARD_CAP_PER_SECOND: u32 = 20;

/// The longest wait this crate accepts from a server value (`Retry-After`,
/// `retry_after`, reset-after). Larger, NaN and infinite values become this.
pub const MAX_SERVER_WAIT: Duration = Duration::from_secs(300);

/// Turns a server-given number of seconds into a safe `Duration`:
/// negative values become 0; NaN, infinite and values above `max` become `max`.
pub fn clamp_server_secs(secs: f64, max: Duration) -> Duration {
    if secs.is_nan() || secs.is_infinite() || secs >= max.as_secs_f64() {
        return max;
    }
    if secs <= 0.0 {
        return Duration::ZERO;
    }
    Duration::try_from_secs_f64(secs).unwrap_or(max).min(max)
}

/// Limits for one account. Values above the hard cap are clamped down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitConfig {
    /// Sustained requests per second (at most 20).
    pub rate_per_sec: f64,
    /// Requests that may start at once after a quiet period (at most 20).
    pub burst: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            rate_per_sec: 10.0,
            burst: 20,
        }
    }
}

impl RateLimitConfig {
    /// The config with every value inside the allowed range.
    pub fn clamped(self) -> Self {
        let cap = f64::from(HARD_CAP_PER_SECOND);
        let rate = if self.rate_per_sec.is_finite() && self.rate_per_sec > 0.0 {
            self.rate_per_sec.min(cap)
        } else {
            Self::default().rate_per_sec
        };
        Self {
            rate_per_sec: rate,
            burst: self.burst.clamp(1, HARD_CAP_PER_SECOND),
        }
    }

    /// The most requests that can start inside any one-second window.
    pub fn max_per_second(&self) -> u32 {
        let c = self.clamped();
        (c.burst + c.rate_per_sec.ceil() as u32).min(HARD_CAP_PER_SECOND)
    }
}

/// Route entries kept before old ones are pruned.
const MAX_ROUTES: usize = 512;

#[derive(Debug, Default)]
struct RouteState {
    remaining: Option<u32>,
    reset_at: Option<Instant>,
    blocked_until: Option<Instant>,
}

impl RouteState {
    fn expired(&self, now: Instant) -> bool {
        self.blocked_until.is_none_or(|u| u <= now) && self.reset_at.is_none_or(|u| u <= now)
    }
}

#[derive(Debug)]
struct State {
    tokens: f64,
    last_refill: Instant,
    /// Start times of the requests in the last second (for the hard cap).
    recent: VecDeque<Instant>,
    global_until: Option<Instant>,
    routes: HashMap<String, RouteState>,
    /// Last time a request went through, for registry eviction.
    last_used: Instant,
}

/// One account's limiter. Shared behind an [`Arc`].
#[derive(Debug)]
pub struct RateLimiter {
    config: RateLimitConfig,
    /// Callers queue here (FIFO) for a global slot.
    queue: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    /// Typing debounce, ack coalescing and traits of the account.
    pub(crate) coalesce: crate::coalesce::Shared,
}

/// What a limiter is shared by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum LimiterKey {
    /// Before the user ID is known: the instance origin and a token digest.
    Token(String, [u8; 32]),
    /// The instance origin and the user ID.
    User(String, String),
}

type Registry = Mutex<HashMap<LimiterKey, Arc<RateLimiter>>>;

/// A registry entry no client holds is dropped after this much idle time
/// (unless a global pause is still running).
pub(crate) const REGISTRY_IDLE: Duration = Duration::from_secs(10 * 60);

/// Drops limiters that no client holds, that have been idle for `idle`, and
/// that have no global pause running.
pub(crate) fn evict_idle(map: &mut HashMap<LimiterKey, Arc<RateLimiter>>, idle: Duration) {
    let now = Instant::now();
    // One limiter can sit under two keys (token and user); count those references.
    let mut held_by_registry: HashMap<*const RateLimiter, usize> = HashMap::new();
    for l in map.values() {
        *held_by_registry.entry(Arc::as_ptr(l)).or_default() += 1;
    }
    map.retain(|_, l| {
        let registry_refs = held_by_registry.get(&Arc::as_ptr(l)).copied().unwrap_or(0);
        Arc::strong_count(l) > registry_refs || !l.is_idle(idle, now)
    });
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

impl RateLimiter {
    /// A new, independent limiter (not in the registry). The config is clamped.
    pub fn new(config: RateLimitConfig) -> Self {
        let config = config.clamped();
        Self {
            config,
            queue: tokio::sync::Mutex::new(()),
            state: Mutex::new(State {
                tokens: f64::from(config.burst),
                last_refill: Instant::now(),
                recent: VecDeque::new(),
                global_until: None,
                routes: HashMap::new(),
                last_used: Instant::now(),
            }),
            coalesce: crate::coalesce::Shared::default(),
        }
    }

    /// The registry's limiter for `key`, created with `config` if there is none.
    pub(crate) fn shared(key: LimiterKey, config: RateLimitConfig) -> Arc<Self> {
        let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
        evict_idle(&mut map, REGISTRY_IDLE);
        map.entry(key)
            .or_insert_with(|| Arc::new(Self::new(config)))
            .clone()
    }

    /// Links `user` to `current` (the token limiter in use). If the user already
    /// has a limiter (another token of the same account), that one is returned
    /// and should be used from now on.
    pub(crate) fn adopt_user(origin: &str, user_id: &str, current: &Arc<Self>) -> Arc<Self> {
        let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
        map.entry(LimiterKey::User(origin.to_owned(), user_id.to_owned()))
            .or_insert_with(|| current.clone())
            .clone()
    }

    /// The configured limits (already clamped).
    pub fn config(&self) -> RateLimitConfig {
        self.config
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits until a request on `route` may start, then takes one slot.
    pub async fn acquire(&self, route: &str) {
        'outer: loop {
            // Route waits happen outside the global queue, so a busy route
            // never holds up requests on other routes.
            loop {
                let wait = {
                    let state = self.lock();
                    route_wait(&state, route, Instant::now())
                };
                match wait {
                    Some(d) => sleep(d).await,
                    None => break,
                }
            }

            let turn = self.queue.lock().await;
            loop {
                let wait = {
                    let mut state = self.lock();
                    let now = Instant::now();
                    state.last_used = now;
                    self.refill(&mut state, now);
                    while state.recent.front().is_some_and(|t| {
                        now.saturating_duration_since(*t) >= Duration::from_secs(1)
                    }) {
                        state.recent.pop_front();
                    }
                    if let Some(until) = state.global_until.filter(|u| *u > now) {
                        until - now
                    } else if route_wait(&state, route, now).is_some() {
                        // A route 429 arrived while this caller was queued:
                        // give up the turn and wait outside the queue.
                        drop(state);
                        drop(turn);
                        continue 'outer;
                    } else if state.recent.len() >= HARD_CAP_PER_SECOND as usize {
                        let oldest = state.recent.front().copied().unwrap_or(now);
                        (oldest + Duration::from_secs(1)).saturating_duration_since(now)
                    } else if state.tokens >= 1.0 {
                        state.tokens -= 1.0;
                        state.recent.push_back(now);
                        if let Some(r) = state.routes.get_mut(route) {
                            if let Some(rem) = r.remaining.as_mut() {
                                *rem = rem.saturating_sub(1);
                            }
                        }
                        return;
                    } else {
                        Duration::from_secs_f64((1.0 - state.tokens) / self.config.rate_per_sec)
                    }
                };
                // Only global waits (pause, cap, token refill) happen in the queue.
                sleep(wait.max(Duration::from_millis(1))).await;
            }
        }
    }

    /// Whether nothing used this limiter for `idle` and no global pause is running.
    fn is_idle(&self, idle: Duration, now: Instant) -> bool {
        let state = self.lock();
        now.saturating_duration_since(state.last_used) >= idle
            && state.global_until.is_none_or(|u| u <= now)
    }

    fn refill(&self, state: &mut State, now: Instant) {
        let elapsed = now
            .saturating_duration_since(state.last_refill)
            .as_secs_f64();
        state.tokens =
            (state.tokens + elapsed * self.config.rate_per_sec).min(f64::from(self.config.burst));
        state.last_refill = now;
    }

    fn route_entry<'a>(state: &'a mut State, route: &str, now: Instant) -> &'a mut RouteState {
        if state.routes.len() >= MAX_ROUTES && !state.routes.contains_key(route) {
            state.routes.retain(|_, r| !r.expired(now));
            if state.routes.len() >= MAX_ROUTES {
                state.routes.clear();
            }
        }
        state.routes.entry(route.to_owned()).or_default()
    }

    /// Records the route bucket headers of a response. Durations are clamped to [`MAX_SERVER_WAIT`].
    pub fn record_headers(
        &self,
        route: &str,
        remaining: Option<u32>,
        reset_after: Option<Duration>,
    ) {
        if remaining.is_none() && reset_after.is_none() {
            return;
        }
        let mut state = self.lock();
        let now = Instant::now();
        let entry = Self::route_entry(&mut state, route, now);
        if remaining.is_some() {
            entry.remaining = remaining;
        }
        if let Some(d) = reset_after {
            entry.reset_at = Some(now + d.min(MAX_SERVER_WAIT));
        }
    }

    /// Records a 429. A global one pauses every request of this account.
    /// `retry_after` is clamped to [`MAX_SERVER_WAIT`].
    pub fn record_429(&self, route: &str, retry_after: Duration, global: bool) {
        let mut state = self.lock();
        let now = Instant::now();
        let until = now + retry_after.min(MAX_SERVER_WAIT);
        if global {
            state.global_until = Some(state.global_until.map_or(until, |u| u.max(until)));
        } else {
            let entry = Self::route_entry(&mut state, route, now);
            entry.blocked_until = Some(entry.blocked_until.map_or(until, |u| u.max(until)));
            entry.remaining = Some(0);
            entry.reset_at = Some(entry.reset_at.map_or(until, |u| u.max(until)));
        }
    }

    #[cfg(test)]
    fn route_count(&self) -> usize {
        self.lock().routes.len()
    }
}

fn route_wait(state: &State, route: &str, now: Instant) -> Option<Duration> {
    let r = state.routes.get(route)?;
    if let Some(until) = r.blocked_until.filter(|u| *u > now) {
        return Some(until - now);
    }
    if r.remaining == Some(0) {
        if let Some(reset) = r.reset_at.filter(|u| *u > now) {
            return Some(reset - now);
        }
    }
    None
}

/// The rate limit key of a request: the method and the path without the query.
pub(crate) fn route_key(method: &reqwest::Method, path: &str) -> String {
    let path = path.split('?').next().unwrap_or(path);
    format!("{method} {path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run_concurrent(
        limiter: Arc<RateLimiter>,
        callers: usize,
        route: impl Fn(usize) -> String,
    ) -> Vec<Instant> {
        let times = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for i in 0..callers {
            let limiter = limiter.clone();
            let times = times.clone();
            let route = route(i);
            handles.push(tokio::spawn(async move {
                limiter.acquire(&route).await;
                times.lock().unwrap().push(Instant::now());
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let mut v = times.lock().unwrap().clone();
        v.sort();
        v
    }

    fn max_in_window(times: &[Instant], window: Duration) -> usize {
        let mut best = 0;
        let mut start = 0;
        for end in 0..times.len() {
            while times[end] - times[start] >= window {
                start += 1;
            }
            best = best.max(end - start + 1);
        }
        best
    }

    #[tokio::test(start_paused = true)]
    async fn never_exceeds_hard_cap_under_concurrency() {
        let limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));
        let times = run_concurrent(limiter, 300, |i| format!("GET /r{}", i % 7)).await;
        assert_eq!(times.len(), 300);
        let peak = max_in_window(&times, Duration::from_secs(1));
        assert!(peak <= HARD_CAP_PER_SECOND as usize, "peak {peak}");
        // 280 requests after the burst at 10/s take about 28 s.
        let span = *times.last().unwrap() - times[0];
        assert!(span >= Duration::from_secs(27), "span {span:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn config_cannot_raise_the_cap() {
        let wild = RateLimitConfig {
            rate_per_sec: 1_000.0,
            burst: 1_000,
        };
        assert_eq!(wild.clamped().burst, 20);
        assert_eq!(wild.clamped().rate_per_sec, 20.0);
        assert_eq!(wild.max_per_second(), 20);
        let limiter = Arc::new(RateLimiter::new(wild));
        let times = run_concurrent(limiter, 200, |_| "GET /x".into()).await;
        assert!(max_in_window(&times, Duration::from_secs(1)) <= 20);
        // NaN and zero fall back to safe values instead of panicking.
        let odd = RateLimitConfig {
            rate_per_sec: f64::NAN,
            burst: 0,
        }
        .clamped();
        assert_eq!(odd.rate_per_sec, 10.0);
        assert_eq!(odd.burst, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn bursts_then_sustains_rate() {
        let config = RateLimitConfig {
            rate_per_sec: 5.0,
            burst: 3,
        };
        let limiter = Arc::new(RateLimiter::new(config));
        let times = run_concurrent(limiter, 23, |_| "GET /x".into()).await;
        let start = times[0];
        assert!(times[2] - start < Duration::from_millis(5));
        let total = *times.last().unwrap() - start;
        assert!(
            total >= Duration::from_millis(3_900) && total <= Duration::from_millis(4_200),
            "{total:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn global_429_pauses_every_route() {
        let limiter = RateLimiter::new(RateLimitConfig::default());
        limiter.record_429("GET /a", Duration::from_secs(3), true);
        let start = Instant::now();
        limiter.acquire("GET /b").await;
        assert!(Instant::now() - start >= Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn route_429_only_blocks_that_route() {
        let limiter = RateLimiter::new(RateLimitConfig::default());
        limiter.record_429("GET /a", Duration::from_secs(5), false);
        let start = Instant::now();
        limiter.acquire("GET /b").await;
        assert!(Instant::now() - start < Duration::from_millis(10));
        limiter.acquire("GET /a").await;
        assert!(Instant::now() - start >= Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn route_429_during_queue_wait_is_honoured() {
        let limiter = Arc::new(RateLimiter::new(RateLimitConfig {
            rate_per_sec: 1.0,
            burst: 1,
        }));
        limiter.acquire("GET /a").await;
        // The next call waits about 1 s for a token; a route 429 arrives meanwhile.
        let l2 = limiter.clone();
        let task = tokio::spawn(async move {
            let start = Instant::now();
            l2.acquire("GET /a").await;
            Instant::now() - start
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        limiter.record_429("GET /a", Duration::from_secs(4), false);
        let waited = task.await.unwrap();
        assert!(waited >= Duration::from_secs(4), "{waited:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn queued_route_block_does_not_stall_other_routes() {
        let limiter = Arc::new(RateLimiter::new(RateLimitConfig {
            rate_per_sec: 1.0,
            burst: 1,
        }));
        limiter.acquire("POST /a").await;
        // A second call on route A waits in the queue for a token...
        let la = limiter.clone();
        let a = tokio::spawn(async move {
            let start = Instant::now();
            la.acquire("POST /a").await;
            Instant::now() - start
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        // ...when route A gets a 60 s 429.
        limiter.record_429("POST /a", Duration::from_secs(60), false);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // A call on route B must not wait behind it.
        let lb = limiter.clone();
        let b = tokio::spawn(async move {
            let start = Instant::now();
            lb.acquire("GET /b").await;
            Instant::now() - start
        });
        let waited_b = b.await.unwrap();
        assert!(
            waited_b < Duration::from_secs(2),
            "route B waited {waited_b:?}"
        );
        let waited_a = a.await.unwrap();
        assert!(
            waited_a >= Duration::from_secs(60),
            "route A waited {waited_a:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_registry_entries_are_evicted() {
        let mut map: HashMap<LimiterKey, Arc<RateLimiter>> = HashMap::new();
        let key = |n: u8| LimiterKey::Token("https://evict".into(), [n; 32]);
        let held = Arc::new(RateLimiter::new(RateLimitConfig::default()));
        map.insert(key(1), held.clone());
        map.insert(
            key(2),
            Arc::new(RateLimiter::new(RateLimitConfig::default())),
        );
        let paused = Arc::new(RateLimiter::new(RateLimitConfig::default()));
        paused.record_429("GET /x", MAX_SERVER_WAIT, true);
        map.insert(key(3), paused);
        // The same limiter under a token key and a user key, held by nobody else.
        let shared = Arc::new(RateLimiter::new(RateLimitConfig::default()));
        map.insert(key(4), shared.clone());
        map.insert(LimiterKey::User("https://evict".into(), "9".into()), shared);

        evict_idle(&mut map, REGISTRY_IDLE);
        assert_eq!(map.len(), 5, "nothing is idle yet");

        tokio::time::advance(Duration::from_secs(11 * 60)).await;
        evict_idle(&mut map, REGISTRY_IDLE);
        // Kept: the one a client holds. Dropped: the idle ones (the pause ended after 300 s).
        assert!(map.contains_key(&key(1)));
        assert_eq!(map.len(), 1, "{:?}", map.keys().collect::<Vec<_>>());
        drop(held);
    }

    #[tokio::test(start_paused = true)]
    async fn running_global_pause_keeps_the_entry() {
        let mut map: HashMap<LimiterKey, Arc<RateLimiter>> = HashMap::new();
        let key = LimiterKey::Token("https://pause".into(), [1; 32]);
        let l = Arc::new(RateLimiter::new(RateLimitConfig::default()));
        map.insert(key.clone(), l.clone());
        drop(l);
        // Idle for longer than the threshold, but the pause still runs.
        evict_idle(&mut map, Duration::ZERO);
        assert!(map.is_empty());
        let l = Arc::new(RateLimiter::new(RateLimitConfig::default()));
        l.record_429("GET /x", MAX_SERVER_WAIT, true);
        map.insert(key.clone(), l);
        evict_idle(&mut map, Duration::ZERO);
        assert!(map.contains_key(&key));
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_route_bucket_waits_for_reset() {
        let limiter = RateLimiter::new(RateLimitConfig::default());
        limiter.record_headers("POST /m", Some(0), Some(Duration::from_secs(2)));
        let start = Instant::now();
        limiter.acquire("POST /m").await;
        assert!(Instant::now() - start >= Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn huge_waits_are_clamped() {
        let limiter = RateLimiter::new(RateLimitConfig::default());
        limiter.record_429("GET /a", Duration::MAX, true);
        limiter.record_headers("GET /b", Some(0), Some(Duration::MAX));
        let start = Instant::now();
        limiter.acquire("GET /c").await;
        assert!(Instant::now() - start <= MAX_SERVER_WAIT + Duration::from_secs(1));
    }

    #[test]
    fn clamp_server_values() {
        let max = MAX_SERVER_WAIT;
        assert_eq!(clamp_server_secs(f64::INFINITY, max), max);
        assert_eq!(clamp_server_secs(f64::NEG_INFINITY, max), max);
        assert_eq!(clamp_server_secs(f64::NAN, max), max);
        assert_eq!(clamp_server_secs(1e20, max), max);
        assert_eq!(clamp_server_secs(1e300, max), max);
        assert_eq!(clamp_server_secs(-5.0, max), Duration::ZERO);
        assert_eq!(clamp_server_secs(0.25, max), Duration::from_millis(250));
    }

    #[test]
    fn route_map_is_bounded() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let limiter = RateLimiter::new(RateLimitConfig::default());
            for i in 0..5_000 {
                limiter.record_headers(
                    &format!("GET /m/{i}"),
                    Some(3),
                    Some(Duration::from_secs(60)),
                );
            }
            assert!(limiter.route_count() <= MAX_ROUTES);
        });
    }

    #[test]
    fn same_key_shares_limiter_and_user_adoption() {
        let key = |n: u8| LimiterKey::Token("https://x".into(), [n; 32]);
        let a = RateLimiter::shared(key(7), RateLimitConfig::default());
        let b = RateLimiter::shared(key(7), RateLimitConfig::default());
        let c = RateLimiter::shared(key(8), RateLimitConfig::default());
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
        // Two tokens of one user end up on one limiter.
        let ua = RateLimiter::adopt_user("https://x", "42", &a);
        let uc = RateLimiter::adopt_user("https://x", "42", &c);
        assert!(Arc::ptr_eq(&ua, &a));
        assert!(Arc::ptr_eq(&uc, &a));
    }
}
