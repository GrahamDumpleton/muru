//! Offloaded uploads as the application sees them (design specification
//! section 4.11). The edge stores each file and passes its descriptor with
//! the request head; the worker owns the descriptors until the request
//! ends and describes each file to Python under `muru.uploads`, where the
//! `muru.uploads` module turns the descriptions into upload objects.

use std::os::fd::RawFd;

use muru_proto::worker::v1 as pb;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

/// One dictionary per stored file, in the order the edge sent them:
/// `ref`, `field` (None for a raw upload), `name`, `content_type`,
/// `size`, `digests` by algorithm, `fd`, and `path` where the spool file
/// has a name the worker may link (None otherwise).
pub(crate) fn describe<'py>(
    py: Python<'py>,
    refs: &[pb::UploadRef],
    fds: &[RawFd],
) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);

    for (upload, fd) in refs.iter().zip(fds) {
        let item = PyDict::new(py);
        item.set_item("ref", &upload.r#ref)?;
        item.set_item(
            "field",
            (!upload.field.is_empty()).then_some(upload.field.as_str()),
        )?;
        item.set_item("name", &upload.filename)?;
        item.set_item("content_type", &upload.content_type)?;
        item.set_item("size", upload.size)?;
        let digests = PyDict::new(py);

        for (algorithm, digest) in &upload.digests {
            digests.set_item(algorithm, digest)?;
        }

        item.set_item("digests", digests)?;
        item.set_item("fd", *fd)?;
        item.set_item(
            "path",
            (!upload.path.is_empty()).then_some(upload.path.as_str()),
        )?;
        list.append(item)?;
    }

    Ok(list)
}
