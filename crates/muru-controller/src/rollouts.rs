//! Canary gates (design specification section 9.3): the request counters
//! of a release read from the edge's metrics, and the verdict on a
//! candidate against the baseline over the same window.

use std::collections::BTreeMap;

use muru_edge::metrics::Sample;

use crate::model::RolloutGates;

/// A release's request counters: how many requests it answered, how many
/// with a 5xx status, and the cumulative histogram of total request time
/// by upper bound in seconds, with its workers' crashes and timeout kills,
/// which the daemon counts from the host's events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Counters {
    /// Requests answered.
    pub total: f64,
    /// Requests answered with a 5xx status.
    pub errors: f64,
    /// Histogram buckets: the upper bound's bits, then the bound in
    /// seconds and the cumulative count at or below it.
    pub buckets: BTreeMap<u64, (f64, f64)>,
    /// Worker crashes and timeout kills.
    pub faults: f64,
}

impl Counters {
    /// The release's counters in a set of edge samples.
    pub fn from_samples(samples: &[Sample], app: &str, release: &str) -> Self {
        let mut counters = Counters::default();
        let label = |sample: &Sample, name: &str| -> Option<String> {
            sample
                .labels
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };

        for sample in samples {
            if label(sample, "app").as_deref() != Some(app)
                || label(sample, "release").as_deref() != Some(release)
            {
                continue;
            }

            match sample.name.as_str() {
                "muru_requests_total" => {
                    counters.total += sample.value;

                    if label(sample, "status").as_deref() == Some("5xx") {
                        counters.errors += sample.value;
                    }
                }
                "muru_request_duration_seconds_bucket"
                    if label(sample, "phase").as_deref() == Some("total") =>
                {
                    let bound = match label(sample, "le").as_deref() {
                        Some("+Inf") => f64::INFINITY,
                        Some(text) => text.parse().unwrap_or(f64::INFINITY),
                        None => continue,
                    };

                    let entry = counters
                        .buckets
                        .entry(bound.to_bits())
                        .or_insert((bound, 0.0));
                    entry.1 += sample.value;
                }
                _ => {}
            }
        }

        counters
    }

    /// What happened since `start`: the counters' growth.
    pub fn since(&self, start: &Counters) -> Counters {
        let mut buckets = BTreeMap::new();

        for (key, (bound, count)) in &self.buckets {
            let before = start.buckets.get(key).map_or(0.0, |(_, count)| *count);
            buckets.insert(*key, (*bound, (count - before).max(0.0)));
        }

        Counters {
            total: (self.total - start.total).max(0.0),
            errors: (self.errors - start.errors).max(0.0),
            buckets,
            faults: (self.faults - start.faults).max(0.0),
        }
    }

    /// The share of requests answered with a 5xx status.
    pub fn error_rate(&self) -> f64 {
        if self.total > 0.0 {
            self.errors / self.total
        } else {
            0.0
        }
    }

    /// The 95th percentile of total request time in milliseconds, or
    /// none without requests.
    pub fn p95_ms(&self) -> Option<f64> {
        self.percentile_ms(0.95)
    }

    /// The 99th percentile of total request time in milliseconds, or
    /// none without requests.
    pub fn p99_ms(&self) -> Option<f64> {
        self.percentile_ms(0.99)
    }

    /// A percentile of total request time in milliseconds: the upper
    /// bound of the first bucket holding that share of the requests the
    /// histogram counted, or none without requests.
    fn percentile_ms(&self, share: f64) -> Option<f64> {
        let mut ordered: Vec<(f64, f64)> = self.buckets.values().copied().collect();
        ordered.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let counted = ordered.last().map(|(_, count)| *count).unwrap_or(0.0);

        if counted <= 0.0 {
            return None;
        }

        let target = counted * share;
        ordered
            .iter()
            .find(|(_, count)| *count >= target)
            .map(|(bound, _)| bound * 1000.0)
    }
}

/// What the gates say about a step so far.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Too few requests to judge.
    Insufficient,
    /// Within every gate.
    Pass,
    /// Outside a gate, and why.
    Fail(String),
}

/// Judge the candidate's step against the gates and the baseline over the
/// same window. The candidate must have served `min_requests`; a gate
/// fails only when the candidate is both over its limit and worse than
/// the baseline, so a fault both releases share does not roll back. A
/// baseline with too few requests is not compared, only the limits apply.
pub fn judge(candidate: &Counters, baseline: &Counters, gates: &RolloutGates) -> Verdict {
    // A worker that crashes or is killed decides at once, however few
    // requests the step has seen.

    if candidate.faults > gates.max_worker_faults as f64 && candidate.faults > baseline.faults {
        return Verdict::Fail(format!(
            "{} worker crashes and timeout kills, over the limit of {} (baseline {})",
            candidate.faults, gates.max_worker_faults, baseline.faults
        ));
    }

    if candidate.total < gates.min_requests as f64 {
        return Verdict::Insufficient;
    }

    let compared = baseline.total >= gates.min_requests as f64;
    let rate = candidate.error_rate();

    if rate > gates.max_error_rate && (!compared || rate > baseline.error_rate()) {
        return Verdict::Fail(format!(
            "5xx rate {:.2}% over the limit of {:.2}% (baseline {:.2}%)",
            rate * 100.0,
            gates.max_error_rate * 100.0,
            baseline.error_rate() * 100.0
        ));
    }

    if let Some(limit) = gates.max_p95_ms
        && let Some(p95) = candidate.p95_ms()
        && p95 > limit as f64
        && (!compared || baseline.p95_ms().is_none_or(|base| p95 > base))
    {
        return Verdict::Fail(format!(
            "p95 {p95:.0} ms over the limit of {limit} ms (baseline {})",
            baseline
                .p95_ms()
                .map_or("none".to_owned(), |base| format!("{base:.0} ms"))
        ));
    }

    if let Some(limit) = gates.max_p99_ms
        && let Some(p99) = candidate.p99_ms()
        && p99 > limit as f64
        && (!compared || baseline.p99_ms().is_none_or(|base| p99 > base))
    {
        return Verdict::Fail(format!(
            "p99 {p99:.0} ms over the limit of {limit} ms (baseline {})",
            baseline
                .p99_ms()
                .map_or("none".to_owned(), |base| format!("{base:.0} ms"))
        ));
    }

    Verdict::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, labels: &[(&str, &str)], value: f64) -> Sample {
        Sample {
            name: name.to_owned(),
            labels: labels
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            value,
        }
    }

    fn counters(total: f64, errors: f64, fast: f64) -> Counters {
        let mut buckets = BTreeMap::new();
        buckets.insert(0.1f64.to_bits(), (0.1, fast));
        buckets.insert(1.0f64.to_bits(), (1.0, total));
        buckets.insert(f64::INFINITY.to_bits(), (f64::INFINITY, total));
        Counters {
            total,
            errors,
            buckets,
            faults: 0.0,
        }
    }

    fn gates() -> RolloutGates {
        RolloutGates {
            max_error_rate: 0.01,
            max_p95_ms: Some(500),
            max_p99_ms: None,
            max_worker_faults: 0,
            min_requests: 10,
        }
    }

    #[test]
    fn counters_come_from_the_release_s_samples_alone() {
        let samples = vec![
            sample(
                "muru_requests_total",
                &[("app", "shop"), ("release", "r2"), ("status", "2xx")],
                90.0,
            ),
            sample(
                "muru_requests_total",
                &[("app", "shop"), ("release", "r2"), ("status", "5xx")],
                10.0,
            ),
            sample(
                "muru_requests_total",
                &[("app", "shop"), ("release", "r1"), ("status", "5xx")],
                50.0,
            ),
            sample(
                "muru_request_duration_seconds_bucket",
                &[
                    ("app", "shop"),
                    ("release", "r2"),
                    ("phase", "total"),
                    ("le", "0.1"),
                ],
                96.0,
            ),
            sample(
                "muru_request_duration_seconds_bucket",
                &[
                    ("app", "shop"),
                    ("release", "r2"),
                    ("phase", "total"),
                    ("le", "+Inf"),
                ],
                100.0,
            ),
        ];
        let counters = Counters::from_samples(&samples, "shop", "r2");
        assert_eq!((counters.total, counters.errors), (100.0, 10.0));
        assert_eq!(counters.error_rate(), 0.1);
        assert_eq!(counters.p95_ms(), Some(100.0));

        let later = Counters {
            total: 150.0,
            errors: 10.0,
            ..counters.clone()
        };
        let growth = later.since(&counters);
        assert_eq!((growth.total, growth.errors), (50.0, 0.0));
        assert_eq!(Counters::default().p95_ms(), None);
    }

    #[test]
    fn gates_judge_against_limits_and_the_baseline() {
        let good = counters(100.0, 0.0, 100.0);
        assert_eq!(
            judge(&counters(5.0, 5.0, 0.0), &good, &gates()),
            Verdict::Insufficient
        );
        assert_eq!(judge(&good, &good, &gates()), Verdict::Pass);

        match judge(&counters(100.0, 5.0, 100.0), &good, &gates()) {
            Verdict::Fail(reason) => assert!(reason.contains("5xx rate 5.00%"), "{reason}"),
            other => panic!("{other:?}"),
        }

        // Both releases failing alike is not the candidate's fault.

        let shared = counters(100.0, 5.0, 100.0);
        assert_eq!(judge(&shared, &shared, &gates()), Verdict::Pass);
        assert!(matches!(
            judge(&shared, &counters(5.0, 0.0, 5.0), &gates()),
            Verdict::Fail(_)
        ));

        // Slow: 95 percent land in the one second bucket, over 500 ms.

        match judge(&counters(100.0, 0.0, 50.0), &good, &gates()) {
            Verdict::Fail(reason) => assert!(reason.contains("p95 1000 ms"), "{reason}"),
            other => panic!("{other:?}"),
        }

        assert_eq!(
            judge(
                &counters(100.0, 0.0, 50.0),
                &counters(100.0, 0.0, 50.0),
                &gates()
            ),
            Verdict::Pass,
            "as slow as the baseline passes"
        );
    }

    #[test]
    fn a_worker_crash_decides_at_once_and_p99_is_a_gate_of_its_own() {
        let good = counters(100.0, 0.0, 100.0);
        let mut crashed = counters(2.0, 0.0, 2.0);
        crashed.faults = 1.0;

        match judge(&crashed, &good, &gates()) {
            Verdict::Fail(reason) => assert!(reason.contains("1 worker crashes"), "{reason}"),
            other => panic!("{other:?}"),
        }

        // Faults the baseline has as well are not the candidate's, and a
        // limit allows some.

        let mut shared = good.clone();
        shared.faults = 1.0;
        assert_eq!(judge(&shared, &shared, &gates()), Verdict::Pass);
        let allowing = RolloutGates {
            max_worker_faults: 1,
            ..gates()
        };
        assert_eq!(judge(&shared, &good, &allowing), Verdict::Pass);

        // 99 percent land in the one second bucket: within p95's limit of
        // 500 ms when 95 are fast, past a p99 limit.

        let tail = counters(100.0, 0.0, 96.0);
        let strict = RolloutGates {
            max_p99_ms: Some(500),
            ..gates()
        };
        assert_eq!(judge(&tail, &good, &gates()), Verdict::Pass);

        match judge(&tail, &good, &strict) {
            Verdict::Fail(reason) => assert!(reason.contains("p99 1000 ms"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }
}
