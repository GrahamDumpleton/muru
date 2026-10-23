//! The `muru._native` extension module.
//!
//! Placeholder created with the project skeleton. Phase 0 adds the tonic
//! server started after fork, the WSGI bridge and the descriptor side channel
//! (design specification sections 5 and 6). The module is declared
//! free-threading compatible from the start (section 6.11).

use pyo3::prelude::*;

/// Return the version of the extension, for `muru.__version__` checks.
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Module initialisation for `muru._native`.
#[pymodule(gil_used = false)]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    Ok(())
}
