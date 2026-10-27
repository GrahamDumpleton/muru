//! Throughput and latency over time for the dashboard's charts (design
//! specification section 10.7): one point a second for the last hour and
//! one a minute for the last day, computed from the differences between
//! successive readings of the request counters and histogram, and
//! streamed to the page as it grows.

use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::sync::broadcast;

/// How many second points are kept: an hour.
pub const SECONDS: usize = 3600;

/// How many minute points are kept: a day.
pub const MINUTES: usize = 1440;

/// One moment in the history.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    /// The second, Unix time.
    pub unix_s: u64,
    /// Requests completed per second.
    pub requests: f64,
    /// Responses with a 5xx status per second.
    pub errors: f64,
    /// Median total time, milliseconds, over the period.
    pub p50_ms: f64,
    /// 95th percentile total time, milliseconds, over the period.
    pub p95_ms: f64,
    /// Requests in flight at the end of the period.
    pub in_flight: u64,
    /// Workers listening at the end of the period.
    pub workers: u64,
    /// WebSockets open at the end of the period.
    pub websockets: u64,
}

impl Point {
    /// The point as the gateway writes it: a compact array of the
    /// second, requests per second, errors per second, p50, p95, in
    /// flight, workers and WebSockets open.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!([
            self.unix_s,
            round(self.requests),
            round(self.errors),
            round(self.p50_ms),
            round(self.p95_ms),
            self.in_flight,
            self.workers,
            self.websockets,
        ])
    }
}

fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// A reading of the counters, from which the next point is computed.
#[derive(Debug, Clone, Default)]
pub struct Reading {
    /// Requests completed, in all.
    pub requests_total: f64,
    /// Responses with a 5xx status, in all.
    pub errors_total: f64,
    /// The total-time histogram's cumulative buckets: the `le` bound in
    /// seconds and the count up to it, in ascending order.
    pub buckets: Vec<(f64, f64)>,
    /// Requests in flight now.
    pub in_flight: u64,
    /// Workers listening now.
    pub workers: u64,
    /// WebSockets open now.
    pub websockets: u64,
}

/// Turns successive readings into points.
#[derive(Debug, Default)]
pub struct Sampler {
    previous: Option<(u64, Reading)>,
}

impl Sampler {
    /// The point for a reading taken at `unix_s`, from its difference
    /// with the previous reading; the first reading yields nothing.
    pub fn observe(&mut self, unix_s: u64, reading: Reading) -> Option<Point> {
        let (then, previous) = self.previous.replace((unix_s, reading.clone()))?;
        let elapsed = unix_s.saturating_sub(then).max(1) as f64;
        let requests = (reading.requests_total - previous.requests_total).max(0.0);
        let errors = (reading.errors_total - previous.errors_total).max(0.0);
        let deltas: Vec<(f64, f64)> = reading
            .buckets
            .iter()
            .map(|(le, count)| {
                let before = previous
                    .buckets
                    .iter()
                    .find(|(other, _)| other == le)
                    .map_or(0.0, |(_, count)| *count);
                (*le, (count - before).max(0.0))
            })
            .collect();

        Some(Point {
            unix_s,
            requests: requests / elapsed,
            errors: errors / elapsed,
            p50_ms: quantile(&deltas, 0.5) * 1000.0,
            p95_ms: quantile(&deltas, 0.95) * 1000.0,
            in_flight: reading.in_flight,
            workers: reading.workers,
            websockets: reading.websockets,
        })
    }
}

/// A quantile from cumulative histogram buckets, interpolated within the
/// bucket it falls in, as Prometheus does; zero with no observations.
fn quantile(buckets: &[(f64, f64)], q: f64) -> f64 {
    let total = buckets.last().map_or(0.0, |(_, count)| *count);

    if total <= 0.0 {
        return 0.0;
    }

    let rank = q * total;
    let mut lower_bound = 0.0;
    let mut lower_count = 0.0;

    for (le, count) in buckets {
        if *count >= rank {
            if le.is_infinite() {
                return lower_bound;
            }

            let within = count - lower_count;

            if within <= 0.0 {
                return *le;
            }

            return lower_bound + (le - lower_bound) * ((rank - lower_count) / within);
        }

        lower_bound = *le;
        lower_count = *count;
    }

    lower_bound
}

/// The points kept, and a broadcast of each new one.
pub struct History {
    rings: Mutex<Rings>,
    live: broadcast::Sender<Point>,
}

#[derive(Default)]
struct Rings {
    seconds: VecDeque<Point>,
    minutes: VecDeque<Point>,
    pending: Vec<Point>,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    /// An empty history.
    pub fn new() -> Self {
        let (live, _) = broadcast::channel(64);
        Self {
            rings: Mutex::new(Rings::default()),
            live,
        }
    }

    /// Record a point and tell the streams.
    pub fn record(&self, point: Point) {
        let mut rings = self.rings.lock().expect("history poisoned");
        rings.seconds.push_back(point);

        while rings.seconds.len() > SECONDS {
            rings.seconds.pop_front();
        }

        // A minute's points fold into one once the minute is over.

        rings.pending.push(point);
        let minute = point.unix_s / 60;

        if rings
            .pending
            .first()
            .is_some_and(|first| first.unix_s / 60 != minute)
        {
            let finished: Vec<Point> = rings
                .pending
                .iter()
                .copied()
                .filter(|candidate| candidate.unix_s / 60 != minute)
                .collect();
            rings
                .pending
                .retain(|candidate| candidate.unix_s / 60 == minute);

            if let Some(folded) = fold(&finished) {
                rings.minutes.push_back(folded);
            }

            while rings.minutes.len() > MINUTES {
                rings.minutes.pop_front();
            }
        }

        drop(rings);
        let _ = self.live.send(point);
    }

    /// The last hour at one second, or the last day at one minute.
    pub fn points(&self, daily: bool) -> Vec<Point> {
        let rings = self.rings.lock().expect("history poisoned");

        if daily {
            rings.minutes.iter().copied().collect()
        } else {
            rings.seconds.iter().copied().collect()
        }
    }

    /// Each new point as it is recorded.
    pub fn subscribe(&self) -> broadcast::Receiver<Point> {
        self.live.subscribe()
    }
}

/// A minute's points as one: mean rates, the worst percentiles, the last
/// gauges.
fn fold(points: &[Point]) -> Option<Point> {
    let last = points.last()?;
    let count = points.len() as f64;
    Some(Point {
        unix_s: points[0].unix_s - points[0].unix_s % 60,
        requests: points.iter().map(|p| p.requests).sum::<f64>() / count,
        errors: points.iter().map(|p| p.errors).sum::<f64>() / count,
        p50_ms: points.iter().map(|p| p.p50_ms).fold(0.0, f64::max),
        p95_ms: points.iter().map(|p| p.p95_ms).fold(0.0, f64::max),
        in_flight: last.in_flight,
        workers: last.workers,
        websockets: last.websockets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(requests: f64, errors: f64, counts: &[f64]) -> Reading {
        let bounds = [0.001, 0.005, 0.01, 0.05, 0.1, 1.0, f64::INFINITY];
        Reading {
            requests_total: requests,
            errors_total: errors,
            buckets: bounds.iter().copied().zip(counts.iter().copied()).collect(),
            in_flight: 1,
            workers: 2,
            websockets: 3,
        }
    }

    #[test]
    fn points_are_differences_with_interpolated_quantiles() {
        let mut sampler = Sampler::default();
        assert!(
            sampler
                .observe(
                    100,
                    reading(10.0, 0.0, &[5.0, 8.0, 10.0, 10.0, 10.0, 10.0, 10.0])
                )
                .is_none()
        );
        let point = sampler
            .observe(
                101,
                reading(30.0, 2.0, &[5.0, 18.0, 28.0, 30.0, 30.0, 30.0, 30.0]),
            )
            .unwrap();
        assert_eq!(point.requests, 20.0);
        assert_eq!(point.errors, 2.0);

        // 20 new observations: 10 in (1ms, 5ms], 8 in (5ms, 10ms] and 2 in
        // (10ms, 50ms]; the median sits at the top of the first bucket and
        // the 95th percentile halfway through the last.

        assert!((point.p50_ms - 5.0).abs() < 0.01, "{}", point.p50_ms);
        assert!((point.p95_ms - 30.0).abs() < 0.01, "{}", point.p95_ms);
        assert_eq!(point.in_flight, 1);
        assert_eq!(point.workers, 2);
        assert_eq!(point.websockets, 3);
    }

    #[test]
    fn minutes_fold_once_the_minute_is_over() {
        let history = History::new();

        for second in 0..125 {
            history.record(Point {
                unix_s: 1_000_000 + second,
                requests: second as f64,
                ..Point::default()
            });
        }

        assert_eq!(history.points(false).len(), 125);
        let minutes = history.points(true);
        assert_eq!(minutes.len(), 2, "two whole minutes have passed");
        assert_eq!(minutes[0].unix_s % 60, 0);
        assert!(minutes[1].requests > minutes[0].requests);
    }

    #[test]
    fn an_empty_histogram_has_no_quantile() {
        assert_eq!(quantile(&[], 0.5), 0.0);
        assert_eq!(quantile(&[(1.0, 0.0), (f64::INFINITY, 0.0)], 0.5), 0.0);
    }
}
