mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use inlet_media::{
    DiskCacheConfig, FetchError, Fit, MediaClient, MediaConfig, MediaError, MediaKind, MediaRequest,
};

fn client(fetcher: &Arc<MockFetcher>, config: MediaConfig) -> MediaClient {
    MediaClient::new(tokio::runtime::Handle::current(), fetcher.clone(), config)
}

fn avatar(path: &str, size: u32) -> MediaRequest {
    MediaRequest::new(url(path), MediaKind::Avatar, Fit::square(size))
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
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
    let media = client(&fetcher, MediaConfig::default());

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
    let media = client(&fetcher, MediaConfig::default());

    let tasks: Vec<_> = (0..25)
        .map(|_| {
            let media = media.clone();
            tokio::spawn(async move { media.load(avatar("a.png", 44)).await })
        })
        .collect();
    wait_until("the fetch to start", || fetcher.calls() >= 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
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
    let media = client(&fetcher, MediaConfig::default());
    let (small, big) = tokio::join!(
        media.load(avatar("a.png", 40)),
        media.load(avatar("a.png", 96))
    );
    assert_eq!(small.unwrap().width, 40);
    assert_eq!(big.unwrap().width, 96);
    assert_eq!(media.stats().memory_entries, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_only_waiter_cancels_the_fetch() {
    let fetcher = MockFetcher::gated();
    fetcher.serve("a.png", png(&split(20, 20)));
    let media = client(&fetcher, MediaConfig::default());

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

    // Asking again later starts over and works.
    fetcher.open_gate();
    assert!(media.load(avatar("a.png", 40)).await.is_ok());
    assert_eq!(fetcher.calls(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_waiter_leaving_does_not_cancel_the_others() {
    let fetcher = MockFetcher::gated();
    fetcher.serve("a.png", png(&split(20, 20)));
    let media = client(&fetcher, MediaConfig::default());

    let first = {
        let media = media.clone();
        tokio::spawn(async move { media.load(avatar("a.png", 40)).await })
    };
    let second = {
        let media = media.clone();
        tokio::spawn(async move { media.load(avatar("a.png", 40)).await })
    };
    wait_until("the fetch to start", || fetcher.calls() == 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    first.abort();
    let _ = first.await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !fetcher.dropped.load(Ordering::SeqCst),
        "the fetch was cancelled too early"
    );

    fetcher.open_gate();
    assert!(second.await.unwrap().is_ok());
    assert_eq!(fetcher.calls(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_memory_cache_evicts_the_least_recently_used_image() {
    let fetcher = MockFetcher::new();
    for name in ["a", "b", "c"] {
        fetcher.serve(&format!("{name}.png"), png(&split(64, 64)));
    }
    // Each 20x20 image is 1600 bytes; the cache holds two.
    let config = MediaConfig {
        memory_cache_bytes: 3500,
        ..MediaConfig::default()
    };
    let media = client(&fetcher, config);

    media.load(avatar("a.png", 20)).await.unwrap();
    media.load(avatar("b.png", 20)).await.unwrap();
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

    media.clear_memory();
    assert_eq!(media.stats().memory_entries, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_image_larger_than_the_whole_cache_is_returned_but_not_kept() {
    let fetcher = MockFetcher::new();
    fetcher.serve("a.png", png(&split(64, 64)));
    let config = MediaConfig {
        memory_cache_bytes: 100,
        ..MediaConfig::default()
    };
    let media = client(&fetcher, config);
    assert!(media.load(avatar("a.png", 64)).await.is_ok());
    assert_eq!(media.stats().memory_entries, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_images_give_errors_and_are_not_retried_at_once() {
    let fetcher = MockFetcher::new();
    fetcher.serve("bad.png", b"this is not an image".to_vec());
    let media = client(&fetcher, MediaConfig::default());

    let err = media.load(avatar("bad.png", 40)).await.unwrap_err();
    assert_eq!(err, MediaError::Unsupported);
    let err = media.load(avatar("bad.png", 40)).await.unwrap_err();
    assert_eq!(err, MediaError::Unsupported);
    assert_eq!(
        fetcher.calls(),
        1,
        "a permanent failure must not be fetched again"
    );

    // A missing file is a fetch error and may be retried.
    let err = media.load(avatar("missing.png", 40)).await.unwrap_err();
    assert_eq!(err, MediaError::Fetch(FetchError::Status(404)));
    let _ = media.load(avatar("missing.png", 40)).await;
    assert_eq!(fetcher.calls(), 3);
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
    let media = client(&fetcher, MediaConfig::default());

    let emoji = MediaRequest::new(url("big.png"), MediaKind::Emoji, Fit::square(48));
    let err = media.load(emoji).await.unwrap_err();
    assert!(
        matches!(err, MediaError::TooLargeEncoded { max, .. } if max == 512 * 1024),
        "{err:?}"
    );
    assert!(media.load(avatar("big.png", 40)).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fetcher_that_ignores_the_limit_is_caught() {
    struct Liar;
    impl inlet_media::Fetcher for Liar {
        fn fetch<'a>(
            &'a self,
            _: &'a url::Url,
            _: usize,
        ) -> futures_util::future::BoxFuture<'a, Result<bytes::Bytes, FetchError>> {
            Box::pin(async { Ok(bytes::Bytes::from(vec![0u8; 3 * 1024 * 1024])) })
        }
    }
    let media = MediaClient::new(
        tokio::runtime::Handle::current(),
        Arc::new(Liar),
        MediaConfig::default(),
    );
    let err = media.load(avatar("a.png", 40)).await.unwrap_err();
    assert!(matches!(err, MediaError::TooLargeEncoded { .. }), "{err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_fetch_times_out() {
    let fetcher = MockFetcher::gated(); // never opened
    let config = MediaConfig {
        fetch_timeout: Duration::from_millis(100),
        ..MediaConfig::default()
    };
    let media = client(&fetcher, config);
    let err = media.load(avatar("a.png", 40)).await.unwrap_err();
    assert_eq!(err, MediaError::FetchTimeout);
    assert!(!err.is_permanent());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_plain_http_urls_are_accepted() {
    let fetcher = MockFetcher::new();
    let media = client(&fetcher, MediaConfig::default());
    for bad in [
        "file:///etc/passwd",
        "ftp://cdn.example/a.png",
        "data:image/png;base64,AAAA",
        "https://user:pass@cdn.example/a.png",
    ] {
        let req = MediaRequest::new(bad.parse().unwrap(), MediaKind::Avatar, Fit::square(40));
        let err = media.load(req).await.unwrap_err();
        assert!(matches!(err, MediaError::InvalidUrl(_)), "{bad}: {err:?}");
    }
    assert_eq!(fetcher.calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decoding_is_capped_at_the_configured_concurrency() {
    // One decode slot: all requests still finish, one after the other.
    let fetcher = MockFetcher::new();
    for i in 0..6 {
        fetcher.serve(&format!("{i}.png"), png(&split(300, 300)));
    }
    let config = MediaConfig {
        decode_concurrency: 1,
        ..MediaConfig::default()
    };
    let media = client(&fetcher, config);
    let loads: Vec<_> = (0..6)
        .map(|i| media.load(avatar(&format!("{i}.png"), 40)))
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
    let media = client(&fetcher, MediaConfig::default());
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
        ..MediaConfig::default()
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
async fn a_tampered_disk_file_is_treated_like_any_download() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = MockFetcher::new();
    fetcher.serve("a.png", png(&split(50, 50)));
    let first = client(&fetcher, with_disk(dir.path(), 1 << 20));
    first.load(avatar("a.png", 40)).await.unwrap();
    let file = dir.path().join(&files(dir.path())[0]);
    std::fs::write(&file, gif_claiming(65535, 65535)).unwrap();

    let second = client(&fetcher, with_disk(dir.path(), 1 << 20));
    let err = second.load(avatar("a.png", 40)).await.unwrap_err();
    assert!(matches!(err, MediaError::BadDimensions { .. }), "{err:?}");
}

use image::RgbaImage;
