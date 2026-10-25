//! HTTP upstreams (design specification section 4.13): unmanaged HTTP
//! servers that routes, static fallbacks, traffic shares and
//! authentication sub-requests can target. A request to an upstream is
//! re-serialised as HTTP/1.1 or HTTP/2 after strict normalisation, with
//! its body re-framed by the edge. The addresses take requests in turn;
//! one whose requests fail is ejected for a while and then tried again
//! gradually, and probes of a configured path mark each address healthy
//! or not.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{CONNECTION, CONTENT_LENGTH, EXPECT, HOST, TRANSFER_ENCODING, UPGRADE};
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, Uri, Version};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::{Body as _, Frame, Incoming};
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use muru_proto::edge::v1 as pb;
use muru_proto::worker::v1 as worker;
use rustls_pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};

use crate::BoxError;
use crate::errors::Problem;
use crate::link::{self, BodyError, Context, Handled, Recording};
use crate::metrics::Sample;
use crate::routing::RedirectFlags;

/// The reason an upstream that could not answer is reported with.
pub const FAILED_REASON: &str = "upstream_failed";

/// How long a connection may take to open when the upstream does not say.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an ejected address is left out when the upstream does not say.
const DEFAULT_EJECT_FOR: Duration = Duration::from_secs(30);

/// The most an ejection grows to, as a multiple of `eject_for`.
const MAX_EJECTION_GROWTH: u32 = 10;

/// How often addresses are probed when the upstream does not say.
const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_secs(10);

/// How long a probe may take when the upstream does not say.
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Idle connections kept per address, and how long one is kept.
const IDLE_PER_ADDRESS: usize = 16;
const IDLE_FOR: Duration = Duration::from_secs(60);

/// The name an upstream's requests are logged under as their group.
pub const GROUP: &str = "upstream";

/// The body sent to an upstream: the client's, streamed, or one the edge
/// buffered.
type OutBody = UnsyncBoxBody<Bytes, BoxError>;

/// A connection to an upstream, whichever transport carries it.
trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

type Connection = TokioIo<Box<dyn Stream>>;

/// The HTTP version an upstream speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Http1,
    H2c,
    H2,
}

impl Protocol {
    fn is_h2(self) -> bool {
        matches!(self, Self::H2c | Self::H2)
    }
}

/// Where an address listens.
enum Target {
    Tcp(String),
    Unix(PathBuf),
}

/// What probes and requests have found out about an address.
#[derive(Default)]
struct State {
    /// What probes say: nothing yet, or without probes.
    probed: Option<bool>,
    probe_successes: u32,
    probe_failures: u32,
    /// Failed requests in a row.
    failures: u32,
    /// Set aside until then, after failures.
    ejected_until: Option<Instant>,
    /// Ejections in a row, each longer than the last.
    ejections: u32,
    /// A trial request is in flight after an ejection.
    trying: bool,
}

/// Whether a request may try an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// In service.
    Open,
    /// Ejected, but its time is up and no trial is in flight.
    Trial,
    /// Ejected or unhealthy.
    Closed,
}

/// One address of an upstream, with its connections and its health.
struct Address {
    text: String,
    target: Target,
    state: Mutex<State>,
    /// HTTP/1.1 connections between requests, newest last.
    idle: Mutex<VecDeque<(http1::SendRequest<OutBody>, Instant)>>,
    /// The HTTP/2 connection, which multiplexes.
    h2: Mutex<Option<http2::SendRequest<OutBody>>>,
}

impl Address {
    fn new(text: &str) -> Result<Self, String> {
        let target = match text.strip_prefix("unix:") {
            Some(path) if path.starts_with('/') => Target::Unix(PathBuf::from(path)),
            Some(_) => return Err(format!("{text:?}: a Unix socket path must be absolute")),
            None => {
                let (host, port) = text
                    .rsplit_once(':')
                    .ok_or_else(|| format!("{text:?} is not host:port or unix:/path"))?;

                if host.is_empty() || port.parse::<u16>().is_err() {
                    return Err(format!("{text:?} is not host:port or unix:/path"));
                }

                Target::Tcp(text.to_owned())
            }
        };

        Ok(Self {
            text: text.to_owned(),
            target,
            state: Mutex::new(State::default()),
            idle: Mutex::new(VecDeque::new()),
            h2: Mutex::new(None),
        })
    }

    /// The host part of a TCP address, for TLS.
    fn host(&self) -> Option<String> {
        match &self.target {
            Target::Tcp(text) => text.rsplit_once(':').map(|(host, _)| {
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_owned()
            }),
            Target::Unix(_) => None,
        }
    }

    fn admission(&self, now: Instant) -> Admission {
        let state = self.state.lock().expect("upstream state poisoned");

        if state.probed == Some(false) {
            return Admission::Closed;
        }

        match state.ejected_until {
            None => Admission::Open,
            Some(until) if now < until || state.trying => Admission::Closed,
            Some(_) => Admission::Trial,
        }
    }

    /// Whether the address takes requests now.
    fn available(&self) -> bool {
        self.admission(Instant::now()) != Admission::Closed
    }

    /// Claim the one trial request an ejected address gets once its time
    /// is up.
    fn claim_trial(&self) -> bool {
        let mut state = self.state.lock().expect("upstream state poisoned");

        if state.trying {
            return false;
        }

        state.trying = true;
        true
    }

    /// A request got its response head: the address is in service, and
    /// an ejection it was recovering from is over.
    fn succeeded(&self, upstream: &str) {
        let mut state = self.state.lock().expect("upstream state poisoned");
        state.failures = 0;
        state.trying = false;

        if state.ejected_until.take().is_some() {
            state.ejections = 0;
            tracing::info!(upstream, address = %self.text, "upstream address back in service");
        }
    }

    /// A request could not be sent or got no response head: after
    /// enough in a row, the address is ejected, for longer each time.
    fn failed(&self, upstream: &str, eject_after: u32, eject_for: Duration) {
        let mut state = self.state.lock().expect("upstream state poisoned");
        state.failures += 1;
        let trial_failed = std::mem::take(&mut state.trying);

        if eject_after == 0 || (state.failures < eject_after && !trial_failed) {
            return;
        }

        state.ejections = (state.ejections + 1).min(MAX_EJECTION_GROWTH);
        let growth = (1u32 << (state.ejections - 1).min(4)).min(MAX_EJECTION_GROWTH);
        let period = eject_for * growth;
        state.ejected_until = Some(Instant::now() + period);
        state.failures = 0;
        tracing::warn!(
            upstream,
            address = %self.text,
            "upstream address ejected for {}s after failed requests",
            period.as_secs()
        );
    }

    /// A probe's verdict, counted towards the thresholds.
    fn probed(&self, ok: bool, config: &Probe, upstream: &str) {
        let mut state = self.state.lock().expect("upstream state poisoned");

        if ok {
            state.probe_successes += 1;
            state.probe_failures = 0;
        } else {
            state.probe_failures += 1;
            state.probe_successes = 0;
        }

        let verdict = if state.probe_failures >= config.unhealthy_after {
            Some(false)
        } else if state.probe_successes >= config.healthy_after || state.probed.is_none() && ok {
            Some(true)
        } else {
            state.probed
        };

        if verdict != state.probed {
            match verdict {
                Some(true) => {
                    tracing::info!(upstream, address = %self.text, "upstream address healthy")
                }
                Some(false) => {
                    tracing::warn!(upstream, address = %self.text, "upstream address unhealthy")
                }
                None => {}
            }

            state.probed = verdict;
        }
    }
}

/// Active health checks.
struct Probe {
    path: Option<String>,
    interval: Duration,
    timeout: Duration,
    unhealthy_after: u32,
    healthy_after: u32,
}

/// What requests and the probe task share.
struct Shared {
    name: String,
    addresses: Vec<Arc<Address>>,
    protocol: Protocol,
    /// The TLS configuration and the name checked, when the upstream
    /// speaks TLS.
    tls: Option<(Arc<rustls::ClientConfig>, Option<String>)>,
    trust_muru_headers: bool,
    /// The file hints by path the upstream may give when trusted
    /// (section 4.9).
    sendfile: Arc<crate::routing::Sendfile>,
    connect_timeout: Duration,
    eject_after: u32,
    eject_for: Duration,
    /// The address the next request starts with.
    next: AtomicUsize,
}

/// An upstream of a snapshot.
pub struct Upstream {
    shared: Arc<Shared>,
    probe: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        if let Some(probe) = &self.probe {
            probe.abort();
        }
    }
}

impl Upstream {
    /// Compile an upstream from a snapshot, starting its probes.
    pub fn from_message(message: &pb::Upstream) -> Result<Self, String> {
        if message.addresses.is_empty() {
            return Err("addresses names nowhere".to_owned());
        }

        let addresses = message
            .addresses
            .iter()
            .map(|text| Address::new(text).map(Arc::new))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("addresses: {error}"))?;

        let protocol = match message.protocol.as_str() {
            "" | "http1" => Protocol::Http1,
            "h2c" => Protocol::H2c,
            "h2" => Protocol::H2,
            other => return Err(format!("protocol {other:?} is not http1, h2c or h2")),
        };

        let tls = if message.tls || protocol == Protocol::H2 {
            let server_name =
                (!message.tls_server_name.is_empty()).then(|| message.tls_server_name.clone());

            if server_name.is_none() && addresses.iter().any(|address| address.host().is_none()) {
                return Err("tls.server_name is needed with a Unix socket address".to_owned());
            }

            Some((
                Arc::new(client_config(
                    &message.tls_ca_pem,
                    message.tls_insecure,
                    protocol,
                )?),
                server_name,
            ))
        } else {
            None
        };

        let millis = |value: u64, default: Duration| {
            if value == 0 {
                default
            } else {
                Duration::from_millis(value)
            }
        };
        let shared = Arc::new(Shared {
            name: message.name.clone(),
            addresses,
            protocol,
            tls,
            trust_muru_headers: message.trust_muru_headers,
            sendfile: crate::routing::Sendfile::new(
                &message.sendfile_roots,
                &message.sendfile_header,
            ),
            connect_timeout: millis(message.connect_timeout_ms, DEFAULT_CONNECT_TIMEOUT),
            eject_after: message.eject_after,
            eject_for: millis(message.eject_for_ms, DEFAULT_EJECT_FOR),
            next: AtomicUsize::new(0),
        });

        let probe = message.probe.as_ref().map(|probe| {
            let config = Probe {
                path: (!probe.path.is_empty()).then(|| probe.path.clone()),
                interval: millis(probe.interval_ms, DEFAULT_PROBE_INTERVAL),
                timeout: millis(probe.timeout_ms, DEFAULT_PROBE_TIMEOUT),
                unhealthy_after: if probe.unhealthy_after == 0 {
                    3
                } else {
                    probe.unhealthy_after
                },
                healthy_after: if probe.healthy_after == 0 {
                    2
                } else {
                    probe.healthy_after
                },
            };
            tokio::spawn(run_probes(Arc::clone(&shared), config))
        });

        Ok(Self { shared, probe })
    }

    /// The upstream's name.
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Whether X-Muru-Redirect from the upstream is honoured.
    pub fn trusts_muru_headers(&self) -> bool {
        self.shared.trust_muru_headers
    }

    /// The file hints by path the upstream may give (section 4.9).
    pub fn sendfile(&self) -> Arc<crate::routing::Sendfile> {
        Arc::clone(&self.shared.sendfile)
    }

    /// The health gauge of each address: 1 when it takes requests.
    pub fn samples(&self) -> Vec<Sample> {
        self.shared
            .addresses
            .iter()
            .map(|address| {
                Sample::new(
                    "muru_upstream_health",
                    &[
                        ("upstream", self.shared.name.as_str()),
                        ("address", address.text.as_str()),
                    ],
                    if address.available() { 1.0 } else { 0.0 },
                )
            })
            .collect()
    }
}

/// The TLS client configuration for an upstream: the given roots or the
/// public ones, or no checking at all when the operator says so, and the
/// protocol through ALPN.
fn client_config(
    ca_pem: &str,
    insecure: bool,
    protocol: Protocol,
) -> Result<rustls::ClientConfig, String> {
    use rustls_pki_types::pem::PemObject;

    let builder = rustls::ClientConfig::builder_with_provider(crate::tls::crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("tls: {error}"))?;

    let mut config = if insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerification))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();

        if ca_pem.is_empty() {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        } else {
            let certificates = rustls_pki_types::CertificateDer::pem_slice_iter(ca_pem.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("tls.ca: {error}"))?;
            let (added, _) = roots.add_parsable_certificates(certificates);

            if added == 0 {
                return Err("tls.ca holds no certificate".to_owned());
            }
        }

        builder.with_root_certificates(roots).with_no_client_auth()
    };

    config.alpn_protocols = vec![if protocol.is_h2() {
        b"h2".to_vec()
    } else {
        b"http/1.1".to_vec()
    }];
    Ok(config)
}

/// A verifier that accepts any certificate, for `verify = false`.
#[derive(Debug)]
struct NoVerification;

impl rustls::client::danger::ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _: &rustls_pki_types::CertificateDer<'_>,
        _: &[rustls_pki_types::CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls_pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls_pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        crate::tls::crypto_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Open a connection to an address, with TLS when the upstream speaks it.
async fn connect(shared: &Shared, address: &Address) -> std::io::Result<Box<dyn Stream>> {
    let stream: Box<dyn Stream> = match &address.target {
        Target::Tcp(target) => {
            let stream = TcpStream::connect(target).await?;
            stream.set_nodelay(true)?;
            Box::new(stream)
        }
        Target::Unix(path) => Box::new(UnixStream::connect(path).await?),
    };

    let Some((config, server_name)) = &shared.tls else {
        return Ok(stream);
    };

    let name = server_name
        .clone()
        .or_else(|| address.host())
        .unwrap_or_default();
    let name = ServerName::try_from(name)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let connector = tokio_rustls::TlsConnector::from(Arc::clone(config));
    let tls = connector.connect(name, stream).await?;
    Ok(Box::new(tls))
}

/// Open a connection within the connect timeout.
async fn connect_in_time(shared: &Shared, address: &Address) -> std::io::Result<Connection> {
    match tokio::time::timeout(shared.connect_timeout, connect(shared, address)).await {
        Ok(Ok(stream)) => Ok(TokioIo::new(stream)),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the connection did not open within connect_timeout",
        )),
    }
}

/// A way to send one request to an address.
enum Sender {
    Http1 {
        sender: http1::SendRequest<OutBody>,
        /// Taken from the idle connections rather than opened now.
        reused: bool,
    },
    Http2(http2::SendRequest<OutBody>),
}

/// A connection ready to take a request: an idle one, else a new one.
async fn sender_for(shared: &Shared, address: &Address) -> std::io::Result<Sender> {
    if shared.protocol.is_h2() {
        let existing = address.h2.lock().expect("upstream h2 poisoned").clone();

        if let Some(sender) = existing
            && !sender.is_closed()
        {
            return Ok(Sender::Http2(sender));
        }

        let io = connect_in_time(shared, address).await?;
        let (sender, connection) = http2::Builder::new(TokioExecutor::new())
            .handshake(io)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, "an upstream HTTP/2 connection ended with an error");
            }
        });
        *address.h2.lock().expect("upstream h2 poisoned") = Some(sender.clone());
        return Ok(Sender::Http2(sender));
    }

    let now = Instant::now();

    loop {
        let idle = address
            .idle
            .lock()
            .expect("upstream idle poisoned")
            .pop_back();

        match idle {
            Some((sender, since)) => {
                if now.duration_since(since) < IDLE_FOR && !sender.is_closed() && sender.is_ready()
                {
                    return Ok(Sender::Http1 {
                        sender,
                        reused: true,
                    });
                }
            }
            None => break,
        }
    }

    let io = connect_in_time(shared, address).await?;
    let (sender, connection) = http1::Builder::new()
        .handshake(io)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    tokio::spawn(async move {
        if let Err(error) = connection.with_upgrades().await {
            tracing::debug!(%error, "an upstream connection ended with an error");
        }
    });

    Ok(Sender::Http1 {
        sender,
        reused: false,
    })
}

/// Put an HTTP/1.1 connection back for the next request, once its
/// response is fully read.
fn keep(address: &Address, sender: http1::SendRequest<OutBody>) {
    if sender.is_closed() {
        return;
    }

    let mut idle = address.idle.lock().expect("upstream idle poisoned");

    while idle.len() >= IDLE_PER_ADDRESS {
        idle.pop_front();
    }

    idle.push_back((sender, Instant::now()));
}

/// Why a request to an address did not get a response head.
enum Failure {
    /// Nothing was sent, and the request is back in its slot for another
    /// address to try.
    Unsent(String),
    /// The request went out and the response did not come whole.
    Sent(String),
    /// The deadline passed first.
    Timeout,
}

/// The response head and, over HTTP/1.1, the connection to keep once
/// the body has arrived.
type Answer = (Response<Incoming>, Option<http1::SendRequest<OutBody>>);

/// Send the request in `slot` to an address and wait for the response
/// head. A request that was never sent goes back into the slot.
async fn send(
    shared: &Shared,
    address: &Address,
    slot: &mut Option<Request<OutBody>>,
    deadline: Instant,
) -> Result<Answer, Failure> {
    loop {
        let sender = match sender_for(shared, address).await {
            Ok(sender) => sender,
            Err(error) => return Err(Failure::Unsent(error.to_string())),
        };

        let outgoing = slot.take().expect("the request is sent once");

        let (sent, kept) = match sender {
            Sender::Http1 { mut sender, reused } => {
                let sent =
                    tokio::time::timeout_at(deadline.into(), sender.try_send_request(outgoing));

                match sent.await {
                    Ok(Ok(response)) => return Ok((response, Some(sender))),
                    Ok(Err(error)) => (error, reused),
                    Err(_) => return Err(Failure::Timeout),
                }
            }
            Sender::Http2(mut sender) => {
                let sent =
                    tokio::time::timeout_at(deadline.into(), sender.try_send_request(outgoing));

                match sent.await {
                    Ok(Ok(response)) => return Ok((response, None)),
                    Ok(Err(error)) => (error, false),
                    Err(_) => return Err(Failure::Timeout),
                }
            }
        };

        // A connection kept between requests may have gone away unseen:
        // the request was not sent, so a fresh connection takes it.

        let mut sent = sent;
        let message = sent.take_message();
        let error = sent.into_error();

        match message {
            Some(message) if kept => {
                *slot = Some(message);
                continue;
            }
            Some(message) => {
                *slot = Some(message);
                return Err(Failure::Unsent(error.to_string()));
            }
            None => return Err(Failure::Sent(error.to_string())),
        }
    }
}

/// Probe each address in turn, forever: the task ends when the upstream
/// is dropped with its snapshot.
async fn run_probes(shared: Arc<Shared>, config: Probe) {
    loop {
        for address in &shared.addresses {
            let ok = tokio::time::timeout(config.timeout, probe_once(&shared, address, &config))
                .await
                .unwrap_or(false);
            address.probed(ok, &config, &shared.name);
        }

        tokio::time::sleep(config.interval).await;
    }
}

/// One probe: a connection that opens, and a 2xx or 3xx to the path
/// when one is configured, over a connection of the probe's own.
async fn probe_once(shared: &Shared, address: &Address, config: &Probe) -> bool {
    let io = match connect(shared, address).await {
        Ok(stream) => TokioIo::new(stream),
        Err(error) => {
            tracing::debug!(upstream = %shared.name, address = %address.text, %error, "probe could not connect");
            return false;
        }
    };

    let Some(path) = &config.path else {
        return true;
    };

    let host = address.host().unwrap_or_else(|| shared.name.clone());
    let request = Request::builder()
        .method(http::Method::GET)
        .uri(if shared.protocol.is_h2() {
            format!("{}://{host}{path}", scheme(shared))
        } else {
            path.clone()
        })
        .version(if shared.protocol.is_h2() {
            Version::HTTP_2
        } else {
            Version::HTTP_11
        })
        .header(HOST, host.as_str())
        .header(http::header::USER_AGENT, "muru-probe")
        .body(
            Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed_unsync(),
        );
    let Ok(request) = request else {
        return false;
    };

    let response = if shared.protocol.is_h2() {
        let Ok((mut sender, connection)) = http2::Builder::new(TokioExecutor::new())
            .handshake(io)
            .await
        else {
            return false;
        };

        tokio::spawn(connection);
        sender.send_request(request).await
    } else {
        let Ok((mut sender, connection)) = http1::Builder::new().handshake(io).await else {
            return false;
        };
        tokio::spawn(connection);
        sender.send_request(request).await
    };

    match response {
        Ok(response) => response.status().is_success() || response.status().is_redirection(),
        Err(error) => {
            tracing::debug!(upstream = %shared.name, address = %address.text, %error, "probe failed");
            false
        }
    }
}

fn scheme(shared: &Shared) -> &'static str {
    if shared.tls.is_some() {
        "https"
    } else {
        "http"
    }
}

/// The request body as it goes to the upstream: nothing, bytes the edge
/// holds, or the client's stream.
enum Payload {
    Empty,
    Buffered(Bytes),
    Streamed(Incoming),
}

impl Payload {
    fn into_body(self) -> OutBody {
        match self {
            Self::Empty => Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed_unsync(),
            Self::Buffered(data) => Full::new(data)
                .map_err(|never| match never {})
                .boxed_unsync(),
            Self::Streamed(incoming) => incoming.map_err(BoxError::from).boxed_unsync(),
        }
    }
}

/// Send a request to the upstream and answer with its response: a 502
/// `upstream_failed` when no address answers, 504 `deadline` when none
/// answers in time. An X-Muru-Redirect from a trusted upstream is an
/// internal redirect, as from an application.
pub async fn handle(
    parts: &mut http::request::Parts,
    mut body: Option<Incoming>,
    buffered: Option<Bytes>,
    upstream: Arc<Upstream>,
    context: &Context<'_>,
) -> Handled {
    let shared = &upstream.shared;
    let upgrade = crate::websocket::detect(parts);
    let limits = &context.limits;
    let recording = Recording::new(context, parts);
    let failing = link::failing(context, parts, Some(recording));

    // Section 4.13: strict normalisation. The message is re-serialised,
    // so a request whose framing is ambiguous never reaches the upstream.

    if parts.headers.contains_key(CONTENT_LENGTH) && parts.headers.contains_key(TRANSFER_ENCODING) {
        return Handled::Response(failing.close(Problem::new(
            StatusCode::BAD_REQUEST,
            "malformed",
            "The request carries both Content-Length and Transfer-Encoding.",
        )));
    }

    let content_length = match parts.headers.get(CONTENT_LENGTH) {
        None => None,
        Some(value) => match value
            .to_str()
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
        {
            Some(length) => Some(length),
            None => {
                return Handled::Response(failing.close(Problem::new(
                    StatusCode::BAD_REQUEST,
                    "malformed",
                    "The request's Content-Length is not a number.",
                )));
            }
        },
    };

    if content_length.is_some_and(|length| length > limits.max_body_size) {
        return Handled::Response(failing.too_large());
    }

    if upgrade.is_some() && shared.protocol.is_h2() {
        return Handled::Response(failing.respond(Problem::new(
            StatusCode::BAD_GATEWAY,
            FAILED_REASON,
            "WebSocket upgrades reach an upstream over HTTP/1.1 only.",
        )));
    }

    // Section 4.6: a body without a declared length is read in full
    // first, so the upstream sees a Content-Length. A sub-request brings
    // its body already buffered, or none.

    let mut payload = match buffered {
        Some(data) => Payload::Buffered(data),
        None => Payload::Empty,
    };

    if matches!(payload, Payload::Empty)
        && let Some(incoming) = body.as_mut()
        && !incoming.is_end_stream()
    {
        if content_length.is_none() {
            match link::read_body(incoming, limits.max_body_size).await {
                Ok(data) => payload = Payload::Buffered(data),
                Err(BodyError::TooLarge) => return Handled::Response(failing.too_large()),
                Err(BodyError::Read(error)) => {
                    tracing::debug!(%error, "reading the request body failed");
                    return Handled::Response(failing.close(Problem::new(
                        StatusCode::BAD_REQUEST,
                        "malformed",
                        "The request body could not be read.",
                    )));
                }
            }
        } else if let Some(incoming) = body.take() {
            payload = Payload::Streamed(incoming);
        }
    }

    let host = requested_host(parts, context);
    let headers = outgoing_headers(parts, context, &host, upgrade.is_some());

    let length = match &payload {
        Payload::Empty => None,
        Payload::Buffered(data) => Some(data.len() as u64),
        Payload::Streamed(_) => content_length,
    };

    // The deadline: what is left of the client's, or a sub-request's
    // budget within it (sections 4.12 and 4.16).

    let deadline = match context.handler {
        Some(budget) => (context.arrived + limits.request_deadline).min(Instant::now() + budget),
        None => context.arrived + limits.request_deadline,
    };

    // The addresses in turn from where the last request left off: those
    // in service first, then one that is due a trial after an ejection,
    // and when nothing is in service every address, which is better than
    // answering nothing.

    let count = shared.addresses.len();
    let start = shared.next.fetch_add(1, Ordering::Relaxed);
    let now = Instant::now();
    let in_order: Vec<&Arc<Address>> = (0..count)
        .map(|offset| &shared.addresses[(start + offset) % count])
        .collect();
    let mut candidates: Vec<&Arc<Address>> = in_order
        .iter()
        .copied()
        .filter(|address| address.admission(now) == Admission::Open)
        .collect();

    let trials = candidates.is_empty();

    if trials {
        candidates = in_order
            .iter()
            .copied()
            .filter(|address| address.admission(now) == Admission::Trial)
            .collect();
    }

    if candidates.is_empty() {
        candidates = in_order.clone();
    }

    let dispatched = Instant::now();
    let mut slot = Some(build_request(
        parts,
        shared,
        &host,
        headers,
        length,
        payload.into_body(),
    ));
    let mut last_error = String::new();
    let mut answered = None;

    // A request that was never sent stays in the slot for the next
    // address; one that went out in part cannot be sent again.

    for address in candidates {
        if slot.is_none() {
            break;
        }

        // An address due a trial takes one request at a time; the claim
        // is made here, so an address never tried keeps none.

        if trials && !address.claim_trial() {
            continue;
        }

        match send(shared, address, &mut slot, deadline).await {
            Ok((response, sender)) => {
                address.succeeded(&shared.name);
                answered = Some((address, response, sender));
                break;
            }
            Err(Failure::Unsent(error)) => {
                tracing::warn!(
                    upstream = %shared.name,
                    address = %address.text,
                    "upstream request could not be sent: {error}"
                );
                address.failed(&shared.name, shared.eject_after, shared.eject_for);
                last_error = error;
            }
            Err(Failure::Sent(error)) => {
                tracing::warn!(
                    upstream = %shared.name,
                    address = %address.text,
                    "upstream request failed: {error}"
                );
                address.failed(&shared.name, shared.eject_after, shared.eject_for);
                return Handled::Response(failing.respond(Problem::new(
                    StatusCode::BAD_GATEWAY,
                    FAILED_REASON,
                    "The upstream did not answer the request.",
                )));
            }
            Err(Failure::Timeout) => {
                address.failed(&shared.name, shared.eject_after, shared.eject_for);
                return Handled::Response(failing.respond(Problem::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "deadline",
                    "The upstream did not respond within the request deadline.",
                )));
            }
        }
    }

    let Some((address, mut response, sender)) = answered else {
        return Handled::Response(failing.respond(Problem::new(
            StatusCode::BAD_GATEWAY,
            FAILED_REASON,
            if last_error.is_empty() {
                "No address of the upstream could be reached.".to_owned()
            } else {
                format!("No address of the upstream could be reached: {last_error}.")
            },
        )));
    };

    let status = response.status();
    let mut recording = failing.take_recording();

    if let Some(recording) = recording.as_mut() {
        recording.record.application = dispatched.elapsed();
        recording.record.status = status.as_u16();
        recording.record.mount = address.text.clone();
    }

    // Section 4.13: X-Muru headers from an upstream mean nothing unless
    // the operator trusts it; a trusted redirect goes through routing
    // again as from an application.

    if !shared.trust_muru_headers {
        let muru: Vec<HeaderName> = response
            .headers()
            .keys()
            .filter(|name| name.as_str().starts_with("x-muru-"))
            .cloned()
            .collect();

        for name in muru {
            response.headers_mut().remove(name);
        }
    }

    let head = worker::ResponseHead {
        status: u32::from(status.as_u16()),
        headers: headers_message(response.headers()),
        ..Default::default()
    };
    let response_end = response.body().is_end_stream();

    if upgrade.is_none()
        && shared.trust_muru_headers
        && let Some(redirect) =
            link::redirect_from(&head, response_end, RedirectFlags::default(), None)
    {
        if let Some(recording) = recording.take() {
            recording.hop(status.as_u16());
        }

        drop(response);
        return Handled::Redirect(Box::new(redirect));
    }

    if upgrade.is_none()
        && context.handler.is_none()
        && let Some(problem) =
            link::override_from(&head, response_end, failing.negotiation(), &context.errors)
    {
        failing.put_recording(recording.take());
        drop(response);
        return Handled::Response(failing.respond(problem));
    }

    // Section 4.9: a trusted upstream's file hint by path is served by
    // the edge like an application's.

    if upgrade.is_none()
        && shared.trust_muru_headers
        && let Some(path) = link::sendfile_from(&head, &context.sendfile)
    {
        drop(response);
        return Handled::Response(
            link::serve_hint(&path, parts, context, &head, &failing, recording.take()).await,
        );
    }

    // The upstream accepted the WebSocket: the connection is handed over
    // on both sides and bytes are copied between them.

    if let Some(upgrade) = upgrade
        && status == StatusCode::SWITCHING_PROTOCOLS
    {
        let upstream_side = hyper::upgrade::on(&mut response);
        let answer = crate::websocket::accepted_response(&upgrade, head.headers);
        tokio::spawn(relay(upgrade, upstream_side, recording));
        return Handled::Response(answer);
    }

    let mut builder = Response::builder().status(status);

    if let Some(headers) = builder.headers_mut() {
        let named = connection_options(response.headers());
        let kept: Vec<worker::Header> = head
            .headers
            .into_iter()
            .filter(|header| {
                let name = String::from_utf8_lossy(&header.name);
                !crate::is_hop_by_hop(&name, &named)
            })
            .collect();
        crate::copy_headers(headers, kept);
    }

    let body = response.into_body();

    let response = if response_end {
        if let Some(sender) = sender {
            keep(address, sender);
        }

        if let Some(recording) = recording.take() {
            recording.complete(status.as_u16(), "", 0);
        }

        builder.body(
            Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
    } else {
        let stream = futures::stream::unfold(
            BodyState {
                body: Some(body),
                address: Arc::clone(address),
                sender,
                recording,
                status: status.as_u16(),
                bytes: 0,
                idle_timeout: limits.response_idle_timeout,
            },
            next_chunk,
        );
        builder.body(UnsyncBoxBody::new(StreamBody::new(stream)))
    };

    match response {
        Ok(response) => Handled::Response(response),
        Err(_) => Handled::Response(failing.respond(Problem::new(
            StatusCode::BAD_GATEWAY,
            FAILED_REASON,
            "The upstream sent an invalid response head.",
        ))),
    }
}

/// Copy bytes between the client and the upstream for the life of a
/// WebSocket the upstream accepted.
async fn relay(
    upgrade: crate::websocket::Upgrade,
    upstream_side: hyper::upgrade::OnUpgrade,
    recording: Option<Recording>,
) {
    let (client, server) = match (upgrade.into_on_upgrade().await, upstream_side.await) {
        (Ok(client), Ok(server)) => (client, server),
        (Err(error), _) | (_, Err(error)) => {
            tracing::debug!(%error, "the WebSocket handover failed");

            if let Some(recording) = recording {
                recording.complete(101, "", 0);
            }

            return;
        }
    };

    let mut client = TokioIo::new(client);
    let mut server = TokioIo::new(server);
    let copied = tokio::io::copy_bidirectional(&mut client, &mut server)
        .await
        .map_or(0, |(_, from_server)| from_server);

    if let Some(recording) = recording {
        recording.complete(101, "", copied);
    }
}

/// The host the upstream is told about: what a trusted proxy forwarded,
/// else what the client asked for.
fn requested_host(parts: &http::request::Parts, context: &Context<'_>) -> String {
    context
        .forwarded_host
        .clone()
        .or_else(|| {
            parts
                .headers
                .get(HOST)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })
        .or_else(|| parts.uri.authority().map(|authority| authority.to_string()))
        .unwrap_or_default()
}

/// The request headers as the upstream gets them: the client's, less
/// hop-by-hop ones, names with underscores and X-Muru headers, with the
/// forwarding headers and the request identifier added.
fn outgoing_headers(
    parts: &http::request::Parts,
    context: &Context<'_>,
    host: &str,
    upgrade: bool,
) -> HeaderMap {
    let named = connection_options(&parts.headers);
    let mut headers = HeaderMap::with_capacity(parts.headers.len() + 6);

    for (name, value) in &parts.headers {
        let text = name.as_str();
        let kept_for_upgrade = upgrade && (name == CONNECTION || name == UPGRADE);

        if (crate::is_hop_by_hop(text, &named) && !kept_for_upgrade)
            || text.contains('_')
            || text.starts_with("x-muru-")
            || name == HOST
            || name == CONTENT_LENGTH
            || name == EXPECT
        {
            continue;
        }

        headers.append(name.clone(), value.clone());
    }

    if let Ok(value) = HeaderValue::from_str(host) {
        headers.insert(HOST, value);
    }

    let client = context.peer.ip().to_string();
    let forwarded_for = match headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
    {
        Some(existing) => format!("{existing}, {client}"),
        None => client.clone(),
    };

    for (name, value) in [
        ("x-forwarded-for", forwarded_for.as_str()),
        ("x-forwarded-proto", context.scheme),
        ("x-real-ip", client.as_str()),
        ("x-request-id", context.request_id),
    ] {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    }

    if !headers.contains_key("x-forwarded-host")
        && let Ok(value) = HeaderValue::from_str(host)
    {
        headers.insert(HeaderName::from_static("x-forwarded-host"), value);
    }

    headers
}

/// The request as sent: origin form over HTTP/1.1, the full URI with
/// the host as its authority over HTTP/2.
fn build_request(
    parts: &http::request::Parts,
    shared: &Shared,
    host: &str,
    mut headers: HeaderMap,
    length: Option<u64>,
    body: OutBody,
) -> Request<OutBody> {
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), |target| target.to_string());

    let (uri, version) = if shared.protocol.is_h2() {
        headers.remove(HOST);
        (
            format!("{}://{host}{target}", scheme(shared))
                .parse()
                .unwrap_or_else(|_| Uri::from_static("/")),
            Version::HTTP_2,
        )
    } else {
        (
            target.parse().unwrap_or_else(|_| Uri::from_static("/")),
            Version::HTTP_11,
        )
    };

    if let Some(length) = length {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    }

    let mut request = Request::new(body);
    *request.method_mut() = parts.method.clone();
    *request.uri_mut() = uri;
    *request.version_mut() = version;
    *request.headers_mut() = headers;
    request
}

/// The headers a Connection header lists as hop-by-hop, lower-cased.
fn connection_options(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|option| option.trim().to_ascii_lowercase())
        .filter(|option| !option.is_empty())
        .collect()
}

/// Headers in the worker message form, which the redirect and override
/// checks read.
fn headers_message(headers: &HeaderMap) -> Vec<worker::Header> {
    headers
        .iter()
        .map(|(name, value)| worker::Header {
            name: Bytes::copy_from_slice(name.as_str().as_bytes()),
            value: Bytes::copy_from_slice(value.as_bytes()),
        })
        .collect()
}

/// The response body in flight from the upstream.
struct BodyState {
    body: Option<Incoming>,
    address: Arc<Address>,
    /// The HTTP/1.1 connection, kept for the next request once the body
    /// has arrived whole.
    sender: Option<http1::SendRequest<OutBody>>,
    recording: Option<Recording>,
    status: u16,
    bytes: u64,
    idle_timeout: Duration,
}

impl BodyState {
    /// The body is done: the connection goes back for the next request
    /// when the body arrived whole, and the request is logged with
    /// `reason` when the edge cut it short.
    fn finish(&mut self, intact: bool, reason: &str) {
        self.body = None;

        if let Some(sender) = self.sender.take()
            && intact
        {
            keep(&self.address, sender);
        }

        if let Some(recording) = self.recording.take() {
            recording.complete(self.status, reason, self.bytes);
        }
    }
}

impl Drop for BodyState {
    /// hyper drops a body once the declared length is written, before the
    /// stream has been polled to its end; the upstream's body is whole by
    /// then when it declared the same length.
    fn drop(&mut self) {
        let intact = self.body.as_ref().is_some_and(|body| body.is_end_stream());

        if self.body.is_some() {
            self.finish(intact, "");
        }
    }
}

async fn next_chunk(mut state: BodyState) -> Option<(Result<Frame<Bytes>, BoxError>, BodyState)> {
    let body = state.body.as_mut()?;

    let next = match tokio::time::timeout(state.idle_timeout, body.frame()).await {
        Ok(next) => next,
        Err(_) => {
            state.finish(false, FAILED_REASON);
            return Some((
                Err(BoxError::from("the upstream stopped sending the response")),
                state,
            ));
        }
    };

    match next {
        Some(Ok(frame)) => {
            if let Some(data) = frame.data_ref() {
                state.bytes += data.len() as u64;
            }

            Some((Ok(frame), state))
        }
        Some(Err(error)) => {
            state.finish(false, FAILED_REASON);
            Some((Err(BoxError::from(error)), state))
        }
        None => {
            state.finish(true, "");
            None
        }
    }
}
