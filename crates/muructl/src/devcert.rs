//! Development certificates for `muructl run --https` (design specification
//! section 13.1): one certificate for localhost, 127.0.0.1, ::1 and any
//! --host names, issued by the local mkcert authority when there is one and
//! self-signed otherwise. It lives in the user cache directory (section
//! 13.8), never in the project, and is reused until it nears expiry or no
//! longer covers the names asked for.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use time::OffsetDateTime;
use x509_parser::prelude::GeneralName;

/// How long a generated certificate is valid.
const VALIDITY: time::Duration = time::Duration::days(365);
/// A certificate with less than this left is replaced.
const RENEW_BEFORE: time::Duration = time::Duration::days(30);
/// Names every development certificate carries.
const DEFAULT_NAMES: [&str; 3] = ["localhost", "127.0.0.1", "::1"];
/// The certificate and key file names under CACHE/certs.
const CERTIFICATE_FILE: &str = "dev.pem";
const KEY_FILE: &str = "dev-key.pem";

/// Who signed the development certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authority {
    /// The mkcert certificate authority kept in this directory.
    Mkcert(PathBuf),
    /// No authority: the certificate signed itself.
    SelfSigned,
}

/// A development certificate ready to serve.
#[derive(Debug)]
pub struct DevCertificate {
    /// The certificate, PEM.
    pub certificate: PathBuf,
    /// Its private key, PEM.
    pub key: PathBuf,
    /// The names it covers.
    pub names: Vec<String>,
    /// Who signed it.
    pub authority: Authority,
    /// Whether an existing certificate was reused rather than made.
    pub reused: bool,
}

impl DevCertificate {
    /// One or two lines for the terminal saying what will be served and,
    /// for a self-signed certificate, how to get a trusted one instead.
    pub fn describe(&self) -> String {
        let names = self.names.join(", ");
        let state = if self.reused { "reused" } else { "created" };

        match &self.authority {
            Authority::Mkcert(root) => format!(
                "muru: certificate for {names} issued by the mkcert authority in {} ({state}, {})",
                root.display(),
                self.certificate.display()
            ),
            Authority::SelfSigned => format!(
                "muru: self-signed certificate for {names} ({state}, {}); browsers will warn until it is trusted\n\
                 muru: for a trusted certificate install mkcert (https://mkcert.dev), run mkcert -install and start again",
                self.certificate.display()
            ),
        }
    }
}

/// The user cache directory for Muru (section 13.8): MURU_CACHE_DIR, else
/// the platform's cache location.
pub fn cache_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = env_path("MURU_CACHE_DIR") {
        return Ok(dir);
    }

    let home = env_path("HOME")
        .context("HOME is not set, so the cache directory is unknown; set MURU_CACHE_DIR")?;

    if cfg!(target_os = "macos") {
        return Ok(home.join("Library").join("Caches").join("muru"));
    }

    Ok(env_path("XDG_CACHE_HOME")
        .unwrap_or_else(|| home.join(".cache"))
        .join("muru"))
}

/// Where mkcert keeps its authority, when one is installed: CAROOT from the
/// environment (which mkcert itself honours exclusively), else what
/// `mkcert -CAROOT` prints, else mkcert's default location.
pub fn mkcert_root() -> Option<PathBuf> {
    if let Some(root) = env_path("CAROOT") {
        return has_authority(&root).then_some(root);
    }

    if let Ok(output) = Command::new("mkcert").arg("-CAROOT").output()
        && output.status.success()
    {
        let printed = String::from_utf8_lossy(&output.stdout).trim().to_owned();

        if !printed.is_empty() {
            let root = PathBuf::from(printed);

            if has_authority(&root) {
                return Some(root);
            }
        }
    }
    let home = env_path("HOME")?;

    let default = if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("mkcert")
    } else {
        env_path("XDG_DATA_HOME")
            .unwrap_or_else(|| home.join(".local").join("share"))
            .join("mkcert")
    };
    has_authority(&default).then_some(default)
}

fn has_authority(root: &Path) -> bool {
    root.join("rootCA.pem").is_file() && root.join("rootCA-key.pem").is_file()
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Find or create the development certificate for the default names plus
/// `extra_names`, in `cache`, issued by mkcert when it is installed.
pub fn obtain(cache: &Path, extra_names: &[String]) -> anyhow::Result<DevCertificate> {
    let authority = mkcert_root().map_or(Authority::SelfSigned, Authority::Mkcert);
    obtain_from(cache, extra_names, authority)
}

fn obtain_from(
    cache: &Path,
    extra_names: &[String],
    authority: Authority,
) -> anyhow::Result<DevCertificate> {
    let names = names(extra_names)?;
    let dir = cache.join("certs");
    let certificate = dir.join(CERTIFICATE_FILE);
    let key = dir.join(KEY_FILE);

    if let Some(existing) = usable(&certificate, &key, &names, &authority) {
        return Ok(DevCertificate {
            certificate,
            key,
            names: existing,
            authority,
            reused: true,
        });
    }

    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let (certificate_pem, key_pem) = generate(&names, &authority)?;
    write_private(&key, key_pem.as_bytes())?;
    std::fs::write(&certificate, certificate_pem)
        .with_context(|| format!("writing {}", certificate.display()))?;
    Ok(DevCertificate {
        certificate,
        key,
        names,
        authority,
        reused: false,
    })
}

/// The default names followed by the extra ones, without repeats.
fn names(extra: &[String]) -> anyhow::Result<Vec<String>> {
    let mut names: Vec<String> = DEFAULT_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();

    for name in extra {
        let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
        anyhow::ensure!(!name.is_empty(), "--host needs a host name");
        anyhow::ensure!(
            name.parse::<IpAddr>().is_ok()
                || name.split('.').all(|label| {
                    !label.is_empty()
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'*')
                }),
            "--host {name:?} is not a host name or address"
        );

        if !names.contains(&name) {
            names.push(name);
        }
    }

    Ok(names)
}

/// The names of an existing certificate, when it covers every requested
/// name, was signed by the current authority and is not close to expiry.
fn usable(
    certificate: &Path,
    key: &Path,
    names: &[String],
    authority: &Authority,
) -> Option<Vec<String>> {
    let pem = std::fs::read(certificate).ok()?;
    let key_pem = std::fs::read_to_string(key).ok()?;
    KeyPair::from_pem(&key_pem).ok()?;
    let (_, parsed) = x509_parser::pem::parse_x509_pem(&pem).ok()?;
    let cert = parsed.parse_x509().ok()?;
    let not_after =
        OffsetDateTime::from_unix_timestamp(cert.validity().not_after.timestamp()).ok()?;

    if not_after - OffsetDateTime::now_utc() < RENEW_BEFORE {
        return None;
    }

    let covered: Vec<String> = cert
        .subject_alternative_name()
        .ok()??
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::DNSName(dns) => Some(dns.to_ascii_lowercase()),
            GeneralName::IPAddress(bytes) => match bytes.len() {
                4 => <[u8; 4]>::try_from(*bytes)
                    .ok()
                    .map(|octets| IpAddr::from(octets).to_string()),
                16 => <[u8; 16]>::try_from(*bytes)
                    .ok()
                    .map(|octets| IpAddr::from(octets).to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect();

    if !names.iter().all(|name| covered.contains(name)) {
        return None;
    }

    let signed_by_current = match authority {
        Authority::SelfSigned => cert.issuer().as_raw() == cert.subject().as_raw(),
        Authority::Mkcert(root) => {
            let ca_pem = std::fs::read(root.join("rootCA.pem")).ok()?;
            let (_, ca) = x509_parser::pem::parse_x509_pem(&ca_pem).ok()?;
            let ca = ca.parse_x509().ok()?;
            cert.issuer().as_raw() == ca.subject().as_raw()
        }
    };

    signed_by_current.then_some(covered)
}

/// Make a certificate for the names, signed by the authority, as PEM text:
/// the certificate, then its key.
fn generate(names: &[String], authority: &Authority) -> anyhow::Result<(String, String)> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("generating a key")?;
    let mut params =
        CertificateParams::new(names.to_vec()).context("the certificate names are not valid")?;
    let now = OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + VALIDITY;
    let mut subject = DistinguishedName::new();
    subject.push(DnType::OrganizationName, "muru development certificate");
    subject.push(DnType::CommonName, "muru development certificate");
    params.distinguished_name = subject;
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let certificate = match authority {
        Authority::SelfSigned => params
            .self_signed(&key)
            .context("signing the certificate")?,
        Authority::Mkcert(root) => {
            params.use_authority_key_identifier_extension = true;
            let ca_pem = std::fs::read_to_string(root.join("rootCA.pem"))
                .with_context(|| format!("reading the mkcert authority in {}", root.display()))?;
            let ca_key =
                std::fs::read_to_string(root.join("rootCA-key.pem")).with_context(|| {
                    format!("reading the mkcert authority key in {}", root.display())
                })?;
            let ca_key =
                KeyPair::from_pem(&ca_key).context("the mkcert authority key is not usable")?;
            let issuer = Issuer::from_ca_cert_pem(&ca_pem, ca_key)
                .context("the mkcert authority certificate is not usable")?;
            params
                .signed_by(&key, &issuer)
                .context("signing the certificate with the mkcert authority")?
        }
    };

    Ok((certificate.pem(), key.serialize_pem()))
}

/// Write a file only its owner can read.
fn write_private(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muru-devcert-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// A throwaway authority laid out as mkcert lays out its own.
    fn fake_mkcert(dir: &Path) {
        let key = KeyPair::generate().expect("ca key");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let mut subject = DistinguishedName::new();
        subject.push(DnType::OrganizationName, "test authority");
        subject.push(DnType::CommonName, "test authority");
        params.distinguished_name = subject;
        let cert = params.self_signed(&key).expect("ca cert");
        std::fs::write(dir.join("rootCA.pem"), cert.pem()).expect("write ca");
        std::fs::write(dir.join("rootCA-key.pem"), key.serialize_pem()).expect("write ca key");
    }

    #[test]
    fn names_add_hosts_once_and_reject_nonsense() {
        let chosen = names(&["Shop.localhost".to_owned(), "localhost".to_owned()]).expect("names");
        assert_eq!(chosen, ["localhost", "127.0.0.1", "::1", "shop.localhost"]);
        assert!(names(&["not a host".to_owned()]).is_err());
        assert!(names(&[String::new()]).is_err());
    }

    #[test]
    fn self_signed_certificate_is_made_once_and_reused() {
        let cache = temp_dir("self");
        let first = obtain_with(&cache, &[], &Authority::SelfSigned);
        assert!(!first.reused);
        assert_eq!(first.authority, Authority::SelfSigned);
        let (chain, key) =
            muru_edge::tls::load_pem_files(&first.certificate, &first.key).expect("loads");
        muru_edge::tls::server_config(chain, key).expect("serves");
        let again = obtain_with(&cache, &[], &Authority::SelfSigned);
        assert!(again.reused);
        assert_eq!(again.names, ["localhost", "127.0.0.1", "::1"]);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn a_new_name_or_authority_replaces_the_certificate() {
        let cache = temp_dir("names");
        let first = obtain_with(&cache, &[], &Authority::SelfSigned);
        assert!(!first.reused);
        let wider = obtain_with(
            &cache,
            &["shop.localhost".to_owned()],
            &Authority::SelfSigned,
        );
        assert!(!wider.reused);
        assert!(wider.names.contains(&"shop.localhost".to_owned()));
        let same = obtain_with(&cache, &[], &Authority::SelfSigned);
        assert!(same.reused, "a subset of the names reuses the certificate");
        let root = temp_dir("ca");
        fake_mkcert(&root);
        let issued = obtain_with(&cache, &[], &Authority::Mkcert(root.clone()));
        assert!(!issued.reused);
        let issued_again = obtain_with(&cache, &[], &Authority::Mkcert(root.clone()));
        assert!(issued_again.reused);
        let _ = std::fs::remove_dir_all(&cache);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `obtain` with the authority decided by the test rather than the
    /// environment.
    fn obtain_with(cache: &Path, extra: &[String], authority: &Authority) -> DevCertificate {
        obtain_from(cache, extra, authority.clone()).expect("certificate")
    }
}
