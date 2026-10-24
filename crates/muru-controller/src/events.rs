//! The event stream (design specification section 11.5): every state
//! change is stored and broadcast to watchers.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::model::Event;
use crate::store::Store;

/// Records events in the store and hands them to watchers.
#[derive(Clone)]
pub struct Events {
    store: Arc<Store>,
    sender: broadcast::Sender<Event>,
}

impl Events {
    /// Events are kept for watchers up to this far behind.
    const BACKLOG: usize = 256;

    /// Create the stream over a store.
    pub fn new(store: Arc<Store>) -> Self {
        let (sender, _) = broadcast::channel(Self::BACKLOG);
        Self { store, sender }
    }

    /// Record an event and tell the watchers. A store failure is logged,
    /// since an event must never stop the action it describes.
    pub fn emit(&self, kind: &str, subject: &str, level: &'static str, message: &str) {
        match self.store.append_event(kind, subject, level, message) {
            Ok(event) => {
                match level {
                    "error" => tracing::error!(%subject, "{message}"),
                    "warning" => tracing::warn!(%subject, "{message}"),
                    _ => tracing::info!(%subject, "{message}"),
                }
                let _ = self.sender.send(event);
            }
            Err(error) => tracing::error!(%error, "recording an event failed"),
        }
    }

    /// Watch for new events from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}
