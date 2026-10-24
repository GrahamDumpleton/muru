//! TLS for listeners: rustls on the ring provider (ADR 0024).
//!
//! The edge terminates TLS itself (design specification section 4.3). A
//! listener's server configuration is built once from a certificate chain
//! and a private key; each accepted connection records what was negotiated
//! so the application sees it as `muru.tls` (appendix D).

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use muru_proto::worker::v1 as pb;
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

/// The application protocols offered through ALPN, most preferred first.
const ALPN: [&[u8]; 2] = [b"h2", b"http/1.1"];

/// The cryptographic provider every TLS use in Muru is built with.
pub fn crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Read a certificate chain and a private key from PEM files.
pub fn load_pem_files(
    certificate: &Path,
    key: &Path,
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let chain = CertificateDer::pem_file_iter(certificate)
        .with_context(|| format!("reading {}", certificate.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("reading certificates from {}", certificate.display()))?;
    anyhow::ensure!(
        !chain.is_empty(),
        "{} holds no certificate",
        certificate.display()
    );
    let key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("reading the private key from {}", key.display()))?;
    Ok((chain, key))
}

/// Build the server configuration for a listener: the safe defaults of
/// rustls, no client authentication, and HTTP/2 or HTTP/1.1 through ALPN.
pub fn server_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<Arc<ServerConfig>> {
    let mut config = ServerConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .context("configuring TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .context("the certificate and private key do not go together")?;
    config.alpn_protocols = ALPN.iter().map(|protocol| protocol.to_vec()).collect();
    Ok(Arc::new(config))
}

/// What a finished handshake negotiated, for the request head.
pub(crate) fn info(connection: &rustls::ServerConnection) -> pb::TlsInfo {
    pb::TlsInfo {
        version: match connection.protocol_version() {
            Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3".to_owned(),
            Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2".to_owned(),
            Some(other) => format!("{other:?}"),
            None => String::new(),
        },
        cipher: connection
            .negotiated_cipher_suite()
            .map(|suite| format!("{:?}", suite.suite()))
            .unwrap_or_default(),
        server_name: connection.server_name().unwrap_or_default().to_owned(),
        alpn: connection
            .alpn_protocol()
            .map(|protocol| String::from_utf8_lossy(protocol).into_owned())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(names: &[&str]) -> (String, String) {
        let rcgen::CertifiedKey { cert, signing_key } = rcgen::generate_simple_self_signed(
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
        .expect("certificate");
        (cert.pem(), signing_key.serialize_pem())
    }

    fn load_pem(cert: &str, key: &str) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let dir = std::env::temp_dir().join(format!(
            "muru-tls-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, cert).expect("write cert");
        std::fs::write(&key_path, key).expect("write key");
        let loaded = load_pem_files(&cert_path, &key_path).expect("load");
        let _ = std::fs::remove_dir_all(&dir);
        loaded
    }

    #[test]
    fn matching_pair_builds_a_config_offering_h2_first() {
        let (cert, key) = pair(&["localhost"]);
        let (chain, key) = load_pem(&cert, &key);
        let config = server_config(chain, key).expect("config");
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn mismatched_key_is_rejected() {
        let (cert, _) = pair(&["localhost"]);
        let (_, other_key) = pair(&["localhost"]);
        let (chain, key) = load_pem(&cert, &other_key);
        assert!(server_config(chain, key).is_err());
    }

    #[test]
    fn missing_files_are_reported_by_path() {
        let error = load_pem_files(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        )
        .expect_err("missing");
        assert!(format!("{error:#}").contains("/nonexistent/cert.pem"));
    }
}
