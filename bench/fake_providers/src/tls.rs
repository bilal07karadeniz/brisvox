//! A test CA and a server certificate signed by it.
//!
//! Generated at start (or reused from `--ca-dir`). The clients trust the CA, never the other way
//! round: brisvox reads `BRISVOX_EXTRA_CA_FILE`, Python reads `SSL_CERT_FILE` (README.md).
//! The CA's private key only signs the one server certificate and is then dropped.

use std::path::Path;
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// PEM material of a CA and the server certificate it signed.
pub struct Material {
    /// The CA certificate (what clients are told to trust).
    pub ca_pem: String,
    /// The server certificate followed by nothing else (the CA is not sent).
    pub server_pem: String,
    /// The server's private key (PKCS#8).
    pub server_key_pem: String,
}

/// Creates a CA and a server certificate valid for `names` (DNS names and IP addresses).
///
/// # Errors
/// A message when a name is not valid for a certificate or key generation fails.
pub fn generate(names: &[String]) -> Result<Material, String> {
    let err = |e: rcgen::Error| e.to_string();
    let ca_key = KeyPair::generate().map_err(err)?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).map_err(err)?;
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "brisvox fake providers test CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca_params.self_signed(&ca_key).map_err(err)?;
    let issuer = Issuer::new(ca_params, ca_key);

    let server_key = KeyPair::generate().map_err(err)?;
    let mut params = CertificateParams::new(names.to_vec()).map_err(err)?;
    params
        .distinguished_name
        .push(DnType::CommonName, "brisvox fake providers");
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    // Strict verifiers (Python 3.13's default context) want the authority key identifier.
    params.use_authority_key_identifier_extension = true;
    let server = params.signed_by(&server_key, &issuer).map_err(err)?;

    // The CA key is dropped with the issuer: nothing can sign for this CA after this call, which
    // keeps `--ca-dir` to the three files clients and server need.
    Ok(Material {
        ca_pem: ca_cert.pem(),
        server_pem: server.pem(),
        server_key_pem: server_key.serialize_pem(),
    })
}

/// Loads `ca.pem`, `server.pem` and `server.key` from `dir`, or generates and writes them.
///
/// # Errors
/// A message for an unreadable or unwritable directory or bad PEM files.
pub fn load_or_create(dir: Option<&Path>, names: &[String]) -> Result<Material, String> {
    let Some(dir) = dir else {
        return generate(names);
    };
    let ca = dir.join("ca.pem");
    let cert = dir.join("server.pem");
    let key = dir.join("server.key");
    if ca.is_file() && cert.is_file() && key.is_file() {
        let read =
            |p: &Path| std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
        return Ok(Material {
            ca_pem: read(&ca)?,
            server_pem: read(&cert)?,
            server_key_pem: read(&key)?,
        });
    }
    let material = generate(names)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (path, text) in [
        (&ca, &material.ca_pem),
        (&cert, &material.server_pem),
        (&key, &material.server_key_pem),
    ] {
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(material)
}

/// The rustls server config: ALPN `h2` and `http/1.1` (a websocket client offers only
/// `http/1.1` and gets it), TLS 1.2 and 1.3 with the `ring` provider.
///
/// # Errors
/// A message when the PEM material does not parse or rustls rejects the pair.
pub fn server_config(material: &Material) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(material.server_pem.as_bytes())
            .collect::<Result<_, _>>()
            .map_err(|e| format!("server certificate: {e}"))?;
    let key = PrivateKeyDer::from_pem_slice(material.server_key_pem.as_bytes())
        .map_err(|e| format!("server key: {e}"))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| e.to_string())?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_is_built_from_generated_material() {
        let names = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
        let material = generate(&names).unwrap();
        assert!(material.ca_pem.contains("BEGIN CERTIFICATE"));
        let config = server_config(&material).unwrap();
        assert_eq!(config.alpn_protocols.len(), 2);
    }

    #[test]
    fn a_ca_dir_is_written_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("fake-providers-tls-{}", std::process::id()));
        let names = vec!["localhost".to_owned()];
        let first = load_or_create(Some(&dir), &names).unwrap();
        let second = load_or_create(Some(&dir), &names).unwrap();
        assert_eq!(first.ca_pem, second.ca_pem);
        assert_eq!(first.server_key_pem, second.server_key_pem);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
