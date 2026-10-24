//! Generates the tonic and prost code for the worker protocol with the vendored protoc.

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let include = protoc_bin_vendored::include_path()?;
    // SAFETY: a build script is single-threaded, so setting environment
    // variables cannot race with another thread reading them.
    unsafe {
        std::env::set_var("PROTOC", &protoc);
        std::env::set_var("PROTOC_INCLUDE", &include);
    }
    tonic_prost_build::configure()
        .bytes(".muru.worker.v1")
        .compile_protos(
            &["../../proto/muru/worker/v1/worker.proto"],
            &["../../proto"],
        )?;
    println!("cargo:rerun-if-changed=../../proto/muru/worker/v1/worker.proto");
    Ok(())
}
