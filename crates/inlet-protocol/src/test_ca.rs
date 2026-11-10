//! Test-only extra root CA, for a private test instance such as `fluxer.test`.
//!
//! Compiled only with the cargo feature `test-extra-ca`, which is off by
//! default and refused in release builds (see `lib.rs`). With the feature on,
//! the PEM file named by `INLET_TEST_EXTRA_CA_FILE` is trusted next to the
//! Mozilla roots, for the HTTP client and the gateway WebSocket alike. Without
//! the feature none of this code exists and the variable is ignored.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;

/// The environment variable with the PEM path.
pub const ENV: &str = "INLET_TEST_EXTRA_CA_FILE";

/// The certificates in the file, or none when the variable is not set. A
/// file that cannot be read or parsed is logged and ignored (the test then
/// fails on TLS, which is the safe outcome).
fn certificates() -> Vec<CertificateDer<'static>> {
    let Some(path) = std::env::var_os(ENV) else {
        return Vec::new();
    };
    let pem = match std::fs::read(&path) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read {ENV}");
            return Vec::new();
        }
    };
    CertificateDer::pem_slice_iter(&pem)
        .filter_map(|c| match c {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = ?e, "bad certificate in {ENV}");
                None
            }
        })
        .collect()
}

/// Adds the extra roots to a reqwest client.
pub(crate) fn add_to_http(mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    for cert in certificates() {
        match reqwest::Certificate::from_der(cert.as_ref()) {
            Ok(c) => builder = builder.add_root_certificate(c),
            Err(e) => tracing::warn!(error = %e, "bad certificate in {ENV}"),
        }
    }
    builder
}

/// Adds the extra roots to the gateway's root store.
pub(crate) fn add_to_store(roots: &mut rustls::RootCertStore) {
    for cert in certificates() {
        if let Err(e) = roots.add(cert) {
            tracing::warn!(error = %e, "bad certificate in {ENV}");
        }
    }
}
