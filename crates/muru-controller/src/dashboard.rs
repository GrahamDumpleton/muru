//! The daemon's dashboard and JSON gateway (design specification section
//! 10.7), served on an internal address from the daemon's own data: each
//! request asks the reconciler for a fresh picture of the node.

use std::sync::Arc;

use muru_dashboard::{NodeMetricsSource, NodeSource};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::daemon::Command;

/// Serve the dashboard on the listener until accepting fails.
pub async fn serve(listener: TcpListener, commands: mpsc::Sender<Command>) -> anyhow::Result<()> {
    let data_commands = commands.clone();
    let source: NodeSource = Arc::new(move || {
        let commands = data_commands.clone();
        Box::pin(async move {
            let (done_tx, done_rx) = oneshot::channel();
            commands
                .send(Command::Dashboard { done: done_tx })
                .await
                .ok()?;
            done_rx.await.ok()
        })
    });
    let metrics: NodeMetricsSource = Arc::new(move || {
        let commands = commands.clone();
        Box::pin(async move {
            let (done_tx, done_rx) = oneshot::channel();
            commands
                .send(Command::Metrics { done: done_tx })
                .await
                .ok()?;
            done_rx.await.ok()
        })
    });
    muru_dashboard::serve_node(listener, source, metrics).await
}
