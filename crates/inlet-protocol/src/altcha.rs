//! ALTCHA v2 proof-of-work solver for the sign-in CAPTCHA.
//!
//! Fluxer answers a gated request without `X-Captcha-Token` with 400
//! `CAPTCHA_REQUIRED` and an `altcha_challenge` (`parameters` + `signature`).
//! The work: find a counter so that
//! `PBKDF2(hash, password = nonce || u32_be(counter), salt, cost, keyLength)`
//! starts with `keyPrefix`. The token is base64 of
//! `{"challenge": {"parameters", "signature"}, "solution": {"counter", "derivedKey"}}`
//! with `parameters` and `signature` copied unchanged.
//!
//! This follows altcha-lib 2.x (`solveChallenge`, counter mode `uint32`),
//! which the official client uses. It is tested against challenges built the
//! same way, not against a live instance.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};

use crate::error::{Error, Result};

/// Total PBKDF2 iterations (counters tried x cost) one solve may spend.
/// About a minute on 8 cores for PBKDF2/SHA-256.
const WORK_BUDGET: u64 = 2_000_000_000;

fn derive(algorithm: &str, password: &[u8], salt: &[u8], cost: u32, out: &mut [u8]) -> Result<()> {
    match algorithm {
        "PBKDF2/SHA-256" => pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password, salt, cost, out),
        "PBKDF2/SHA-384" => pbkdf2::pbkdf2_hmac::<sha2::Sha384>(password, salt, cost, out),
        "PBKDF2/SHA-512" => pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password, salt, cost, out),
        other => return Err(Error::Captcha(format!("unsupported algorithm {other}"))),
    }
    Ok(())
}

struct Params {
    algorithm: String,
    nonce: Vec<u8>,
    salt: Vec<u8>,
    cost: u32,
    key_length: usize,
    prefix_hex: String,
}

fn parse(parameters: &Value) -> Result<Params> {
    let s = |k: &str| {
        parameters
            .get(k)
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Captcha(format!("challenge has no {k}")))
    };
    let hexd = |k: &str| -> Result<Vec<u8>> {
        hex::decode(s(k)?).map_err(|_| Error::Captcha(format!("{k} is not hex")))
    };
    let cost = parameters
        .get("cost")
        .and_then(Value::as_u64)
        .filter(|c| *c >= 1 && *c <= 10_000_000)
        .ok_or_else(|| Error::Captcha("bad cost".into()))? as u32;
    let key_length = parameters
        .get("keyLength")
        .and_then(Value::as_u64)
        .unwrap_or(32) as usize;
    if !(1..=128).contains(&key_length) {
        return Err(Error::Captcha("bad keyLength".into()));
    }
    let prefix_hex = s("keyPrefix")?.to_ascii_lowercase();
    if prefix_hex.is_empty()
        || prefix_hex.len() > key_length * 2
        || !prefix_hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(Error::Captcha("bad keyPrefix".into()));
    }
    Ok(Params {
        algorithm: s("algorithm")?.to_owned(),
        nonce: hexd("nonce")?,
        salt: hexd("salt")?,
        cost,
        key_length,
        prefix_hex,
    })
}

fn try_counter(p: &Params, counter: u32, buf: &mut Vec<u8>, key: &mut [u8]) -> Result<bool> {
    buf.truncate(p.nonce.len());
    buf.extend_from_slice(&counter.to_be_bytes());
    derive(&p.algorithm, buf, &p.salt, p.cost, key)?;
    Ok(hex::encode(&key[..p.prefix_hex.len().div_ceil(2)]).starts_with(&p.prefix_hex))
}

/// Solves `challenge` (the `altcha_challenge` object) on all cores and returns
/// the `X-Captcha-Token` value. Blocking: run it with `spawn_blocking`.
pub fn solve_blocking(challenge: &Value, timeout: Duration) -> Result<String> {
    solve_cancellable(challenge, timeout, &AtomicBool::new(false))
}

fn solve_cancellable(challenge: &Value, timeout: Duration, cancel: &AtomicBool) -> Result<String> {
    let parameters = challenge
        .get("parameters")
        .ok_or_else(|| Error::Captcha("challenge has no parameters".into()))?;
    let signature = challenge.get("signature").cloned().unwrap_or(Value::Null);
    let params = Arc::new(parse(parameters)?);
    if let Some(expires) = parameters.get("expiresAt").and_then(Value::as_f64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        if expires < now {
            return Err(Error::Captcha("the challenge has expired".into()));
        }
    }
    // Bound the work: never try more counters than the budget allows for this cost.
    let max_counter = (WORK_BUDGET / u64::from(params.cost)).min(u64::from(u32::MAX));
    // Fail fast on an unsupported algorithm.
    derive(&params.algorithm, b"", b"", 1, &mut [0u8; 1])?;

    let workers = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .clamp(1, 8);
    let stop = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    let found = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for w in 0..workers {
            let params = params.clone();
            let stop = stop.clone();
            handles.push(scope.spawn(move || -> Option<(u32, Vec<u8>)> {
                let mut buf = params.nonce.clone();
                let mut key = vec![0u8; params.key_length];
                let mut counter = w as u64;
                while counter <= max_counter {
                    if stop.load(Ordering::Relaxed)
                        || cancel.load(Ordering::Relaxed)
                        || start.elapsed() > timeout
                    {
                        return None;
                    }
                    if try_counter(&params, counter as u32, &mut buf, &mut key).ok()? {
                        stop.store(true, Ordering::Relaxed);
                        return Some((counter as u32, key));
                    }
                    counter += workers as u64;
                }
                None
            }));
        }
        handles
            .into_iter()
            .filter_map(|h| h.join().ok().flatten())
            .min_by_key(|(c, _)| *c)
    });
    let (counter, key) = found
        .ok_or_else(|| Error::Captcha("no solution within the time and work limits".into()))?;
    let token = json!({
        "challenge": { "parameters": parameters, "signature": signature },
        "solution": {
            "counter": counter,
            "derivedKey": hex::encode(key),
            "time": start.elapsed().as_millis() as u64,
        }
    });
    Ok(base64::engine::general_purpose::STANDARD.encode(token.to_string()))
}

/// One solve at a time in the whole process (sign-in and group DMs share
/// it), so many clicks or accounts cannot take every core.
fn solver_slot() -> Arc<tokio::sync::Semaphore> {
    static SLOT: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
}

/// Async wrapper around [`solve_blocking`], with a hard deadline.
///
/// - Only one solve runs at a time in the process; waiting for it counts
///   against `timeout`.
/// - The answer (or an error) comes back within `timeout`, whatever happens.
///   The workers stop at their next attempt after the deadline or when this
///   future is dropped, and the slot is only freed once they have stopped
///   (one attempt at the highest accepted cost can take a few seconds).
pub async fn solve(challenge: Value, timeout: Duration) -> Result<String> {
    let started = tokio::time::Instant::now();
    let permit = tokio::time::timeout(timeout, solver_slot().acquire_owned())
        .await
        .map_err(|_| Error::Captcha("another CAPTCHA is being solved".into()))?
        .map_err(|_| Error::Captcha("solver closed".into()))?;
    let left = timeout.saturating_sub(started.elapsed());
    tokio::time::timeout(left, solve_holding(challenge, left, permit))
        .await
        .unwrap_or_else(|_| Err(Error::Captcha("CAPTCHA timed out".into())))
}

/// Like [`solve`], and keeps `guard` alive until the blocking work has really
/// stopped (also when this future is dropped early), for example a permit
/// that limits how many solves run at once.
pub(crate) async fn solve_holding<G: Send + 'static>(
    challenge: Value,
    timeout: Duration,
    guard: G,
) -> Result<String> {
    struct CancelOnDrop(Arc<AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(cancel.clone());
    tokio::task::spawn_blocking(move || {
        let result = solve_cancellable(&challenge, timeout, &cancel);
        drop(guard);
        result
    })
    .await
    .map_err(|e| Error::Captcha(e.to_string()))?
}

/// Reads the challenge from a 400 `CAPTCHA_REQUIRED`/`INVALID_CAPTCHA` body.
pub(crate) fn challenge_from_error(body: &Value) -> Option<Value> {
    let code = body.get("code")?.as_str()?;
    if code != "CAPTCHA_REQUIRED" && code != "INVALID_CAPTCHA" {
        return None;
    }
    let challenge = body.get("altcha_challenge")?;
    challenge.get("parameters")?.as_object()?;
    Some(challenge.clone())
}

/// Builds a challenge whose answer is `counter`, the way an ALTCHA server
/// does in deterministic mode. For tests and the mock server.
#[doc(hidden)]
pub fn make_test_challenge(counter: u32, cost: u32) -> Value {
    let nonce: [u8; 16] = rand::random();
    let salt: [u8; 16] = rand::random();
    let mut password = nonce.to_vec();
    password.extend_from_slice(&counter.to_be_bytes());
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(&password, &salt, cost, &mut key);
    json!({
        "parameters": {
            "algorithm": "PBKDF2/SHA-256",
            "cost": cost,
            "keyLength": 32,
            "keyPrefix": hex::encode(&key[..3]),
            "nonce": hex::encode(nonce),
            "salt": hex::encode(salt),
            "expiresAt": 4_102_444_800u64,
        },
        "signature": "test-signature"
    })
}

/// Checks a token against its challenge, as the server does (without the HMAC).
#[doc(hidden)]
pub fn verify_test_token(token: &str) -> bool {
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(token) else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    let Ok(p) = parse(&v["challenge"]["parameters"]) else {
        return false;
    };
    let Some(counter) = v["solution"]["counter"].as_u64() else {
        return false;
    };
    let mut buf = p.nonce.clone();
    let mut key = vec![0u8; p.key_length];
    try_counter(&p, counter as u32, &mut buf, &mut key).unwrap_or(false)
        && v["solution"]["derivedKey"].as_str() == Some(hex::encode(&key).as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solves_and_verifies() {
        let challenge = make_test_challenge(1234, 50);
        let token = solve_blocking(&challenge, Duration::from_secs(60)).unwrap();
        assert!(verify_test_token(&token));
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&token)
            .unwrap();
        let v: Value = serde_json::from_slice(&raw).unwrap();
        // Parameters and signature are copied unchanged.
        assert_eq!(v["challenge"], challenge);
        assert!(v["solution"]["counter"].as_u64().unwrap() <= 1234);
    }

    #[test]
    fn rejects_expired_and_respects_work_budget() {
        let mut c = make_test_challenge(5, 10);
        c["parameters"]["expiresAt"] = json!(1_000);
        assert!(solve_blocking(&c, Duration::from_secs(5)).is_err());
        // A huge cost leaves room for only a few counters; the answer at 5000 is out of reach.
        let mut c = make_test_challenge(5_000, 1);
        c["parameters"]["cost"] = json!(1_000_000_000u64);
        assert!(parse(&c["parameters"]).is_err());
    }

    #[tokio::test]
    async fn dropping_the_future_cancels_the_work() {
        // An unsolvable challenge (prefix longer than any match) runs until cancelled.
        let mut c = make_test_challenge(1, 1);
        c["parameters"]["keyPrefix"] = json!("0123456789abcdef0123456789abcdef");
        let fut = solve(c, Duration::from_secs(600));
        let r = tokio::time::timeout(Duration::from_millis(200), fut).await;
        assert!(r.is_err(), "still running when dropped");
    }

    #[test]
    fn rejects_unknown_algorithm() {
        let mut c = make_test_challenge(1, 1);
        c["parameters"]["algorithm"] = json!("ARGON2ID");
        assert!(solve_blocking(&c, Duration::from_secs(1)).is_err());
    }
}
