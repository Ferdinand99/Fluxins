//! Client-side rate limiting.
//!
//! Fluxer revokes a user token when the token goes over the global limit of
//! 50 requests per second. To make that impossible, every request made with a
//! token passes through one [`RateLimiter`] shared by everything that uses the
//! token (see [`RateLimiter::for_token`]). The limiter enforces:
//!
//! - a hard global cap: a token bucket with [`RateLimitConfig::rate_per_sec`]
//!   (default 10) and [`RateLimitConfig::burst`] (default 20). In any one
//!   second at most `burst + rate_per_sec` (30) requests can start;
//! - a pause of every request after a 429 with `global: true`;
//! - per-route waits from `X-RateLimit-Remaining`/`X-RateLimit-Reset-After`
//!   and from the `retry_after` of a route 429.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use tokio::time::{sleep, Instant};

/// Limits for one token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitConfig {
    /// Sustained requests per second.
    pub rate_per_sec: f64,
    /// Requests that may start at once after a quiet period.
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
    /// The most requests that can start inside any one-second window.
    pub fn max_per_second(&self) -> u32 {
        self.burst + self.rate_per_sec.ceil() as u32
    }
}

#[derive(Debug, Default)]
struct RouteState {
    /// Requests the route bucket still allows, from the last response.
    remaining: Option<u32>,
    /// When the route bucket is full again.
    reset_at: Option<Instant>,
    /// Set by a route 429: no request on this route before this instant.
    blocked_until: Option<Instant>,
}

#[derive(Debug)]
struct State {
    tokens: f64,
    last_refill: Instant,
    global_until: Option<Instant>,
    routes: HashMap<String, RouteState>,
}

/// One token's limiter. Cheap to share behind an [`Arc`].
#[derive(Debug)]
pub struct RateLimiter {
    config: RateLimitConfig,
    /// Callers queue here (FIFO) for a global slot.
    queue: tokio::sync::Mutex<()>,
    state: Mutex<State>,
}

type Registry = Mutex<HashMap<[u8; 32], Weak<RateLimiter>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

impl RateLimiter {
    /// A new, independent limiter. Prefer [`RateLimiter::for_token`] for real tokens.
    pub fn new(config: RateLimitConfig) -> Self {
        assert!(config.rate_per_sec > 0.0, "rate must be positive");
        assert!(config.burst >= 1, "burst must be at least 1");
        Self {
            config,
            queue: tokio::sync::Mutex::new(()),
            state: Mutex::new(State {
                tokens: f64::from(config.burst),
                last_refill: Instant::now(),
                global_until: None,
                routes: HashMap::new(),
            }),
        }
    }

    /// The limiter for a token fingerprint. Every caller with the same token
    /// gets the same limiter for as long as one of them holds it.
    pub(crate) fn for_token(fingerprint: [u8; 32], config: RateLimitConfig) -> Arc<Self> {
        let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, weak| weak.strong_count() > 0);
        if let Some(existing) = map.get(&fingerprint).and_then(Weak::upgrade) {
            return existing;
        }
        let limiter = Arc::new(Self::new(config));
        map.insert(fingerprint, Arc::downgrade(&limiter));
        limiter
    }

    /// The configured limits.
    pub fn config(&self) -> RateLimitConfig {
        self.config
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits until a request on `route` may start, then takes one slot.
    pub async fn acquire(&self, route: &str) {
        // Route waits first, outside the global queue, so a busy route does not
        // hold up requests on other routes.
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

        let _turn = self.queue.lock().await;
        loop {
            let wait = {
                let mut state = self.lock();
                let now = Instant::now();
                self.refill(&mut state, now);
                if let Some(until) = state.global_until.filter(|u| *u > now) {
                    until - now
                } else if state.tokens >= 1.0 {
                    state.tokens -= 1.0;
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
            sleep(wait.max(Duration::from_millis(1))).await;
        }
    }

    fn refill(&self, state: &mut State, now: Instant) {
        let elapsed = now
            .saturating_duration_since(state.last_refill)
            .as_secs_f64();
        state.tokens =
            (state.tokens + elapsed * self.config.rate_per_sec).min(f64::from(self.config.burst));
        state.last_refill = now;
    }

    /// Records the route bucket headers of a response.
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
        let entry = state.routes.entry(route.to_owned()).or_default();
        if remaining.is_some() {
            entry.remaining = remaining;
        }
        if let Some(d) = reset_after {
            entry.reset_at = Some(Instant::now() + d);
        }
    }

    /// Records a 429. A global one pauses every request with this token.
    pub fn record_429(&self, route: &str, retry_after: Duration, global: bool) {
        let mut state = self.lock();
        let until = Instant::now() + retry_after;
        if global {
            state.global_until = Some(state.global_until.map_or(until, |u| u.max(until)));
        } else {
            let entry = state.routes.entry(route.to_owned()).or_default();
            entry.blocked_until = Some(entry.blocked_until.map_or(until, |u| u.max(until)));
            entry.remaining = Some(0);
            entry.reset_at = Some(entry.reset_at.map_or(until, |u| u.max(until)));
        }
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
    async fn never_exceeds_cap_under_concurrency() {
        let config = RateLimitConfig::default();
        let limiter = Arc::new(RateLimiter::new(config));
        let times = run_concurrent(limiter, 300, |i| format!("GET /r{}", i % 7)).await;
        assert_eq!(times.len(), 300);
        let peak = max_in_window(&times, Duration::from_secs(1));
        assert!(peak <= config.max_per_second() as usize, "peak {peak}");
        assert!(
            peak < 50,
            "peak {peak} must stay below the API's global limit"
        );
        // 280 requests after the burst at 10/s take about 28 s.
        let span = *times.last().unwrap() - times[0];
        assert!(span >= Duration::from_secs(27), "span {span:?}");
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
        // The burst starts at once, then one every 200 ms.
        assert!(times[2] - start < Duration::from_millis(5));
        let total = *times.last().unwrap() - start;
        assert!(
            total >= Duration::from_millis(3_900) && total <= Duration::from_millis(4_200),
            "{total:?}"
        );
        assert!(max_in_window(&times, Duration::from_secs(1)) <= config.max_per_second() as usize);
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
    async fn exhausted_route_bucket_waits_for_reset() {
        let limiter = RateLimiter::new(RateLimitConfig::default());
        limiter.record_headers("POST /m", Some(0), Some(Duration::from_secs(2)));
        let start = Instant::now();
        limiter.acquire("POST /m").await;
        assert!(Instant::now() - start >= Duration::from_secs(2));
    }

    #[test]
    fn same_token_shares_limiter() {
        let a = RateLimiter::for_token([7; 32], RateLimitConfig::default());
        let b = RateLimiter::for_token([7; 32], RateLimitConfig::default());
        let c = RateLimiter::for_token([8; 32], RateLimitConfig::default());
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
    }
}
