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

/// Highest counter tried before giving up.
const MAX_COUNTER: u64 = u32::MAX as u64;

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
    let hexd = |k: &str| -> Result<Vec<u8>> { hex::decode(s(k)?).map_err(|_| Error::Captcha(format!("{k} is not hex"))) };
    let cost = parameters
        .get("cost")
        .and_then(Value::as_u64)
        .filter(|c| *c >= 1 && *c <= 10_000_000)
        .ok_or_else(|| Error::Captcha("bad cost".into()))? as u32;
    let key_length = parameters.get("keyLength").and_then(Value::as_u64).unwrap_or(32) as usize;
    if !(1..=128).contains(&key_length) {
        return Err(Error::Captcha("bad keyLength".into()));
    }
    let prefix_hex = s("keyPrefix")?.to_ascii_lowercase();
    if prefix_hex.is_empty() || prefix_hex.len() > key_length * 2 || !prefix_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
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
    let parameters = challenge
        .get("parameters")
        .ok_or_else(|| Error::Captcha("challenge has no parameters".into()))?;
    let signature = challenge.get("signature").cloned().unwrap_or(Value::Null);
    let params = Arc::new(parse(parameters)?);
    // Fail fast on an unsupported algorithm.
    derive(&params.algorithm, b"", b"", 1, &mut [0u8; 1])?;

    let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 8);
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
                while counter <= MAX_COUNTER {
                    if stop.load(Ordering::Relaxed) || start.elapsed() > timeout {
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
    let (counter, key) = found.ok_or_else(|| Error::Captcha("no solution before the timeout".into()))?;
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

/// Async wrapper around [`solve_blocking`].
pub async fn solve(challenge: Value, timeout: Duration) -> Result<String> {
    tokio::task::spawn_blocking(move || solve_blocking(&challenge, timeout))
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
        let raw = base64::engine::general_purpose::STANDARD.decode(&token).unwrap();
        let v: Value = serde_json::from_slice(&raw).unwrap();
        // Parameters and signature are copied unchanged.
        assert_eq!(v["challenge"], challenge);
        assert!(v["solution"]["counter"].as_u64().unwrap() <= 1234);
    }

    #[test]
    fn rejects_unknown_algorithm() {
        let mut c = make_test_challenge(1, 1);
        c["parameters"]["algorithm"] = json!("ARGON2ID");
        assert!(solve_blocking(&c, Duration::from_secs(1)).is_err());
    }
}
