mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use common::*;
use image::RgbaImage;
use inlet_media::{
    DecodedImage, DiskCacheConfig, FetchError, FetchResponse, Fit, ImageSink, MediaClient,
    MediaConfig, MediaError, MediaKind, MediaRequest, RawImages,
};

fn client(fetcher: &Arc<MockFetcher>, config: MediaConfig) -> MediaClient<RawImages> {
    MediaClient::new(
        tokio::runtime::Handle::current(),
        fetcher.clone(),
        RawImages,
        config,
    )
}

fn avatar(path: &str, size: u32) -> MediaRequest {
    MediaRequest::new(url(path), MediaKind::Avatar, Fit::square(size))
}

fn attachment(path: &str) -> MediaRequest {
    MediaRequest::new(url(path), MediaKind::Attachment, Fit::square(32))
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..300 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loads_and_caches_in_memory() {
    let fetcher = MockFetcher::new();
    fetcher.serve("a.png", png(&split(200, 200)));
    let media = client(&fetcher, config());

    assert!(media.cached(&avatar("a.png", 40)).is_none());
    let image = media.load(avatar("a.png", 40)).await.unwrap();
    assert_eq!((image.width, image.height), (40, 40));
    assert!(media.cached(&avatar("a.png", 40)).is_some());
    let again = media.load(avatar("a.png", 40)).await.unwrap();
    assert!(Arc::ptr_eq(&image, &again));
    assert_eq!(fetcher.calls(), 1);

    let stats = media.stats();
    assert_eq!(stats.memory_entries, 1);
    assert_eq!(stats.memory_bytes, 40 * 40 * 4);
    assert_eq!(stats.decodes, 1);
    assert_eq!(stats.in_flight, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_for_one_image_share_a_fetch_and_a_decode() {
    let fetcher = MockFetcher::gated();
    fetcher.serve("a.png", png(&split(100, 100)));
    let media = client(&fetcher, config());

    let tasks: Vec<_> = (0..25)
        .map(|_| {
            let media = media.clone();
            tokio::spawn(async move { media.load(avatar("a.png", 44)).await })
        })
        .collect();
    // Every caller has joined the one request before the fetch is let go.
    wait_until("all 25 callers to join", || media.stats().waiters == 25).await;
    assert_eq!(media.stats().in_flight, 1);
    fetcher.open_gate();

    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.unwrap().unwrap());
    }
    assert_eq!(fetcher.calls(), 1);
    assert_eq!(media.stats().decodes, 1);
    assert!(results.iter().all(|r| Arc::ptr_eq(r, &results[0])));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_sizes_are_separate_requests() {
    let fetcher = MockFetcher::new();
    fetcher.serve("a.png", png(&split(200, 200)));
    let media = client(&fetcher, config());
    let (small, big) = tokio::join!(
        media.load(avatar("a.png", 40)),
        media.load(avatar("a.png", 96))
    );
    assert_eq!(small.unwrap().width, 40);
    assert_eq!(big.unwrap().width, 96);
    assert_eq!(media.stats().memory_entries, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_proxy_size_is_part_of_the_identity_and_tightens_the_limits() {
    let fetcher = MockFetcher::new();
    fetcher.serve("big.png", png(&solid(1000, 1000, RED)));
    let media = client(&fetcher, config());
    // Asked for 128 pixels, got 1000: that is not what the proxy does.
    let err = media
        .load(avatar("big.png", 40).with_proxy_size(128))
        .await
        .unwrap_err();
    assert!(matches!(err, MediaError::BadDimensions { .. }), "{err:?}");
    // The same URL without the hint is fine, and is a separate entry.
    assert!(media.load(avatar("big.png", 40)).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_only_waiter_cancels_the_fetch() {
    let fetcher = MockFetcher::gated();
    fetcher.serve("a.png", png(&split(20, 20)));
    let media = client(&fetcher, config());

    let task = {
        let media = media.clone();
        tokio::spawn(async move { media.load(avatar("a.png", 40)).await })
    };
    wait_until("the fetch to start", || fetcher.calls() == 1).await;
    assert_eq!(media.stats().in_flight, 1);

    task.abort(); // the UI no longer needs the image
    let _ = task.await;
    wait_until("the fetch to be dropped", || {
        fetcher.dropped.load(Ordering::SeqCst)
    })
    .await;
    assert_eq!(media.stats().in_flight, 0);

    // Asking again later starts over and works. A cancel is not a failure.
    fetcher.open_gate();
    assert!(media.load(avatar("a.png", 40)).await.is_ok());
    assert_eq!(fetcher.calls(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_waiter_leaving_does_not_cancel_the_others() {
    let fetcher = MockFetcher::gated();
    fetcher.serve("a.png", png(&split(20, 20)));
    let media = client(&fetcher, config());

    let first = {
        let media = media.clone();
        tokio::spawn(async move { media.load(avatar("a.png", 40)).await })
    };
    let second = {
        let media = media.clone();
        tokio::spawn(async move { media.load(avatar("a.png", 40)).await })
    };
    wait_until("both callers to join", || media.stats().waiters == 2).await;

    first.abort();
    let _ = first.await;
    // The request goes on for the one that is left. If the fetch had been cancelled,
    // the second caller would fail below.
    wait_until("the first caller to leave", || media.stats().waiters == 1).await;
    assert_eq!(media.stats().in_flight, 1);

    fetcher.open_gate();
    assert!(second.await.unwrap().is_ok());
    assert_eq!(fetcher.calls(), 1);
}

// ---- memory cache and the eviction callback ----------------------------------------

/// Keeps the decoded pixels and records every handle the cache lets go of.
#[derive(Default)]
struct Tracking {
    evicted: Mutex<Vec<Arc<DecodedImage>>>,
}

#[derive(Clone)]
struct TrackingSink(Arc<Tracking>);

impl ImageSink for TrackingSink {
    type Handle = Arc<DecodedImage>;
    fn make(&self, image: DecodedImage) -> (Self::Handle, u64) {
        let bytes = image.byte_size();
        (Arc::new(image), bytes)
    }
    fn evicted(&self, handle: Self::Handle) {
        self.0.evicted.lock().unwrap().push(handle);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_memory_cache_evicts_the_least_recently_used_image_and_says_so() {
    let fetcher = MockFetcher::new();
    for name in ["a", "b", "c"] {
        fetcher.serve(&format!("{name}.png"), png(&split(64, 64)));
    }
    // Each 20x20 image is 1600 bytes; the cache holds two.
    let sink = Arc::new(Tracking::default());
    let media = MediaClient::new(
        tokio::runtime::Handle::current(),
        fetcher.clone(),
        TrackingSink(sink.clone()),
        MediaConfig {
            memory_cache_bytes: 3500,
            ..config()
        },
    );

    media.load(avatar("a.png", 20)).await.unwrap();
    let b = media.load(avatar("b.png", 20)).await.unwrap();
    assert!(media.cached(&avatar("a.png", 20)).is_some()); // a is now newer than b
    media.load(avatar("c.png", 20)).await.unwrap();

    assert!(media.cached(&avatar("a.png", 20)).is_some());
    assert!(
        media.cached(&avatar("b.png", 20)).is_none(),
        "b should be evicted"
    );
    assert!(media.cached(&avatar("c.png", 20)).is_some());
    let stats = media.stats();
    assert_eq!(stats.memory_entries, 2);
    assert!(stats.memory_bytes <= 3500);
    {
        let evicted = sink.evicted.lock().unwrap();
        assert_eq!(evicted.len(), 1);
        assert!(Arc::ptr_eq(&evicted[0], &b));
    }

    media.clear_memory();
    assert_eq!(media.stats().memory_entries, 0);
    assert_eq!(sink.evicted.lock().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_image_larger_than_the_whole_cache_is_returned_and_reported_as_dropped() {
    let fetcher = MockFetcher::new();
    fetcher.serve("a.png", png(&split(64, 64)));
    let sink = Arc::new(Tracking::default());
    let media = MediaClient::new(
        tokio::runtime::Handle::current(),
        fetcher.clone(),
        TrackingSink(sink.clone()),
        MediaConfig {
            memory_cache_bytes: 100,
            ..config()
        },
    );
    let image = media.load(avatar("a.png", 64)).await.unwrap();
    assert_eq!(media.stats().memory_entries, 0);
    let evicted = sink.evicted.lock().unwrap();
    assert_eq!(evicted.len(), 1);
    assert!(Arc::ptr_eq(&evicted[0], &image));
}

// ---- failures --------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_images_give_errors_and_are_not_retried_at_once() {
    let fetcher = MockFetcher::new();
    fetcher.serve("bad.png", b"this is not an image".to_vec());
    let media = client(&fetcher, config());

    let err = media.load(avatar("bad.png", 40)).await.unwrap_err();
    assert_eq!(err, MediaError::Unsupported);
    let err = media.load(avatar("bad.png", 40)).await.unwrap_err();
    assert_eq!(err, MediaError::Unsupported);
    assert_eq!(
        fetcher.calls(),
        1,
        "a permanent failure must not be fetched again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_failure_is_remembered_with_a_growing_back_off() {
    // Time only moves when the test moves it.
    let clock = ManualClock::new();
    let fetcher = MockFetcher::new();
    let media = client(
        &fetcher,
        MediaConfig {
            permanent_failure_backoff: Duration::from_secs(200),
            failure_backoff: Duration::from_secs(100),
            failure_backoff_max: Duration::from_secs(500),
            clock: clock.clone(),
            ..config()
        },
    );
    let secs = Duration::from_secs;

    // 404 is permanent, but the back-off ends: 200 s, then 400 s, then 500 s.
    let err = media.load(avatar("missing.png", 40)).await.unwrap_err();
    assert_eq!(err, MediaError::Fetch(FetchError::Status(404)));
    assert!(err.is_permanent());
    for _ in 0..5 {
        assert_eq!(
            media.load(avatar("missing.png", 40)).await.unwrap_err(),
            err
        );
    }
    assert_eq!(
        fetcher.calls(),
        1,
        "a dead URL must not be fetched on every paint"
    );
    assert_eq!(media.stats().failures, 1);
    clock.advance(secs(199));
    let _ = media.load(avatar("missing.png", 40)).await;
    assert_eq!(fetcher.calls(), 1, "still inside the first back-off");
    clock.advance(secs(1));
    let _ = media.load(avatar("missing.png", 40)).await;
    assert_eq!(fetcher.calls(), 2, "the first back-off ended");
    // The second failure doubled the wait to 400 s.
    clock.advance(secs(399));
    let _ = media.load(avatar("missing.png", 40)).await;
    assert_eq!(fetcher.calls(), 2);
    clock.advance(secs(1));
    let _ = media.load(avatar("missing.png", 40)).await;
    assert_eq!(fetcher.calls(), 3);
    // The third would be 800 s, but 500 s is the most.
    clock.advance(secs(499));
    let _ = media.load(avatar("missing.png", 40)).await;
    assert_eq!(fetcher.calls(), 3);
    clock.advance(secs(1));
    let _ = media.load(avatar("missing.png", 40)).await;
    assert_eq!(fetcher.calls(), 4);

    // A server error is transient but remembered the same way, with the shorter wait.
    fetcher.serve_status("down.png", 503);
    let err = media.load(avatar("down.png", 40)).await.unwrap_err();
    assert_eq!(err, MediaError::Fetch(FetchError::Status(503)));
    assert!(!err.is_permanent());
    assert_eq!(fetcher.calls(), 5);
    clock.advance(secs(99));
    let _ = media.load(avatar("down.png", 40)).await;
    assert_eq!(fetcher.calls(), 5);
    clock.advance(secs(1));
    let _ = media.load(avatar("down.png", 40)).await;
    assert_eq!(fetcher.calls(), 6);

    // forget_failure ends the wait at once, and a success clears the record.
    media.forget_failure(&avatar("down.png", 40));
    fetcher.serve("down.png", png(&solid(8, 8, RED)));
    fetcher.statuses.lock().unwrap().clear();
    assert!(media.load(avatar("down.png", 40)).await.is_ok());
    assert_eq!(fetcher.calls(), 7);
    assert_eq!(media.stats().failures, 1, "only the dead URL is left");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_fetch_times_out_and_is_remembered() {
    let fetcher = MockFetcher::gated(); // never opened
    let media = client(
        &fetcher,
        MediaConfig {
            fetch_timeout: Duration::from_millis(100),
            ..config()
        },
    );
    let err = media.load(avatar("a.png", 40)).await.unwrap_err();
    assert_eq!(err, MediaError::FetchTimeout);
    assert!(!err.is_permanent());
    assert_eq!(
        media.load(avatar("a.png", 40)).await.unwrap_err(),
        MediaError::FetchTimeout
    );
    assert_eq!(fetcher.calls(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_panicking_fetcher_fails_the_request_instead_of_hanging_it() {
    struct Panics;
    impl inlet_media::Fetcher for Panics {
        fn fetch<'a>(
            &'a self,
            _: &'a url::Url,
        ) -> futures_util::future::BoxFuture<'a, Result<FetchResponse, FetchError>> {
            Box::pin(async { panic!("fetcher bug") })
        }
    }
    let media = MediaClient::new(
        tokio::runtime::Handle::current(),
        Arc::new(Panics),
        RawImages,
        config(),
    );
    let waiters = [
        media.load(avatar("a.png", 40)),
        media.load(avatar("a.png", 40)),
    ];
    for result in futures_util::future::join_all(waiters).await {
        assert_eq!(result.unwrap_err(), MediaError::TaskFailed);
    }
    assert_eq!(media.stats().in_flight, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_panicking_sink_fails_the_request_instead_of_hanging_it() {
    struct Panics;
    impl ImageSink for Panics {
        type Handle = Arc<DecodedImage>;
        fn make(&self, _: DecodedImage) -> (Self::Handle, u64) {
            panic!("sink bug")
        }
    }
    let fetcher = MockFetcher::new();
    fetcher.serve("a.png", png(&solid(8, 8, RED)));
    let media = MediaClient::new(tokio::runtime::Handle::current(), fetcher, Panics, config());
    assert_eq!(
        media.load(avatar("a.png", 40)).await.unwrap_err(),
        MediaError::TaskFailed
    );
}

// ---- URL policy and the fetch contract ------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_https_urls_on_the_allow_list_are_fetched() {
    let fetcher = MockFetcher::new();
    let media = client(&fetcher, config());
    for bad in [
        "file:///etc/passwd",
        "ftp://cdn.example/a.png",
        "data:image/png;base64,AAAA",
        "https://user:pass@cdn.example/a.png",
        "http://cdn.example/a.png",
    ] {
        let req = MediaRequest::new(bad.parse().unwrap(), MediaKind::Avatar, Fit::square(40));
        let err = media.load(req).await.unwrap_err();
        assert!(matches!(err, MediaError::InvalidUrl(_)), "{bad}: {err:?}");
    }
    for elsewhere in [
        "https://evil.example/a.png",
        "https://10.0.0.1/a.png",
        "https://cdn.example.evil.net/a.png",
        "https://192.168.1.1/admin",
    ] {
        let req = MediaRequest::new(
            elsewhere.parse().unwrap(),
            MediaKind::Avatar,
            Fit::square(40),
        );
        let err = media.load(req).await.unwrap_err();
        assert!(
            matches!(err, MediaError::HostNotAllowed(_)),
            "{elsewhere}: {err:?}"
        );
    }
    assert_eq!(fetcher.calls(), 0);

    // With an empty list nothing is allowed.
    let strict = client(&fetcher, MediaConfig::default());
    assert!(matches!(
        strict.load(avatar("a.png", 40)).await.unwrap_err(),
        MediaError::HostNotAllowed(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wildcard_hosts_match_subdomains_only() {
    let fetcher = MockFetcher::new();
    let media = client(
        &fetcher,
        MediaConfig {
            allowed_hosts: vec!["*.cdn.example".into()],
            ..MediaConfig::default()
        },
    );
    let req = |u: &str| MediaRequest::new(u.parse().unwrap(), MediaKind::Avatar, Fit::square(40));
    assert!(matches!(
        media
            .load(req("https://cdn.example/a.png"))
            .await
            .unwrap_err(),
        MediaError::HostNotAllowed(_)
    ));
    // Allowed by the list, then a 404 from the mock: proves the fetcher was called.
    assert_eq!(
        media
            .load(req("https://eu.cdn.example/a.png"))
            .await
            .unwrap_err(),
        MediaError::Fetch(FetchError::Status(404))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_that_never_ends_is_cut_at_the_limit_and_the_stream_is_dropped() {
    let fetcher = MockFetcher::new();
    fetcher.serve_endless("loop.png");
    let media = client(&fetcher, config());
    let err = media.load(avatar("loop.png", 40)).await.unwrap_err();
    assert!(
        matches!(err, MediaError::TooLargeEncoded { max, .. } if max == 2 * 1024 * 1024),
        "{err:?}"
    );
    let sent = fetcher.sent.load(Ordering::SeqCst);
    assert!(
        sent <= 2 * 1024 * 1024 + 8192,
        "{sent} bytes were read for a 2 MiB limit"
    );
    assert!(
        fetcher.stream_dropped.load(Ordering::SeqCst),
        "the download must be cancelled"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_announced_length_over_the_limit_is_refused_without_reading() {
    let fetcher = MockFetcher::new();
    fetcher.serve("big.bin", vec![0u8; 3 * 1024 * 1024]);
    let media = client(&fetcher, config());
    let err = media.load(avatar("big.bin", 40)).await.unwrap_err();
    assert!(matches!(err, MediaError::TooLargeEncoded { .. }), "{err:?}");
    assert_eq!(fetcher.sent.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_encoded_limit_depends_on_the_kind() {
    let fetcher = MockFetcher::new();
    // A valid PNG padded with a 700 KiB text chunk: over the emoji limit, under the avatar limit.
    let mut big = png(&solid(8, 8, RED));
    big.truncate(big.len() - 12); // drop IEND
    png_chunk(&mut big, b"tEXt", &vec![b'a'; 700 * 1024]);
    png_chunk(&mut big, b"IEND", &[]);
    fetcher.serve("big.png", big);
    let media = client(&fetcher, config());

    let emoji = MediaRequest::new(url("big.png"), MediaKind::Emoji, Fit::square(48));
    let err = media.load(emoji).await.unwrap_err();
    assert!(
        matches!(err, MediaError::TooLargeEncoded { max, .. } if max == 512 * 1024),
        "{err:?}"
    );
    assert!(media.load(avatar("big.png", 40)).await.is_ok());
}

// ---- decode scheduling ------------------------------------------------------------------

/// Blocks inside `make` of the first image until it is released. `make` runs
/// while the decode permits are held, so this parks a decode.
struct Gate {
    made: AtomicUsize,
    open: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            made: AtomicUsize::new(0),
            open: Mutex::new(false),
            cv: Condvar::new(),
        })
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

#[derive(Clone)]
struct GateSink(Arc<Gate>);

impl ImageSink for GateSink {
    type Handle = Arc<DecodedImage>;
    fn make(&self, image: DecodedImage) -> (Self::Handle, u64) {
        let gate = &self.0;
        if gate.made.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut open = gate.open.lock().unwrap();
            while !*open {
                open = gate.cv.wait(open).unwrap();
            }
        }
        let bytes = image.byte_size();
        (Arc::new(image), bytes)
    }
}

fn gated_client(
    fetcher: &Arc<MockFetcher>,
    gate: &Arc<Gate>,
    config: MediaConfig,
) -> MediaClient<GateSink> {
    MediaClient::new(
        tokio::runtime::Handle::current(),
        fetcher.clone(),
        GateSink(gate.clone()),
        config,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn avatars_and_emoji_still_decode_while_big_attachments_hold_every_general_slot() {
    let fetcher = MockFetcher::new();
    for name in ["a1", "a2", "x"] {
        fetcher.serve(&format!("{name}.png"), png(&solid(300, 300, RED)));
    }
    fetcher.serve("emoji.png", png(&solid(64, 64, RED)));
    let gate = Gate::new();
    let media = gated_client(
        &fetcher,
        &gate,
        MediaConfig {
            decode_concurrency: 1,
            ..config()
        },
    );

    // The first attachment parks inside the only general slot.
    let first = tokio::spawn({
        let media = media.clone();
        async move { media.load(attachment("a1.png")).await }
    });
    wait_until("the first decode to park", || {
        gate.made.load(Ordering::SeqCst) == 1
    })
    .await;

    // A second attachment has to wait for it.
    let second = tokio::spawn({
        let media = media.clone();
        async move { media.load(attachment("a2.png")).await }
    });
    wait_until("the second attachment to queue", || {
        media.stats().waiting_to_decode == 1
    })
    .await;
    // An avatar and an emoji use the reserved slot and finish.
    let small = tokio::time::timeout(Duration::from_secs(10), async {
        let a = media.load(avatar("x.png", 40)).await;
        let e = media
            .load(MediaRequest::new(
                url("emoji.png"),
                MediaKind::Emoji,
                Fit::square(32),
            ))
            .await;
        (a, e)
    })
    .await
    .expect("small images were starved");
    assert!(small.0.is_ok() && small.1.is_ok());
    assert!(!second.is_finished(), "the second attachment must wait");

    gate.release();
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decodes_wait_for_the_memory_budget_not_only_for_a_slot() {
    let fetcher = MockFetcher::new();
    for name in ["a", "b"] {
        fetcher.serve(&format!("{name}.png"), png(&solid(2000, 2000, RED)));
    }
    let gate = Gate::new();
    // Plenty of slots, but a budget for one big decode at a time (a 2000x2000
    // image is estimated at 48 MiB and more; the general lane has 48 MiB).
    let media = gated_client(
        &fetcher,
        &gate,
        MediaConfig {
            decode_concurrency: 4,
            decode_memory_bytes: 64 * 1024 * 1024,
            ..config()
        },
    );
    let first = tokio::spawn({
        let media = media.clone();
        async move { media.load(attachment("a.png")).await }
    });
    wait_until("the first decode to park", || {
        gate.made.load(Ordering::SeqCst) == 1
    })
    .await;
    let second = tokio::spawn({
        let media = media.clone();
        async move { media.load(attachment("b.png")).await }
    });
    // The second one has its image and waits for memory; the first still holds it all.
    wait_until("the second decode to queue", || {
        media.stats().waiting_to_decode == 1
    })
    .await;
    assert_eq!(
        gate.made.load(Ordering::SeqCst),
        1,
        "the second decode started although the memory budget was used up"
    );
    assert_eq!(media.stats().decodes, 1);

    gate.release();
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
    assert_eq!(media.stats().decodes, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decoding_is_capped_at_the_configured_concurrency() {
    // One decode slot: all requests still finish, one after the other.
    let fetcher = MockFetcher::new();
    for i in 0..6 {
        fetcher.serve(&format!("{i}.png"), png(&split(300, 300)));
    }
    let media = client(
        &fetcher,
        MediaConfig {
            decode_concurrency: 1,
            ..config()
        },
    );
    let loads: Vec<_> = (0..6)
        .map(|i| media.load(attachment(&format!("{i}.png"))))
        .collect();
    for result in futures_util::future::join_all(loads).await {
        assert!(result.is_ok());
    }
    assert_eq!(media.stats().decodes, 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn animated_images_load_with_all_frames() {
    let fetcher = MockFetcher::new();
    fetcher.serve(
        "a.gif",
        gif(&[(solid(64, 64, RED), 50), (solid(64, 64, GREEN), 50)]),
    );
    let media = client(&fetcher, config());
    let image = media.load(avatar("a.gif", 32)).await.unwrap();
    assert!(image.is_animated());
    assert_eq!(image.frames.len(), 2);
    assert_eq!((image.width, image.height), (32, 32));
}

// ---- disk cache --------------------------------------------------------------------

fn with_disk(dir: &std::path::Path, max_bytes: u64) -> MediaConfig {
    MediaConfig {
        disk_cache: Some(DiskCacheConfig {
            dir: dir.to_owned(),
            max_bytes,
        }),
        ..config()
    }
}

fn files(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_disk_cache_serves_a_new_client_without_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = MockFetcher::new();
    fetcher.serve("a.png", png(&split(100, 100)));

    let first = client(&fetcher, with_disk(dir.path(), 1 << 20));
    first.load(avatar("a.png", 40)).await.unwrap();
    assert_eq!(fetcher.calls(), 1);

    let names = files(dir.path());
    assert_eq!(names.len(), 1);
    assert!(names[0].ends_with(".bin"));
    assert_eq!(names[0].len(), 64 + 4);
    assert!(names[0][..64].bytes().all(|b| b.is_ascii_hexdigit()));

    // A fresh client (a restart) and another size of the same image.
    let second = client(&fetcher, with_disk(dir.path(), 1 << 20));
    let image = second.load(avatar("a.png", 96)).await.unwrap();
    assert_eq!(image.width, 96);
    assert_eq!(fetcher.calls(), 1, "no second download");
    assert_eq!(second.stats().disk_hits, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn data_that_does_not_decode_is_not_written_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = MockFetcher::new();
    fetcher.serve("bad.png", b"not an image".to_vec());
    let media = client(&fetcher, with_disk(dir.path(), 1 << 20));
    assert!(media.load(avatar("bad.png", 40)).await.is_err());
    assert!(files(dir.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_disk_cache_stays_within_its_cap() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = MockFetcher::new();
    let mut size = 0;
    for i in 0..8 {
        let bytes = png(&RgbaImage::from_fn(60, 60, |x, y| {
            image::Rgba([(x * 4) as u8, (y * 4) as u8, i * 30, 255])
        }));
        size = bytes.len() as u64;
        fetcher.serve(&format!("{i}.png"), bytes);
    }
    let media = client(&fetcher, with_disk(dir.path(), size * 3 + size / 2));
    for i in 0..8 {
        media.load(avatar(&format!("{i}.png"), 20)).await.unwrap();
    }
    let total: u64 = files(dir.path())
        .iter()
        .map(|n| std::fs::metadata(dir.path().join(n)).unwrap().len())
        .sum();
    assert!(total <= size * 3 + size / 2, "{total} bytes on disk");
    assert!(files(dir.path()).len() >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_damaged_disk_file_is_deleted_and_fetched_again_once() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = MockFetcher::new();
    let good = png(&split(50, 50));
    fetcher.serve("a.png", good.clone());
    let first = client(&fetcher, with_disk(dir.path(), 1 << 20));
    first.load(avatar("a.png", 40)).await.unwrap();
    let file = dir.path().join(&files(dir.path())[0]);

    // An empty file (power cut before the data reached the disk), garbage, and a
    // file cut off in the middle.
    for damage in [
        Vec::new(),
        b"garbage".to_vec(),
        good[..good.len() / 2].to_vec(),
    ] {
        std::fs::write(&file, damage).unwrap();
        let before = fetcher.calls();
        let second = client(&fetcher, with_disk(dir.path(), 1 << 20));
        let image = second.load(avatar("a.png", 40)).await.unwrap();
        assert_eq!(image.width, 40);
        assert_eq!(fetcher.calls(), before + 1, "one fetch repairs it");
        assert_eq!(
            std::fs::read(&file).unwrap(),
            good,
            "the file was rewritten"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kind_with_a_lower_limit_does_not_delete_a_file_that_is_too_big_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = MockFetcher::new();
    let mut big = png(&solid(8, 8, RED));
    big.truncate(big.len() - 12);
    png_chunk(&mut big, b"tEXt", &vec![b'a'; 700 * 1024]);
    png_chunk(&mut big, b"IEND", &[]);
    fetcher.serve("big.png", big.clone());

    let media = client(&fetcher, with_disk(dir.path(), 4 << 20));
    assert!(media.load(attachment("big.png")).await.is_ok());
    let file = dir.path().join(&files(dir.path())[0]);

    let emoji = MediaRequest::new(url("big.png"), MediaKind::Emoji, Fit::square(32));
    let err = media.load(emoji).await.unwrap_err();
    assert!(matches!(err, MediaError::TooLargeEncoded { .. }), "{err:?}");
    assert_eq!(std::fs::read(&file).unwrap(), big, "the file must stay");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_that_only_breaks_a_limit_of_one_request_is_kept_and_not_fetched_again() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = MockFetcher::new();
    let big = png(&solid(1000, 1000, RED));
    fetcher.serve("big.png", big.clone());

    let media = client(&fetcher, with_disk(dir.path(), 4 << 20));
    assert!(media.load(attachment("big.png")).await.is_ok());
    assert_eq!(fetcher.calls(), 1);
    let file = dir.path().join(&files(dir.path())[0]);

    // The same URL for a request that asked the proxy for 40 pixels: 1000 pixels
    // is over its limit. The file is good; the network would say the same.
    let small = avatar("big.png", 40).with_proxy_size(40);
    let err = media.load(small).await.unwrap_err();
    assert!(matches!(err, MediaError::BadDimensions { .. }), "{err:?}");
    assert_eq!(fetcher.calls(), 1, "no download for a limit error");
    assert_eq!(std::fs::read(&file).unwrap(), big, "the file must stay");
}
