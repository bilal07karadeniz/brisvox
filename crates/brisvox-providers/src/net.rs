//! Shared TLS for the network adapters: rustls with the `ring` provider and the
//! compiled-in webpki roots. No platform verifier, no system proxy, no aws-lc-rs.
//!
//! Build the config once per process with [`tls_config`] and hand clones of the `Arc` to each
//! connector (`tokio_tungstenite::Connector::Rustls`, `reqwest::ClientBuilder::
//! use_preconfigured_tls`). The returned config has no ALPN protocols: an HTTP/2 client sets its
//! own on a clone (`h2`, `http/1.1`); WebSocket handshakes must not offer `h2`.
//!
//! # Dialing (happy eyeballs)
//! WebSocket adapters connect with [`connect_ws`], which races the resolved addresses (RFC 8305)
//! instead of trying them one after the other like `tokio_tungstenite::connect_async`; see
//! [`dial`].

use std::sync::{Arc, OnceLock};

#[cfg(feature = "ws")]
mod dial;
#[cfg(feature = "http")]
mod http;
#[cfg(feature = "ws")]
mod ws;
#[cfg(feature = "ws")]
pub use dial::{ATTEMPT_DELAY, MAX_ADDRS, dial, interleave};
#[cfg(feature = "http")]
pub use http::{HttpClientSpec, MAX_SHARED_CLIENTS, PoolStats, pool_stats, shared_client};
#[cfg(feature = "ws")]
pub use ws::{WsStream, connect_ws};

use crate::error::{Error, Result};

static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();

/// Environment variable naming a PEM file of extra CA certificates that [`tls_config`] trusts
/// besides the compiled-in webpki roots. **For tests and benchmarks only**: it lets the adapters
/// talk TLS to fake providers (`bench/fake_providers`) signed by a throwaway CA, with full
/// certificate and host name verification. It is read once, with the config; an unreadable or
/// invalid file makes the first TLS connection fail loudly instead of silently trusting less.
pub const EXTRA_CA_ENV: &str = "BRISVOX_EXTRA_CA_FILE";

/// The process-wide TLS client config (built on first use; cheap afterwards).
///
/// Trusts the webpki roots and, when [`EXTRA_CA_ENV`] is set, the certificates of that PEM file.
///
/// # Errors
/// [`Error::Protocol`] if rustls rejects the protocol versions of the `ring` provider (not
/// expected: covered by a test), or if [`EXTRA_CA_ENV`] names a file that cannot be read or holds
/// no valid certificate.
pub fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    if let Some(config) = CONFIG.get() {
        return Ok(Arc::clone(config));
    }
    let config = build_config(
        std::env::var_os(EXTRA_CA_ENV)
            .filter(|p| !p.is_empty())
            .as_deref(),
    )?;
    Ok(Arc::clone(CONFIG.get_or_init(|| config)))
}

/// The config for `extra_ca` (the value of [`EXTRA_CA_ENV`]) or, without it, for the webpki roots.
fn build_config(extra_ca: Option<&std::ffi::OsStr>) -> Result<Arc<rustls::ClientConfig>> {
    if let Some(path) = extra_ca {
        // One small read of a test file, once per process, with the config.
        let pem = std::fs::read(path).map_err(|e| Error::Protocol {
            provider: "tls",
            detail: format!("{EXTRA_CA_ENV}: cannot read the file: {e}"),
        })?;
        tracing::warn!(
            file = %std::path::Path::new(path).display(),
            "{EXTRA_CA_ENV} is set: provider connections also trust this CA (meant for tests and benchmarks)"
        );
        return tls_config_with_ca_pem(&pem);
    }
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::Protocol {
        provider: "tls",
        detail: e.to_string(),
    })?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Most certificates a private CA bundle may hold ([`tls_config_with_ca_pem`]).
pub const MAX_CA_CERTS: usize = 16;

/// A TLS client config that trusts the compiled-in webpki roots plus the certificates of the PEM
/// bundle `pem` (a private CA of an internal service). Not cached: build it once at
/// startup and keep the `Arc`. Public CAs stay trusted, so one config serves public and private
/// endpoints.
///
/// # Errors
/// [`Error::Protocol`] if `pem` holds no certificate, more than [`MAX_CA_CERTS`], a certificate
/// rustls rejects, or a PEM section that does not parse.
pub fn tls_config_with_ca_pem(pem: &[u8]) -> Result<Arc<rustls::ClientConfig>> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;

    let tls_err = |detail: String| Error::Protocol {
        provider: "tls",
        detail,
    };
    let mut roots =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut added = 0_usize;
    for cert in CertificateDer::pem_slice_iter(pem) {
        let cert = cert.map_err(|e| tls_err(format!("CA bundle: {e}")))?;
        added += 1;
        if added > MAX_CA_CERTS {
            return Err(tls_err(format!(
                "CA bundle: more than {MAX_CA_CERTS} certificates"
            )));
        }
        roots
            .add(cert)
            .map_err(|e| tls_err(format!("CA bundle: certificate rejected: {e}")))?;
    }
    if added == 0 {
        return Err(tls_err("CA bundle: no certificate found".to_owned()));
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| tls_err(e.to_string()))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_is_built_once_with_roots_and_no_alpn() {
        let first = tls_config().ok();
        let second = tls_config().ok();
        assert!(first.is_some());
        assert!(
            first
                .zip(second)
                .is_some_and(|(a, b)| Arc::ptr_eq(&a, &b) && a.alpn_protocols.is_empty())
        );
        assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
    }

    const TEST_CA_PEM: &str = include_str!("net/testdata/ca.pem");

    #[test]
    fn the_extra_ca_file_is_read_and_a_missing_one_fails_loudly() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/net/testdata/ca.pem");
        assert!(build_config(Some(path.as_os_str())).is_ok());
        assert!(build_config(Some(std::ffi::OsStr::new("no/such/ca.pem"))).is_err());
        assert!(build_config(None).is_ok());
    }

    #[test]
    fn a_ca_bundle_needs_a_certificate() {
        assert!(tls_config_with_ca_pem(b"").is_err());
        assert!(tls_config_with_ca_pem(b"not a pem file").is_err());
        let broken = "-----BEGIN CERTIFICATE-----
!!!
-----END CERTIFICATE-----
";
        assert!(tls_config_with_ca_pem(broken.as_bytes()).is_err());
        let config = tls_config_with_ca_pem(TEST_CA_PEM.as_bytes());
        assert!(config.is_ok_and(|c| c.alpn_protocols.is_empty()));
        // More certificates than the cap are refused.
        let many = TEST_CA_PEM.repeat(MAX_CA_CERTS + 1);
        assert!(tls_config_with_ca_pem(many.as_bytes()).is_err());
    }
}
