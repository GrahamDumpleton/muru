//! The worker's own scaling signals (design specification section 7.3):
//! the CPU time it has used, how long its threads wait for the GIL when
//! Rust hands work back to Python, and how late its event loop's
//! heartbeat runs. The host relays them to the daemon, which turns the
//! growth between two reports into rates and percentiles.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use muru_proto::worker::v1::SIGNAL_BOUNDS_US;
use pyo3::prelude::*;

/// A cumulative histogram over `SIGNAL_BOUNDS_US`, with the count of
/// all after the bounds.
pub struct Histogram {
    counts: [AtomicU64; SIGNAL_BOUNDS_US.len() + 1],
}

impl Histogram {
    const fn new() -> Self {
        Self {
            counts: [const { AtomicU64::new(0) }; SIGNAL_BOUNDS_US.len() + 1],
        }
    }

    /// Count one observation.
    pub fn record(&self, value: Duration) {
        let micros = u64::try_from(value.as_micros()).unwrap_or(u64::MAX);
        let slot = SIGNAL_BOUNDS_US
            .iter()
            .position(|bound| micros <= *bound)
            .unwrap_or(SIGNAL_BOUNDS_US.len());
        self.counts[slot].fetch_add(1, Ordering::Relaxed);
    }

    /// The counts at or below each bound, then the count of all, as a
    /// JSON array.
    fn cumulative(&self) -> String {
        let mut total = 0;
        let counts: Vec<String> = self
            .counts
            .iter()
            .map(|count| {
                total += count.load(Ordering::Relaxed);
                total.to_string()
            })
            .collect();
        format!("[{}]", counts.join(","))
    }
}

/// Waits from a thread being ready to run Python to holding the GIL.
pub static GIL_WAIT: Histogram = Histogram::new();

/// How much later than due the event loop's heartbeat ran.
pub static LOOP_LAG: Histogram = Histogram::new();

/// The CPU time this process has used, user and system, in
/// milliseconds.
fn cpu_ms() -> u64 {
    // SAFETY: getrusage fills the struct it is given; a zeroed rusage is
    // a valid value to pass.

    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };

    let ms = |time: libc::timeval| time.tv_sec as u64 * 1000 + time.tv_usec as u64 / 1000;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

/// Whether the interpreter runs with a GIL: always, unless it is a
/// free-threaded build with the GIL off. Asked once.
fn gil_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();

    *ENABLED.get_or_init(|| {
        Python::attach(|py| {
            py.import("sys")
                .and_then(|sys| sys.getattr("_is_gil_enabled"))
                .and_then(|is_enabled| is_enabled.call0())
                .and_then(|enabled| enabled.extract())
                .unwrap_or(true)
        })
    })
}

/// The signals as fields to add to a status line: the CPU time, whether
/// the interpreter has a GIL, and the two histograms.
pub fn report_fields() -> String {
    format!(
        ",\"cpu_ms\":{},\"gil_enabled\":{},\"gil_wait\":{},\"loop_lag\":{}",
        cpu_ms(),
        gil_enabled(),
        GIL_WAIT.cumulative(),
        LOOP_LAG.cumulative()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_histogram_counts_at_or_below_each_bound() {
        let histogram = Histogram::new();
        histogram.record(Duration::from_micros(50));
        histogram.record(Duration::from_millis(3));
        histogram.record(Duration::from_secs(60));
        assert_eq!(histogram.cumulative(), "[1,1,1,2,2,2,2,2,2,2,3]");
        assert!(cpu_ms() < u64::MAX);
    }
}
