//! TLS for listeners: rustls on the ring provider (ADR 0024).
//!
//! The edge terminates TLS itself (design specification section 4.3). In
//! development a listener's server configuration is built once from a
//! certificate chain and a private key; under the daemon the certificates
//! come with each snapshot and a [`Resolver`] picks one by SNI, so they
//! rotate live. Each accepted connection records what was negotiated so
//! the application sees it as `muru.tls` (appendix D).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use anyhow::Context;
use muru_proto::worker::v1 as pb;
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
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

/// Read a certificate chain and a private key from PEM text, as a
/// snapshot carries them.
pub fn parse_pem(
    certificate: &str,
    key: &str,
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let chain = CertificateDer::pem_slice_iter(certificate.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("reading the certificate chain")?;
    anyhow::ensure!(
        !chain.is_empty(),
        "the certificate PEM holds no certificate"
    );
    let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).context("reading the private key")?;

    Ok((chain, key))
}

/// A chain and key ready to serve, checked to go together.
pub fn certified_key(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<Arc<CertifiedKey>> {
    let signing = crypto_provider()
        .key_provider
        .load_private_key(key)
        .context("loading the private key")?;
    let certified = CertifiedKey::new(chain, signing);
    certified
        .keys_match()
        .context("the certificate and private key do not go together")?;

    Ok(Arc::new(certified))
}

/// One host's certificate, for the resolver: the names it answers to and
/// whether it is the one offered when the client names no host.
pub struct HostCertificate {
    /// Exact names and `*.` wildcards, as the host declares them.
    pub names: Vec<String>,
    /// The chain and key.
    pub key: Arc<CertifiedKey>,
    /// Offered when no name matches.
    pub default: bool,
}

#[derive(Default)]
struct Table {
    exact: HashMap<String, Arc<CertifiedKey>>,
    wildcards: Vec<(String, Arc<CertifiedKey>)>,
    default: Option<Arc<CertifiedKey>>,
}

/// Chooses a certificate by the name the client asked for (SNI), from
/// the hosts of the current snapshot.
#[derive(Default)]
pub struct Resolver {
    table: RwLock<Table>,
}

impl Resolver {
    /// Replace the certificates with those of a new snapshot.
    pub fn replace(&self, hosts: Vec<HostCertificate>) {
        let mut table = Table::default();

        for host in hosts {
            for name in &host.names {
                let name = crate::routing::normalise(name);

                match name.strip_prefix("*.") {
                    Some(suffix) => table
                        .wildcards
                        .push((format!(".{suffix}"), Arc::clone(&host.key))),
                    None => {
                        table.exact.insert(name, Arc::clone(&host.key));
                    }
                }
            }

            if host.default || table.default.is_none() {
                table.default = Some(host.key);
            }
        }

        table
            .wildcards
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));

        *self.table.write().expect("certificate table poisoned") = table;
    }

    /// Whether any certificate is loaded.
    pub fn is_empty(&self) -> bool {
        self.table
            .read()
            .expect("certificate table poisoned")
            .default
            .is_none()
    }
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Resolver")
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let table = self.table.read().expect("certificate table poisoned");
        let name = client_hello.server_name().map(crate::routing::normalise)?;

        if let Some(key) = table.exact.get(&name) {
            return Some(Arc::clone(key));
        }

        for (suffix, key) in &table.wildcards {
            let label = name.strip_suffix(suffix.as_str());

            if label.is_some_and(|label| !label.is_empty() && !label.contains('.')) {
                return Some(Arc::clone(key));
            }
        }

        table.default.clone()
    }
}

/// The server configuration for a listener whose certificates come from
/// snapshots through a resolver.
pub fn server_config_with_resolver(resolver: Arc<Resolver>) -> anyhow::Result<Arc<ServerConfig>> {
    let mut config = ServerConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .context("configuring TLS protocol versions")?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
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
        ja4: String::new(),
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
    fn the_resolver_picks_by_name_with_wildcards_and_a_default() {
        let (shop_cert, shop_key) = pair(&["shop.example.com", "*.shop.example.com"]);
        let (other_cert, other_key) = pair(&["other.example.com"]);
        let certified = |cert: &str, key: &str| {
            let (chain, key) = parse_pem(cert, key).unwrap();
            certified_key(chain, key).unwrap()
        };
        let shop = certified(&shop_cert, &shop_key);
        let other = certified(&other_cert, &other_key);

        let resolver = Resolver::default();
        assert!(resolver.is_empty());
        resolver.replace(vec![
            HostCertificate {
                names: vec![
                    "shop.example.com".to_owned(),
                    "*.shop.example.com".to_owned(),
                ],
                key: Arc::clone(&shop),
                default: false,
            },
            HostCertificate {
                names: vec!["other.example.com".to_owned()],
                key: Arc::clone(&other),
                default: true,
            },
        ]);
        let table = resolver.table.read().unwrap();
        assert!(Arc::ptr_eq(
            table.exact.get("shop.example.com").unwrap(),
            &shop
        ));
        assert!(Arc::ptr_eq(table.default.as_ref().unwrap(), &other));
        assert_eq!(table.wildcards[0].0, ".shop.example.com");
    }

    #[test]
    fn mismatched_pem_pair_is_refused() {
        let (cert, _) = pair(&["localhost"]);
        let (_, other_key) = pair(&["localhost"]);
        let (chain, key) = parse_pem(&cert, &other_key).unwrap();
        assert!(certified_key(chain, key).is_err());
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
