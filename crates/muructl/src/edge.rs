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
    /// The file descriptor of the bound, listening HTTP socket.
    #[arg(long, value_name = "FD")]
    listener_fd: i32,

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

    /// A timeout as NAME=DURATION: queue, deadline, header_read, body_idle
    /// or response_idle.
    #[arg(long = "timeout", value_name = "NAME=DURATION", value_parser = crate::parse_timeout)]
    timeouts: Vec<(String, Duration)>,

    /// A worker socket to serve from the start, before any snapshot.
    #[arg(long = "worker-socket", value_name = "PATH")]
    worker_sockets: Vec<PathBuf>,

    /// A Unix socket on which the listener is handed to whoever connects:
    /// a daemon that restarted without it (section 2.3).
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

        for (name, duration) in &self.timeouts {
            match name.as_str() {
                "queue" => limits.queue_timeout = *duration,
                "deadline" => limits.request_deadline = *duration,
                "header_read" => limits.header_read_timeout = *duration,
                "body_idle" => limits.body_idle_timeout = *duration,
                "response_idle" => limits.response_idle_timeout = *duration,
                _ => {}
            }
        }
        limits
    }
}

/// Run the edge until the daemon tells it to stop or the listener fails.
pub async fn run(args: EdgeRunArgs) -> anyhow::Result<()> {
    // The listener was bound by the daemon and inherited across exec.
    // SAFETY: the daemon passed this descriptor for the edge to own, and
    // nothing else in this process refers to it.

    let listener = unsafe { std::net::TcpListener::from_raw_fd(args.listener_fd) };
    let listen = listener
        .local_addr()
        .context("the inherited listener has no address")?;
    let handing_back = match &args.listener_socket {
        Some(path) => Some(hand_back_listener(path, listener.try_clone()?)?),
        None => None,
    };
    let access_log = muru_edge::access::Config {
        destination: match args.access_log.as_str() {
            "off" => muru_edge::access::Destination::Off,
            path => muru_edge::access::Destination::File(PathBuf::from(path)),
        },
        ..muru_edge::access::Config::default()
    };

    let edge = muru_edge::Edge::adopt(
        muru_edge::Config {
            listen,
            sockets: args.worker_sockets.clone(),
            development: false,
            access_log,
            limits: args.limits(),
            https: None,
        },
        listener,
    )
    .await?;
    let pools = edge.pools();
    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel::<()>(1);

    // The control socket, on which the daemon pushes snapshots.

    let _ = std::fs::remove_file(&args.control);
    let control = tokio::net::UnixListener::bind(&args.control)
        .with_context(|| format!("binding the control socket {}", args.control.display()))?;
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(control);
    let service = EdgeControlServer::new(Control {
        pools,
        listen,
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

    let mut serving = tokio::spawn(edge.run());
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    eprintln!("muru: edge serving {listen} (pid {})", std::process::id());
    let outcome = tokio::select! {
        served = &mut serving => served.map_err(anyhow::Error::from).and_then(|result| result),
        served = &mut api => served.map_err(anyhow::Error::from).and_then(|result| result),
        _ = stop_rx.recv() => Ok(()),
        _ = terminate.recv() => Ok(()),
    };

    let _ = std::fs::remove_file(&args.control);
    if let Some(path) = &args.listener_socket {
        let _ = std::fs::remove_file(path);
    }
    drop(handing_back);
    outcome
}

/// Serve the listener-return socket: each connection receives a copy of
/// the listening socket over SCM_RIGHTS.
fn hand_back_listener(
    path: &std::path::Path,
    listener: std::net::TcpListener,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let _ = std::fs::remove_file(path);
    let socket = std::os::unix::net::UnixListener::bind(path)
        .with_context(|| format!("binding the listener socket {}", path.display()))?;

    Ok(std::thread::spawn(move || {
        for connection in socket.incoming().flatten() {
            let _ = muru_controller::fdpass::send_fd(&connection, listener.as_raw_fd());
        }
    }))
}

/// The control service: snapshots in, status out.
struct Control {
    pools: Arc<muru_edge::Pools>,
    listen: SocketAddr,
    version: AtomicU64,
    stop: tokio::sync::mpsc::Sender<()>,
}

#[tonic::async_trait]
impl EdgeControl for Control {
    async fn apply(&self, request: Request<pb::Snapshot>) -> Result<Response<pb::Applied>, Status> {
        let snapshot = request.into_inner();
        let sockets: Vec<PathBuf> = snapshot.worker_sockets.iter().map(PathBuf::from).collect();

        // The new pool is connected before it replaces the old one, so a
        // snapshot that cannot be reached leaves the last good one serving.

        let pool = muru_edge::link::LinkPool::connect(&sockets)
            .await
            .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;
        self.pools.replace(pool);
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
            listen: self.listen.to_string(),
            snapshot_version: self.version.load(Ordering::Relaxed),
        }))
    }

    async fn stop(&self, _: Request<pb::StopRequest>) -> Result<Response<pb::Stopped>, Status> {
        let _ = self.stop.try_send(());
        Ok(Response::new(pb::Stopped {}))
    }
}
