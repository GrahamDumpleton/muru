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

pub mod descriptors;
pub mod frame;

/// The worker protocol, package `muru.worker.v1`.
pub mod worker {
    /// Version 1 of the worker protocol.
    #[allow(missing_docs, clippy::all, clippy::pedantic)]
    pub mod v1 {
        tonic::include_proto!("muru.worker.v1");

        /// The protocol version announced in HELLO frames and request heads.
        pub const PROTOCOL_VERSION: &str = "1.0";

        /// Section 7.3: the upper bounds, in microseconds, of the GIL
        /// wait and event loop lag histograms a worker reports to its
        /// host, each reported as the cumulative count at or below each
        /// bound and then the count of all.
        pub const SIGNAL_BOUNDS_US: [u64; 10] = [
            100, 500, 1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000, 5_000_000,
        ];
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

/// The edge control protocol, package `muru.edge.v1`, between the daemon
/// and its edge processes.
pub mod edge {
    /// Version 1 of the edge control protocol.
    #[allow(missing_docs, clippy::all, clippy::pedantic)]
    pub mod v1 {
        tonic::include_proto!("muru.edge.v1");
    }
}
