//! Garbage collection in the worker (design specification sections 11.1
//! and 11.7). CPython's cyclic collector pauses the whole process while
//! it runs, so each worker times its own collections through
//! `gc.callbacks` and reports them the way it reports its CPU time and
//! GIL waits: how many ran, how long they took, what they freed, and
//! whether requests were in flight while they did, kept apart by
//! generation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::signals::Histogram;

/// The generations kept apart: 0, 1 and 2 on a build whose collector has
/// them, and one slot for a collector that takes the whole heap each
/// time, which the free-threaded build's does.
const SLOTS: usize = 4;

/// The slot of a whole-heap collection.
const FULL: usize = 3;

/// What has happened in one generation since the worker started.
struct Generation {
    /// Collections run.
    collections: AtomicU64,
    /// Time spent collecting, in nanoseconds.
    nanos: AtomicU64,
    /// Objects the collections freed.
    collected: AtomicU64,
    /// Collections that ran while the worker had requests in flight.
    busy: AtomicU64,
    /// The requests in flight during those, summed.
    waited: AtomicU64,
    /// Pauses with no request in flight.
    idle_pauses: Histogram,
    /// Pauses with requests in flight.
    busy_pauses: Histogram,
}

impl Generation {
    const fn new() -> Self {
        Self {
            collections: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
            collected: AtomicU64::new(0),
            busy: AtomicU64::new(0),
            waited: AtomicU64::new(0),
            idle_pauses: Histogram::new(),
            busy_pauses: Histogram::new(),
        }
    }
}

static GENERATIONS: [Generation; SLOTS] = [const { Generation::new() }; SLOTS];

/// The collection under way: when it started and the requests in flight
/// then. CPython runs one collection at a time.
static STARTED: Mutex<Option<(Instant, u64)>> = Mutex::new(None);

/// Whether the collector has generations: it does unless the build is
/// free-threaded, whose collector takes the whole heap each time. Asked
/// once.
fn generational(py: Python<'_>) -> bool {
    static GENERATIONAL: OnceLock<bool> = OnceLock::new();

    *GENERATIONAL.get_or_init(|| {
        py.import("sysconfig")
            .and_then(|sysconfig| sysconfig.call_method1("get_config_var", ("Py_GIL_DISABLED",)))
            .and_then(|value| value.extract::<Option<i64>>())
            .map(|value| value.unwrap_or(0) != 1)
            .unwrap_or(true)
    })
}

/// The requests the worker has in flight now.
fn in_flight() -> u64 {
    crate::SERVING
        .lock()
        .expect("serving poisoned")
        .upgrade()
        .map_or(0, |shared| u64::from(shared.in_flight()))
}

/// Record one finished collection.
fn record(slot: usize, pause: Duration, collected: u64, in_flight: u64) {
    let generation = &GENERATIONS[slot.min(FULL)];
    let nanos = u64::try_from(pause.as_nanos()).unwrap_or(u64::MAX);
    generation.collections.fetch_add(1, Ordering::Relaxed);
    generation.nanos.fetch_add(nanos, Ordering::Relaxed);
    generation.collected.fetch_add(collected, Ordering::Relaxed);

    if in_flight > 0 {
        generation.busy.fetch_add(1, Ordering::Relaxed);
        generation.waited.fetch_add(in_flight, Ordering::Relaxed);
        generation.busy_pauses.record(pause);
    } else {
        generation.idle_pauses.record(pause);
    }
}

/// The callback the worker adds to `gc.callbacks`: `start` notes the
/// time and the requests in flight, `stop` records the collection with
/// the generation and the objects collected from `info`.
#[pyfunction]
pub fn gc_callback(py: Python<'_>, phase: &str, info: &Bound<'_, PyDict>) -> PyResult<()> {
    match phase {
        "start" => {
            *STARTED.lock().expect("gc poisoned") = Some((Instant::now(), in_flight()));
        }
        "stop" => {
            let Some((started, in_flight)) = STARTED.lock().expect("gc poisoned").take() else {
                return Ok(());
            };

            let pause = started.elapsed();
            let item = |key: &str| -> PyResult<u64> {
                Ok(info
                    .get_item(key)?
                    .and_then(|value| value.extract::<i64>().ok())
                    .and_then(|value| u64::try_from(value).ok())
                    .unwrap_or(0))
            };

            let slot = if generational(py) {
                usize::try_from(item("generation")?).unwrap_or(0).min(2)
            } else {
                FULL
            };
            record(slot, pause, item("collected")?, in_flight);
        }
        _ => {}
    }

    Ok(())
}

/// The slot's name in a report: the generation number, or `full`.
fn name(slot: usize) -> &'static str {
    ["0", "1", "2", "full"][slot.min(FULL)]
}

/// The generations with a collection so far, each as a JSON object, for
/// the status line.
pub fn report_field() -> String {
    let objects: Vec<String> = (0..SLOTS)
        .filter(|slot| GENERATIONS[*slot].collections.load(Ordering::Relaxed) > 0)
        .map(|slot| {
            let generation = &GENERATIONS[slot];
            format!(
                "{{\"generation\":\"{}\",\"collections\":{},\"ns\":{},\"collected\":{},\"busy\":{},\"waited\":{},\"idle_hist\":{},\"idle_us\":{},\"busy_hist\":{},\"busy_us\":{}}}",
                name(slot),
                generation.collections.load(Ordering::Relaxed),
                generation.nanos.load(Ordering::Relaxed),
                generation.collected.load(Ordering::Relaxed),
                generation.busy.load(Ordering::Relaxed),
                generation.waited.load(Ordering::Relaxed),
                generation.idle_pauses.cumulative(),
                generation.idle_pauses.sum_us(),
                generation.busy_pauses.cumulative(),
                generation.busy_pauses.sum_us(),
            )
        })
        .collect();
    format!(",\"gc\":[{}]", objects.join(","))
}

/// The same figures for `muructl debug memory` (section 11.7): one dict
/// per generation with a collection so far, with the pause histograms as
/// cumulative counts over the signal bounds.
#[pyfunction]
pub fn gc_stats(py: Python<'_>) -> PyResult<Bound<'_, PyList>> {
    let list = PyList::empty(py);

    for slot in (0..SLOTS).filter(|slot| GENERATIONS[*slot].collections.load(Ordering::Relaxed) > 0)
    {
        let generation = &GENERATIONS[slot];
        let dict = PyDict::new(py);
        dict.set_item("generation", name(slot))?;
        dict.set_item(
            "collections",
            generation.collections.load(Ordering::Relaxed),
        )?;
        dict.set_item("ns", generation.nanos.load(Ordering::Relaxed))?;
        dict.set_item("collected", generation.collected.load(Ordering::Relaxed))?;
        dict.set_item("busy", generation.busy.load(Ordering::Relaxed))?;
        dict.set_item("waited", generation.waited.load(Ordering::Relaxed))?;
        dict.set_item("idle_hist", generation.idle_pauses.counts())?;
        dict.set_item("idle_us", generation.idle_pauses.sum_us())?;
        dict.set_item("busy_hist", generation.busy_pauses.counts())?;
        dict.set_item("busy_us", generation.busy_pauses.sum_us())?;
        list.append(dict)?;
    }

    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collections_are_kept_by_generation_and_by_requests_in_flight() {
        // Tests share the statics, so this one uses the slot nothing
        // else records into on a test run: the whole-heap one.

        record(FULL, Duration::from_micros(300), 12, 0);
        record(FULL, Duration::from_millis(20), 7, 3);
        record(9, Duration::from_micros(50), 1, 0);

        let generation = &GENERATIONS[FULL];
        assert_eq!(generation.collections.load(Ordering::Relaxed), 3);
        assert_eq!(
            generation.nanos.load(Ordering::Relaxed),
            300_000 + 20_000_000 + 50_000
        );
        assert_eq!(generation.collected.load(Ordering::Relaxed), 20);
        assert_eq!(generation.busy.load(Ordering::Relaxed), 1);
        assert_eq!(generation.waited.load(Ordering::Relaxed), 3);
        assert_eq!(
            generation.idle_pauses.cumulative(),
            "[1,2,2,2,2,2,2,2,2,2,2]"
        );
        assert_eq!(
            generation.busy_pauses.cumulative(),
            "[0,0,0,0,0,1,1,1,1,1,1]"
        );

        let report = report_field();
        assert!(report.starts_with(",\"gc\":["), "{report}");
        assert!(
            report.contains("{\"generation\":\"full\",\"collections\":3,\"ns\":20350000,\"collected\":20,\"busy\":1,\"waited\":3,"),
            "{report}"
        );
        assert_eq!(name(0), "0");
        assert_eq!(name(FULL), "full");
    }
}
