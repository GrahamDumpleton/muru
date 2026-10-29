//! The edge process (design specification sections 2.3 and 4.4): spawned
//! by the daemon from its own executable with the listening socket already
//! bound and handed down, so an edge can be replaced without refusing a
//! connection. The daemon pushes routing snapshots over the edge's control
//! socket; the edge keeps the last good one.

use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use clap::{Args, Subcommand};
use muru_proto::edge::v1 as pb;
use muru_proto::edge::v1::edge_control_server::{EdgeControl, EdgeControlServer};
use tonic::{Request, Response, Status};

/// The edge subcommands.
#[derive(Subcommand)]
pub enum EdgeCommand {
    /// Run an edge process on a listener inherited from the daemon.
    Run(Box<EdgeRunArgs>),
}

/// What the daemon starts an edge with.
#[derive(Args)]
pub struct EdgeRunArgs {
    /// A bound, listening socket as its file descriptor, with :tls when
    /// the edge terminates TLS on it. The first is the main listener.
    #[arg(long = "listener", value_name = "FD[:tls]", required = true, value_parser = parse_listener)]
    listeners: Vec<(i32, bool)>,

    /// The Unix socket to serve the control protocol on.
    #[arg(long, value_name = "PATH")]
    control: PathBuf,

    /// Where the access log goes: a file path, or off.
    #[arg(long, value_name = "PATH", default_value = "off")]
    access_log: String,

    /// Largest request body accepted, in bytes.
    #[arg(long, value_name = "BYTES")]
    max_body_size: Option<u64>,

    /// Largest request head accepted, in bytes.
    #[arg(long, value_name = "BYTES")]
    max_header_bytes: Option<usize>,

    /// Most headers accepted in a request.
    #[arg(long, value_name = "N")]
    max_headers: Option<usize>,

    /// A timeout as NAME=DURATION: queue, deadline, header_read, body_idle,
    /// response_idle, keep_alive or keep_alive_proxy.
    #[arg(long = "timeout", value_name = "NAME=DURATION", value_parser = crate::parse_timeout)]
    timeouts: Vec<(String, Duration)>,

    /// Open connections per listener.
    #[arg(long, value_name = "N")]
    max_connections: Option<usize>,

    /// Open connections per client address; 0 disables it.
    #[arg(long, value_name = "N")]
    max_connections_per_client: Option<usize>,

    /// Requests served on one connection before it is closed; 0 for no
    /// limit.
    #[arg(long, value_name = "N")]
    max_requests_per_connection: Option<u64>,

    /// Concurrent streams per HTTP/2 connection.
    #[arg(long, value_name = "N")]
    http2_max_concurrent_streams: Option<u32>,

    /// A worker socket to serve from the start, before any snapshot.
    #[arg(long = "worker-socket", value_name = "PATH")]
    worker_sockets: Vec<PathBuf>,

    /// Where upload routes store files.
    #[arg(long, value_name = "PATH")]
    spool: Option<PathBuf>,

    /// What stored uploads may hold at once, in bytes; 0 for no limit.
    #[arg(long, value_name = "BYTES")]
    spool_quota: Option<u64>,

    /// The rate limits' buckets, shared with the node's other edge
    /// processes; this process's own when absent.
    #[arg(long, value_name = "PATH")]
    rate_buckets: Option<PathBuf>,

    /// A Unix socket on which the listener is handed to whoever connects:
    /// a daemon that restarted without it.
    #[arg(long, value_name = "PATH")]
    listener_socket: Option<PathBuf>,
}

impl EdgeRunArgs {
    fn limits(&self) -> muru_edge::Limits {
        let mut limits = muru_edge::Limits::default();

        if let Some(size) = self.max_body_size {
            limits.max_body_size = size;
        }

        if let Some(size) = self.max_header_bytes {
            limits.max_header_bytes = size;
        }

        if let Some(count) = self.max_headers {
            limits.max_headers = count;
        }

        if let Some(count) = self.max_connections {
            limits.max_connections = count;
        }

        if let Some(count) = self.max_connections_per_client {
            limits.max_connections_per_client = count;
        }

        if let Some(count) = self.max_requests_per_connection {
            limits.max_requests_per_connection = count;
        }

        if let Some(count) = self.http2_max_concurrent_streams {
            limits.http2_max_concurrent_streams = count;
        }

        if let Some(bytes) = self.spool_quota {
            limits.spool_quota = bytes;
        }

        crate::apply_timeouts(&mut limits, &self.timeouts);
        limits
    }
}

/// Parse FD or FD:tls.
fn parse_listener(text: &str) -> Result<(i32, bool), String> {
    let (fd, tls) = match text.split_once(':') {
        Some((fd, "tls")) => (fd, true),
        Some((_, other)) => return Err(format!("{other:?} is not tls")),
        None => (text, false),
    };

    let fd: i32 = fd
        .parse()
        .map_err(|_| format!("{text:?} is not a file descriptor number"))?;

    Ok((fd, tls))
}

/// Run the edge until the daemon tells it to stop or a listener fails.
pub async fn run(args: EdgeRunArgs) -> anyhow::Result<()> {
    // The listeners were bound by the daemon and inherited across exec.

    let mut inherited = Vec::new();

    for (fd, tls) in &args.listeners {
        // SAFETY: the daemon passed this descriptor for the edge to own,
        // and nothing else in this process refers to it.

        let listener = unsafe { std::net::TcpListener::from_raw_fd(*fd) };
        listener
            .local_addr()
            .with_context(|| format!("the inherited listener {fd} has no address"))?;
        inherited.push(muru_edge::Inherited {
            listener,
            tls: *tls,
        });
    }

    let listen = inherited[0].listener.local_addr()?;

    let handing_back = match &args.listener_socket {
        Some(path) => {
            let copies = inherited
                .iter()
                .map(|inherited| inherited.listener.try_clone())
                .collect::<Result<Vec<_>, _>>()?;
            Some(hand_back_listeners(path, copies)?)
        }
        None => None,
    };

    // Section 4.11: without a usable spool, upload routes answer 507.

    let limits = args.limits();
    let spool = args.spool.as_ref().and_then(|path| {
        let quota = (limits.spool_quota > 0).then_some(limits.spool_quota);

        muru_edge::uploads::Spool::new(path.clone(), quota)
            .map_err(|error| {
                eprintln!(
                    "muru: the upload spool {} is unusable: {error}",
                    path.display()
                );
            })
            .ok()
    });

    let access_log = muru_edge::access::Config {
        destination: match args.access_log.as_str() {
            "off" => muru_edge::access::Destination::Off,
            path => muru_edge::access::Destination::File(PathBuf::from(path)),
        },
        ..muru_edge::access::Config::default()
    };

    let resolver = Arc::new(muru_edge::tls::Resolver::default());
    let edge = muru_edge::Edge::adopt(
        muru_edge::Config {
            listen,
            app: String::new(),
            release: String::new(),
            sockets: args.worker_sockets.clone(),
            groups: Vec::new(),
            mounts: Vec::new(),
            static_dirs: Vec::new(),
            accel_redirect: false,
            cgi_local_redirect: false,
            sendfile: Arc::default(),
            grpc_web_origins: Vec::new(),
            errors: Arc::new(muru_edge::errors::ErrorPolicy::default()),
            development: false,
            access_log,
            limits,
            compression: muru_edge::compression::Compression::default(),
            trust: muru_edge::proxy::Trust::default(),
            https: None,
            spool,
            rate_buckets: args.rate_buckets.clone(),
        },
        inherited,
        Arc::clone(&resolver),
    )
    .await?;
    let pools = edge.pools();
    let listeners = edge.listeners();
    let connections = edge.connections();
    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel::<()>(1);

    // The control socket, on which the daemon pushes snapshots.

    let _ = std::fs::remove_file(&args.control);
    let control = tokio::net::UnixListener::bind(&args.control)
        .with_context(|| format!("binding the control socket {}", args.control.display()))?;
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(control);
    let service = EdgeControlServer::new(Control {
        pools,
        resolver,
        listeners,
        access_log: edge.access_log(),
        connections,
        version: AtomicU64::new(0),
        stop: stop_tx,
    });
    let mut api = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
            .map_err(anyhow::Error::from)
    });

    // Section 11.3: SIGUSR1 reopens the access log after logrotate has
    // renamed it.

    let access_log = edge.access_log();
    let mut reopen = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    tokio::spawn(async move {
        while reopen.recv().await.is_some() {
            match access_log.reopen() {
                Ok(()) => eprintln!("muru: edge reopened the access log"),
                Err(error) => eprintln!("muru: edge could not reopen the access log: {error}"),
            }
        }
    });

    let drainer = edge.drainer();
    let grace = args.limits().request_deadline;
    let mut serving = tokio::spawn(edge.run());
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    eprintln!("muru: edge serving {listen} (pid {})", std::process::id());
    let outcome = tokio::select! {
        served = &mut serving => served.map_err(anyhow::Error::from).and_then(|result| result),
        served = &mut api => served.map_err(anyhow::Error::from).and_then(|result| result),
        _ = stop_rx.recv() => Ok(()),
        _ = terminate.recv() => Ok(()),
    };

    // Section 13.4: told to stop, the edge stops accepting, lets the
    // requests in flight finish and closes idle connections, then exits.
    // Whatever waits in a listener's backlog is accepted by the daemon's
    // next edge, which holds the same sockets.

    if outcome.is_ok() {
        eprintln!("muru: edge draining (pid {})", std::process::id());

        if !drainer.drain(grace).await {
            eprintln!(
                "muru: edge exiting with connections still open after {} seconds",
                grace.as_secs()
            );
        }
    }

    let _ = std::fs::remove_file(&args.control);

    if let Some(path) = &args.listener_socket {
        let _ = std::fs::remove_file(path);
    }

    drop(handing_back);
    outcome
}

/// Serve the listener-return socket: each connection receives a copy of
/// every listening socket over SCM_RIGHTS, in order, then the connection
/// closes.
fn hand_back_listeners(
    path: &std::path::Path,
    listeners: Vec<std::net::TcpListener>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let _ = std::fs::remove_file(path);
    let socket = std::os::unix::net::UnixListener::bind(path)
        .with_context(|| format!("binding the listener socket {}", path.display()))?;

    Ok(std::thread::spawn(move || {
        for connection in socket.incoming().flatten() {
            for listener in &listeners {
                if muru_controller::fdpass::send_fd(&connection, listener.as_raw_fd()).is_err() {
                    break;
                }
            }
        }
    }))
}

/// The control service: snapshots in, status out.
struct Control {
    pools: Arc<muru_edge::Pools>,
    resolver: Arc<muru_edge::tls::Resolver>,
    listeners: Vec<(SocketAddr, bool)>,
    access_log: Arc<muru_edge::access::AccessLog>,
    connections: muru_edge::ConnectionCounts,
    version: AtomicU64,
    stop: tokio::sync::mpsc::Sender<()>,
}

#[tonic::async_trait]
impl EdgeControl for Control {
    async fn apply(&self, request: Request<pb::Snapshot>) -> Result<Response<pb::Applied>, Status> {
        let snapshot = request.into_inner();

        // The tables are built and every worker connected before anything
        // replaces the last good snapshot, and so are the certificates.

        let routing = muru_edge::routing::Routing::from_snapshot(&snapshot)
            .await
            .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;
        let mut certificates = Vec::new();

        for host in &snapshot.hosts {
            if host.certificate_pem.is_empty() {
                continue;
            }

            let (chain, key) = muru_edge::tls::parse_pem(&host.certificate_pem, &host.key_pem)
                .map_err(|error| {
                    Status::failed_precondition(format!("host {}: {error:#}", host.name))
                })?;
            let key = muru_edge::tls::certified_key(chain, key).map_err(|error| {
                Status::failed_precondition(format!("host {}: {error:#}", host.name))
            })?;
            certificates.push(muru_edge::tls::HostCertificate {
                names: host.names.clone(),
                key,
                default: host.default,
            });
        }

        self.resolver.replace(certificates);
        self.pools.apply(routing);
        self.version.store(snapshot.version, Ordering::Relaxed);
        Ok(Response::new(pb::Applied {
            version: snapshot.version,
        }))
    }

    async fn get_status(
        &self,
        _: Request<pb::EdgeStatusRequest>,
    ) -> Result<Response<pb::EdgeStatus>, Status> {
        Ok(Response::new(pb::EdgeStatus {
            pid: std::process::id(),
            listen: self.listeners[0].0.to_string(),
            snapshot_version: self.version.load(Ordering::Relaxed),
            listeners: self
                .listeners
                .iter()
                .map(|(address, tls)| pb::Listener {
                    address: address.to_string(),
                    tls: *tls,
                })
                .collect(),
        }))
    }

    async fn get_metrics(
        &self,
        _: Request<pb::MetricsRequest>,
    ) -> Result<Response<pb::MetricsReport>, Status> {
        let routing = self.pools.current();
        let samples = self
            .access_log
            .metrics()
            .samples()
            .into_iter()
            .chain(muru_edge::metrics::pool_samples(&routing))
            .chain(self.connections.samples())
            .map(|sample| sample.to_message())
            .collect();
        Ok(Response::new(pb::MetricsReport { samples }))
    }

    async fn get_recent(
        &self,
        request: Request<pb::RecentRequest>,
    ) -> Result<Response<pb::RecentRequests>, Status> {
        let limit = request.into_inner().limit.max(1) as usize;
        let minute = self.access_log.last_minute();
        let routing = self.pools.current();
        let mut pools = Vec::new();
        let websockets = self.access_log.websocket_counts();

        for app_pools in routing.app_pools() {
            let (app, release) = app_pools.labels();

            for (group, pool) in app_pools.pools() {
                let workers = pool.workers();
                let open = websockets
                    .get(&(app.to_owned(), release.to_owned(), group.clone()))
                    .copied()
                    .unwrap_or(0);
                pools.push(pb::PoolState {
                    app: app.to_owned(),
                    release: release.to_owned(),
                    group,
                    workers: workers.len() as u32,
                    slots: workers.iter().map(|worker| worker.slots as u32).sum(),
                    busy: workers.iter().map(|worker| worker.in_flight as u32).sum(),
                    waiting: pool.waiting() as u32,
                    websockets: open as u32,
                });
            }
        }

        let records = self
            .access_log
            .recent(limit)
            .into_iter()
            .map(|record| pb::AccessRecord {
                started_unix_ms: record
                    .started
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_millis() as u64),
                request_id: record.request_id,
                client: record.client.to_string(),
                app: record.app,
                release: record.release,
                host: record.host,
                group: record.group,
                mount: record.mount,
                method: record.method,
                target: record.target,
                status: u32::from(record.status),
                reason: record.reason,
                bytes: record.bytes,
                worker: record.worker.map_or(-1, |worker| worker as i32),
                queue_us: record.queue.as_micros() as u64,
                application_us: record.application.as_micros() as u64,
                total_us: record.total.as_micros() as u64,
                websocket_close: record
                    .websocket
                    .map(|end| format!("closed by {}, code {}", end.by, end.code))
                    .unwrap_or_default(),
            })
            .collect();
        Ok(Response::new(pb::RecentRequests {
            records,
            minute_requests: minute.requests,
            minute_errors: minute.errors,
            minute_seconds: minute.seconds,
            pools,
        }))
    }

    async fn get_web_sockets(
        &self,
        request: Request<pb::WebSocketsRequest>,
    ) -> Result<Response<pb::WebSockets>, Status> {
        let request = request.into_inner();
        Ok(Response::new(muru_edge::endpoints::report(
            &self.access_log,
            &request.app,
            request.limit as usize,
        )))
    }

    async fn stop(&self, _: Request<pb::StopRequest>) -> Result<Response<pb::Stopped>, Status> {
        let _ = self.stop.try_send(());
        Ok(Response::new(pb::Stopped {}))
    }

    type WatchWakesStream = tokio_stream::wrappers::ReceiverStream<Result<pb::Wake, Status>>;

    async fn watch_wakes(
        &self,
        _: Request<pb::WatchWakesRequest>,
    ) -> Result<Response<Self::WatchWakesStream>, Status> {
        let mut wakes = muru_edge::wake::wakes().subscribe();
        let (sender, receiver) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            loop {
                let received = tokio::select! {
                    () = sender.closed() => return,
                    received = wakes.recv() => received,
                };

                let (app, release, group) = match received {
                    Ok(key) => key,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };

                let wake = pb::Wake {
                    app,
                    release,
                    group,
                };

                if sender.send(Ok(wake)).await.is_err() {
                    return;
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            receiver,
        )))
    }
}
