//! The Muru controller: the state store, the reconciler that drives
//! releases toward desired state, the admin API, and the hosts it
//! supervises (design specification sections 2, 9, 10 and 13).
//!
//! Built as a library so the same code runs embedded in `muructl run` and
//! in the daemon (ADR 0002).

pub mod api;
pub mod budget;
pub mod client;
pub mod clients;
pub mod config;
pub mod daemon;
pub mod dashboard;
pub mod edges;
pub mod events;
pub mod fdpass;
pub mod host;
pub mod locations;
pub mod logs;
pub mod metrics;
pub mod model;
pub mod notify;
pub mod overlays;
pub mod provision;
pub mod reload;
pub mod rollouts;
pub mod rotation;
pub mod scaling;
pub mod secrets;
pub mod servers;
pub mod settings;
pub mod shutdown;
pub mod sinks;
pub mod store;
pub mod triggers;
