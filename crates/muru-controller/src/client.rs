//! The admin API client, for muructl: a gRPC channel over the daemon's
//! Unix socket.

use std::path::{Path, PathBuf};

use hyper_util::rt::TokioIo;
use muru_proto::admin::v1::admin_client::AdminClient;
use tonic::transport::{Channel, Endpoint, Uri};

/// Connect to the daemon's admin socket.
pub async fn connect(socket: &Path) -> anyhow::Result<AdminClient<Channel>> {
    let socket: PathBuf = socket.to_path_buf();

    if !socket.exists() {
        anyhow::bail!(
            "no daemon is running: {} does not exist (muructl daemon start, or muructl up)",
            socket.display()
        );
    }

    // The URI is required by the channel but never used: the connector
    // opens the Unix socket whatever it says.

    let endpoint = Endpoint::try_from("http://muru.invalid")?;
    let channel = endpoint
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await?;

    Ok(AdminClient::new(channel))
}
