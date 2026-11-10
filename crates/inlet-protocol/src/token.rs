//! The user session token.

use std::fmt;

use sha2::{Digest, Sha256};
use zeroize::Zeroize;

/// A Fluxer user session token.
///
/// The value never shows up in `Debug` output, and the memory is wiped when the
/// token is dropped. There is no `Display` implementation on purpose: call
/// [`Token::expose_secret`] at the one place that needs the raw value (for
/// example, to hand it to the OS keychain).
///
/// Fluxer has no refresh token. A token lasts until it is revoked: by signing
/// out, by a password change, or by the API when the global rate limit is hit.
pub struct Token(String);

impl Token {
    /// Wraps a raw token value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw token value. Keep the borrow short and never log it.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// A stable, non-reversible ID for this token, used to share one rate
    /// limiter per token.
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_bytes()).into()
    }
}

impl Clone for Token {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl PartialEq for Token {
    fn eq(&self, other: &Self) -> bool {
        // Compare digests so the comparison time does not depend on the value.
        self.fingerprint() == other.fingerprint()
    }
}

impl Eq for Token {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let token = Token::new("very-secret-value");
        let shown = format!("{token:?}");
        assert!(!shown.contains("very-secret-value"));
        assert_eq!(shown, "Token(<redacted>)");
    }

    #[test]
    fn expose_returns_value() {
        let token = Token::new("abc");
        assert_eq!(token.expose_secret(), "abc");
        assert_eq!(token.clone(), token);
    }
}
