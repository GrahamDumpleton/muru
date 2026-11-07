//! TLS for listeners: rustls on the ring provider (ADR 0024).
//!
//! The edge terminates TLS itself (design specification section 4.3). In
//! development a listener's server configuration is built once from a
//! certificate chain and a private key; under the daemon the certificates
//! come with each snapshot and a [`Resolver`] picks one for each
//! connection, by its listener, the name it asks for and the address it
//! arrived on, so they rotate live. Each accepted connection records what
//! was negotiated so the application sees it as `muru.tls` (appendix D).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, RwLock};

use anyhow::Context;
use muru_proto::worker::v1 as pb;
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::server::ResolvesServerCert;
use rustls::sign::{CertifiedKey, SingleCertAndKey};
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

/// One host's certificate, for the resolver: the names it answers to, the
/// listeners it answers on and whether it is the default host.
pub struct HostCertificate {
    /// Exact names, IP addresses and `*.` wildcards, as the host declares
    /// them.
    pub names: Vec<String>,
    /// The listener addresses the host answers on, as routing compares
    /// them; empty for every listener.
    pub listeners: Vec<String>,
    /// The chain and key.
    pub key: Arc<CertifiedKey>,
    /// Whether the host is the default host.
    pub default: bool,
}

/// One host's certificate as the resolver keeps it, with the server
/// configuration that presents it.
struct Entry {
    listeners: Vec<String>,
    addresses: Vec<IpAddr>,
    config: Arc<ServerConfig>,
}

impl Entry {
    fn answers_on(&self, listener: &str) -> bool {
        self.listeners.is_empty() || self.listeners.iter().any(|address| address == listener)
    }
}

/// The certificates of one snapshot, each with its server configuration,
/// ready to replace those a [`Resolver`] holds.
#[derive(Default)]
pub struct Certificates {
    entries: Vec<Entry>,
    exact: HashMap<String, usize>,
    wildcards: Vec<(String, usize)>,
    default: Option<usize>,
}

impl Certificates {
    /// Index the hosts' certificates and build the server configuration
    /// for each.
    pub fn new(hosts: Vec<HostCertificate>) -> anyhow::Result<Self> {
        let mut table = Self::default();

        for host in hosts {
            let index = table.entries.len();
            let mut addresses = Vec::new();

            for name in &host.names {
                let name = crate::routing::normalise(name);

                if let Some(address) = address_of(&name) {
                    addresses.push(address);
                }

                match name.strip_prefix("*.") {
                    Some(suffix) => table.wildcards.push((format!(".{suffix}"), index)),
                    None => {
                        table.exact.insert(name, index);
                    }
                }
            }

            if host.default {
                table.default = Some(index);
            }

            let resolver = Arc::new(SingleCertAndKey::from(host.key));

            table.entries.push(Entry {
                listeners: host.listeners,
                addresses,
                config: config_with_resolver(resolver)?,
            });
        }

        table
            .wildcards
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));

        Ok(table)
    }
}

/// Chooses the server configuration for each TLS connection under the
/// daemon, from the hosts of the current snapshot, as routing chooses
/// the host: among the hosts that answer on the connection's listener,
/// by the name the client asks for (SNI), else by the address the
/// connection arrived on, else the default host, else the first.
#[derive(Default)]
pub struct Resolver {
    table: RwLock<Certificates>,
}

impl Resolver {
    /// Replace the certificates with those of a new snapshot.
    pub fn replace(&self, certificates: Certificates) {
        *self.table.write().expect("certificate table poisoned") = certificates;
    }

    /// Whether any certificate is loaded.
    pub fn is_empty(&self) -> bool {
        self.table
            .read()
            .expect("certificate table poisoned")
            .entries
            .is_empty()
    }

    /// The server configuration for a connection on `listener` (its
    /// address as routing names it) that arrived at `local`, asking for
    /// `server_name`; none when no host on the listener has a
    /// certificate.
    pub fn config_for(
        &self,
        server_name: Option<&str>,
        listener: &str,
        local: IpAddr,
    ) -> Option<Arc<ServerConfig>> {
        let table = self.table.read().expect("certificate table poisoned");

        choose(&table, server_name, listener, local)
            .map(|index| Arc::clone(&table.entries[index].config))
    }
}

/// The entry for a connection: by name (exact, then the longest wildcard),
/// else with no name by the local address, else the default host, else
/// the first, each only among the hosts that answer on the listener.
fn choose(
    table: &Certificates,
    server_name: Option<&str>,
    listener: &str,
    local: IpAddr,
) -> Option<usize> {
    let answers = |index: &usize| table.entries[*index].answers_on(listener);

    match server_name.map(crate::routing::normalise) {
        Some(name) => {
            if let Some(index) = table.exact.get(&name).copied().filter(answers) {
                return Some(index);
            }

            let wildcard = table.wildcards.iter().find(|(suffix, index)| {
                crate::routing::wildcard_matches(suffix, &name) && answers(index)
            });

            if let Some((_, index)) = wildcard {
                return Some(*index);
            }
        }
        None => {
            // A client connecting by IP address names no host, so the
            // address the connection arrived on stands in for the name.

            let local = local.to_canonical();
            let by_address = (0..table.entries.len())
                .find(|index| answers(index) && table.entries[*index].addresses.contains(&local));

            if by_address.is_some() {
                return by_address;
            }
        }
    }

    table
        .default
        .filter(answers)
        .or_else(|| (0..table.entries.len()).find(answers))
}

/// The address a host name stands for when it is an IP address, written
/// bare or, for IPv6, in brackets as a Host header carries it.
fn address_of(name: &str) -> Option<IpAddr> {
    let bare = name
        .strip_prefix('[')
        .and_then(|name| name.strip_suffix(']'))
        .unwrap_or(name);

    bare.parse::<IpAddr>()
        .ok()
        .map(|address| address.to_canonical())
}

/// The server configuration for one certificate under the daemon.
fn config_with_resolver(
    resolver: Arc<dyn ResolvesServerCert>,
) -> anyhow::Result<Arc<ServerConfig>> {
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
    fn certificates_are_chosen_as_routing_chooses_hosts() {
        let host = |names: &[&str], listeners: &[&str], default: bool| {
            let (cert, key) = pair(&["localhost"]);
            let (chain, key) = parse_pem(&cert, &key).unwrap();

            HostCertificate {
                names: names.iter().map(|name| (*name).to_owned()).collect(),
                listeners: listeners.iter().map(|name| (*name).to_owned()).collect(),
                key: certified_key(chain, key).unwrap(),
                default,
            }
        };

        // Hosts 0 and 1 answer everywhere, host 2 on the second listener
        // only, by name and by two addresses, and host 3 is the default.

        let table = Certificates::new(vec![
            host(&["shop.example.com", "*.shop.example.com"], &[], false),
            host(&["other.example.com"], &[], false),
            host(
                &["inside.example.com", "10.0.0.5", "[::1]"],
                &["10.0.0.5:443"],
                false,
            ),
            host(&["fallback.example.com"], &[], true),
        ])
        .unwrap();
        let (first, second) = ("0.0.0.0:443", "10.0.0.5:443");
        let any: IpAddr = "192.0.2.1".parse().unwrap();
        let chosen = |name: Option<&str>, listener: &str, local: &str| {
            choose(&table, name, listener, local.parse().unwrap())
        };

        // By name: exact (case and a trailing dot aside), then wildcard.

        assert_eq!(
            chosen(Some("Shop.Example.COM."), first, "192.0.2.1"),
            Some(0)
        );
        assert_eq!(
            chosen(Some("eu.shop.example.com"), first, "192.0.2.1"),
            Some(0)
        );
        assert_eq!(
            chosen(Some("other.example.com"), first, "192.0.2.1"),
            Some(1)
        );

        // A host is chosen only on a listener it answers on.

        assert_eq!(
            chosen(Some("inside.example.com"), second, "10.0.0.5"),
            Some(2)
        );
        assert_eq!(
            chosen(Some("inside.example.com"), first, "10.0.0.5"),
            Some(3)
        );
        assert_eq!(chosen(None, first, "10.0.0.5"), Some(3));

        // With no name, the address the connection arrived on, IPv6 and
        // IPv4-mapped included; else the default.

        assert_eq!(chosen(None, second, "10.0.0.5"), Some(2));
        assert_eq!(chosen(None, second, "::ffff:10.0.0.5"), Some(2));
        assert_eq!(chosen(None, second, "::1"), Some(2));
        assert_eq!(chosen(None, second, "10.0.0.6"), Some(3));
        assert_eq!(
            chosen(Some("a.eu.shop.example.com"), first, "192.0.2.1"),
            Some(3)
        );
        assert_eq!(chosen(Some("10.0.0.5"), first, "10.0.0.5"), Some(3));

        // Without a default on the listener, the first host on it; with no
        // host on it, none.

        let table = Certificates::new(vec![
            host(&["inside.example.com"], &[second], true),
            host(&["shop.example.com"], &[], false),
            host(&["other.example.com"], &[], false),
        ])
        .unwrap();
        assert_eq!(choose(&table, None, first, any), Some(1));
        assert_eq!(choose(&table, None, second, any), Some(0));

        let table =
            Certificates::new(vec![host(&["inside.example.com"], &[second], false)]).unwrap();
        assert_eq!(choose(&table, Some("inside.example.com"), first, any), None);
        assert_eq!(choose(&Certificates::default(), None, first, any), None);

        // The resolver hands out the chosen host's configuration, with the
        // listener's protocols.

        let resolver = Resolver::default();
        assert!(resolver.is_empty());
        resolver.replace(Certificates::new(vec![host(&["shop.example.com"], &[], false)]).unwrap());
        let config = resolver.config_for(None, first, any).unwrap();
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
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
