//! Generated prost and tonic code for the worker and admin protocols, and
//! the frame codec of the worker link.
//!
//! The protocol definitions live under `proto/` at the repository root and
//! are compiled by the build script with the vendored protoc. The worker
//! link is described in section 5 of the design specification: requests
//! travel as frames (`frame`) whose structured payloads are the protobuf
//! messages here, and the `Worker` service is the control plane, served
//! from phase 2. The admin protocol (`admin`) is what muructl speaks to the
//! daemon (section 10.6 and appendix B).

pub mod frame;

/// The worker protocol, package `muru.worker.v1`.
pub mod worker {
    /// Version 1 of the worker protocol.
    #[allow(missing_docs, clippy::all, clippy::pedantic)]
    pub mod v1 {
        tonic::include_proto!("muru.worker.v1");

        /// The protocol version announced in HELLO frames and request heads.
        pub const PROTOCOL_VERSION: &str = "1.0";
    }
}

/// The admin protocol, package `muru.admin.v1`.
pub mod admin {
    /// Version 1 of the admin protocol.
    #[allow(missing_docs, clippy::all, clippy::pedantic)]
    pub mod v1 {
        tonic::include_proto!("muru.admin.v1");
    }
}
