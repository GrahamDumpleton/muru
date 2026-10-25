//! The autoscaler (design specification sections 7.2 to 7.4): one scaler
//! per worker group, fed the edge's slot and queue signals each second,
//! deciding how many workers the group should have. Scale-out is quick
//! and bounded; scale-in is slow, one worker at a time, held off after
//! growth and by a churn budget, so load that comes and goes never turns
//! into workers that come and go. The clock is passed in, so the
//! decisions can be driven through synthetic load shapes in tests.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use muru_config::ScalingMode;
use muru_edge::metrics::Sample;

use crate::model::ScalingSpec;

/// A cumulative histogram: by the bound's bits, the bound in seconds and
/// the count at or below it.
pub type Buckets = BTreeMap<u64, (f64, f64)>;

/// The hour the churn budget counts reaps over.
const BUDGET_WINDOW: Duration = Duration::from_secs(3600);

/// The floor of the default minimum lifetime, with 30 times the measured
/// time to ready.
const MIN_LIFETIME_FLOOR: Duration = Duration::from_secs(15 * 60);

/// What the edge and the host say about one worker group at one instant.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Signals {
    /// Slots busy with a request across the workers the edge routes to.
    pub busy: f64,
    /// Slots across the workers the edge routes to.
    pub total: f64,
    /// Requests waiting at the edge for a slot in the group.
    pub queue_depth: f64,
    /// Workers the edge routes to.
    pub ready: u32,
    /// Slots each worker offers when none is routed to yet: its threads,
    /// or its event loop's concurrency.
    pub slots_per_worker: f64,
    /// The cumulative queue wait histogram.
    pub queue_buckets: Buckets,
    /// How long the youngest worker that serves has been ready.
    pub youngest: Option<Duration>,
}

impl Signals {
    /// The group's signals in a set of edge samples: the slot gauges and
    /// the queue histogram labelled with the application, release and
    /// group.
    pub fn from_samples(
        samples: &[Sample],
        app: &str,
        release: &str,
        group: &str,
        ready: u32,
        slots_per_worker: f64,
        youngest: Option<Duration>,
    ) -> Self {
        let mut signals = Signals {
            ready,
            slots_per_worker,
            youngest,
            ..Signals::default()
        };
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
                || label(sample, "group").as_deref() != Some(group)
            {
                continue;
            }

            match sample.name.as_str() {
                "muru_slots_busy" => signals.busy += sample.value,
                "muru_slots_total" => signals.total += sample.value,
                "muru_queue_depth" => signals.queue_depth += sample.value,
                "muru_request_duration_seconds_bucket"
                    if label(sample, "phase").as_deref() == Some("queue") =>
                {
                    let bound = match label(sample, "le").as_deref() {
                        Some("+Inf") => f64::INFINITY,
                        Some(text) => text.parse().unwrap_or(f64::INFINITY),
                        None => continue,
                    };

                    let entry = signals
                        .queue_buckets
                        .entry(bound.to_bits())
                        .or_insert((bound, 0.0));
                    entry.1 += sample.value;
                }
                _ => {}
            }
        }

        signals
    }
}

/// What the autoscaler decided for a group.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Fork workers up to this count.
    ScaleOut {
        /// The count to reach.
        to: u32,
        /// Why.
        reason: String,
    },
    /// Drain one worker, down to this count.
    ScaleIn {
        /// The count to reach.
        to: u32,
        /// Why.
        reason: String,
    },
    /// Scale-in is suspended until the churn budget recovers.
    Suspended {
        /// Why.
        reason: String,
    },
}

/// One observation kept for the windows.
#[derive(Debug, Clone, Copy)]
struct Point {
    at: Instant,
    idle: f64,
    busy: f64,
}

/// The autoscaler of one worker group.
#[derive(Debug)]
pub struct Scaler {
    spec: ScalingSpec,
    desired: u32,
    pinned: Option<u32>,
    points: VecDeque<Point>,
    queue: VecDeque<(Instant, Buckets)>,
    out_since: Option<Instant>,
    last_out: Option<Instant>,
    last_step: u32,
    last_in: Option<Instant>,
    reaps: VecDeque<Instant>,
    suspended: bool,
    lengthened: bool,
    requested_at: Option<Instant>,
    time_to_ready: Option<Duration>,
    last: Option<(Instant, String)>,
    signals: Signals,
    queue_wait_p95_ms: Option<f64>,
    decisions: (u64, u64),
}

/// The scaler's state for the admin API and the dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct ScaleStatus {
    /// The policy.
    pub spec: ScalingSpec,
    /// The count the group is being brought to.
    pub desired: u32,
    /// The count pinned by an operator, when one is.
    pub pinned: Option<u32>,
    /// Workers the edge routes to.
    pub ready: u32,
    /// Slots busy and total across them.
    pub busy: f64,
    /// Slots across them.
    pub total: f64,
    /// Idle worker equivalents, counting workers still starting.
    pub idle: f64,
    /// The p95 queue wait over the scale-out window, in milliseconds.
    pub queue_wait_p95_ms: Option<f64>,
    /// The last decision and how long ago it was made.
    pub last: Option<(Duration, String)>,
    /// Reaps in the last hour.
    pub reaps: u32,
    /// Whether scale-in is suspended by the churn budget.
    pub suspended: bool,
    /// Scale-outs and scale-ins decided since the scaler began.
    pub decisions: (u64, u64),
}

impl Scaler {
    /// A scaler for a group that has `current` workers.
    pub fn new(spec: ScalingSpec, current: u32) -> Self {
        Self {
            spec,
            desired: current,
            pinned: None,
            points: VecDeque::new(),
            queue: VecDeque::new(),
            out_since: None,
            last_out: None,
            last_step: 1,
            last_in: None,
            reaps: VecDeque::new(),
            suspended: false,
            lengthened: false,
            requested_at: None,
            time_to_ready: None,
            last: None,
            signals: Signals {
                ready: current,
                ..Signals::default()
            },
            queue_wait_p95_ms: None,
            decisions: (0, 0),
        }
    }

    /// The policy.
    pub fn spec(&self) -> &ScalingSpec {
        &self.spec
    }

    /// The count the group is being brought to.
    pub fn desired(&self) -> u32 {
        self.desired
    }

    /// The operator's pin, when there is one.
    pub fn pinned(&self) -> Option<u32> {
        self.pinned
    }

    /// Pin the count: the autoscaler stands aside until `unpin`.
    pub fn pin(&mut self, count: u32, now: Instant) {
        self.pinned = Some(count);
        self.desired = count;
        self.out_since = None;
        self.last = Some((now, format!("pinned to {count}")));
    }

    /// Let the autoscaler decide again, from the count the group has.
    pub fn unpin(&mut self, now: Instant) {
        self.pinned = None;
        self.last = Some((now, "unpinned".to_owned()));
    }

    /// Take the count the group has as the one wanted: in advisory mode
    /// nothing is carried out, so each recommendation starts from what
    /// is there.
    pub fn sync(&mut self, current: u32) {
        if self.pinned.is_none() {
            self.desired = current;
        }
    }

    /// Reaps in the last hour, for the budget and the metrics.
    pub fn reaps(&self) -> u32 {
        self.reaps.len() as u32
    }

    /// The state for reports.
    pub fn status(&self, now: Instant) -> ScaleStatus {
        ScaleStatus {
            spec: self.spec.clone(),
            desired: self.desired,
            pinned: self.pinned,
            ready: self.signals.ready,
            busy: self.signals.busy,
            total: self.signals.total,
            idle: self.idle(&self.signals),
            queue_wait_p95_ms: self.queue_wait_p95_ms,
            last: self
                .last
                .as_ref()
                .map(|(at, what)| (now.saturating_duration_since(*at), what.clone())),
            reaps: self.reaps(),
            suspended: self.suspended,
            decisions: self.decisions,
        }
    }

    /// Take one observation and decide; none means hold.
    pub fn observe(&mut self, now: Instant, signals: Signals) -> Option<Decision> {
        self.note_queue(now, &signals);
        self.signals = signals;

        if self.pinned.is_some() || self.spec.is_static() {
            return None;
        }

        // Starting workers have all arrived: how long that took sets the
        // default minimum lifetime.

        if let Some(requested) = self.requested_at
            && self.signals.ready >= self.desired
        {
            self.time_to_ready = Some(now.saturating_duration_since(requested));
            self.requested_at = None;
        }

        let idle = self.idle(&self.signals);
        self.points.push_back(Point {
            at: now,
            idle,
            busy: self.signals.busy,
        });

        // Observations are kept a little past the longest window, so the
        // window is always covered whole whatever the tick's jitter.

        let keep = self
            .scale_in_window()
            .max(Duration::from_millis(self.spec.idle_reap_after_ms))
            + Duration::from_secs(5);

        while self
            .points
            .front()
            .is_some_and(|point| now.saturating_duration_since(point.at) > keep)
        {
            self.points.pop_front();
        }

        while self
            .reaps
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= BUDGET_WINDOW)
        {
            self.reaps.pop_front();
        }

        if self.reaps.is_empty() {
            self.lengthened = false;
        }

        let exhausted = self.reaps.len() >= self.spec.churn_budget as usize;

        if let Some(decision) = self.scale_out(now, idle) {
            return Some(decision);
        }

        if exhausted {
            if self.suspended {
                return None;
            }

            self.suspended = true;
            self.lengthened = true;
            let reason = format!(
                "{} workers reaped in the last hour, the churn budget; scale-in is suspended and the scale-in window doubled until it recovers",
                self.reaps.len()
            );
            self.last = Some((now, format!("suspended: {reason}")));
            return Some(Decision::Suspended { reason });
        }

        self.suspended = false;
        self.scale_in(now)
    }

    /// Keep the queue histogram for the scale-out window and read the p95
    /// of what arrived over it.
    fn note_queue(&mut self, now: Instant, signals: &Signals) {
        let window = Duration::from_millis(self.spec.scale_out_window_ms);
        self.queue.push_back((now, signals.queue_buckets.clone()));

        while self.queue.len() > 1
            && self
                .queue
                .get(1)
                .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= window)
        {
            self.queue.pop_front();
        }

        let Some((_, start)) = self.queue.front() else {
            return;
        };
        let mut delta: Vec<(f64, f64)> = signals
            .queue_buckets
            .iter()
            .map(|(key, (bound, count))| {
                let before = start.get(key).map_or(0.0, |(_, count)| *count);
                (*bound, (count - before).max(0.0))
            })
            .collect();
        delta.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let counted = delta.last().map(|(_, count)| *count).unwrap_or(0.0);

        self.queue_wait_p95_ms = if counted > 0.0 {
            let target = counted * 0.95;
            delta
                .iter()
                .find(|(_, count)| *count >= target)
                .map(|(bound, _)| bound * 1000.0)
        } else {
            None
        };
    }

    /// Idle worker equivalents: the slots not wanted, over the slots a
    /// worker offers, plus the workers still starting, which are
    /// capacity on the way. Requests waiting at the edge count as wanted
    /// slots, so a saturated group reads as short by that many.
    fn idle(&self, signals: &Signals) -> f64 {
        let per_worker = self.slots_per_worker(signals);
        let pending = self.desired.saturating_sub(signals.ready) as f64;

        if per_worker <= 0.0 {
            return pending;
        }

        (signals.total - signals.busy - signals.queue_depth) / per_worker + pending
    }

    fn slots_per_worker(&self, signals: &Signals) -> f64 {
        if signals.ready > 0 && signals.total > 0.0 {
            signals.total / signals.ready as f64
        } else {
            signals.slots_per_worker
        }
    }

    /// The scale-in window, doubled while the churn budget recovers.
    fn scale_in_window(&self) -> Duration {
        let window = Duration::from_millis(self.spec.scale_in_window_ms);

        if self.lengthened { window * 2 } else { window }
    }

    /// The target utilization as a fraction.
    fn target(&self) -> f64 {
        self.spec.slot_utilization_percent as f64 / 100.0
    }

    /// Workers that the busy slots would occupy at the target utilization.
    fn workers_for(&self, busy: f64, per_worker: f64) -> u32 {
        if per_worker <= 0.0 {
            return 0;
        }

        (busy / (self.target() * per_worker)).ceil() as u32
    }

    /// Section 7.4: scale out once a rule has held for its window, by a
    /// doubling step bounded by max_step_up, no sooner than a window
    /// after the last.
    fn scale_out(&mut self, now: Instant, idle: f64) -> Option<Decision> {
        let per_worker = self.slots_per_worker(&self.signals);
        let pending = self.desired.saturating_sub(self.signals.ready) as f64;
        let capacity = self.signals.total + pending * per_worker;

        let utilization = if capacity > 0.0 {
            self.signals.busy / capacity
        } else {
            0.0
        };

        let (short, why) = match self.spec.mode {
            ScalingMode::Spare => (
                idle < self.spec.min_spare as f64,
                if idle < 0.0 {
                    format!("short by {:.1} workers", -idle)
                } else {
                    format!(
                        "{idle:.1} idle workers, under min_spare {}",
                        self.spec.min_spare
                    )
                },
            ),
            ScalingMode::Target => {
                let over_queue = self
                    .spec
                    .queue_wait_p95_ms
                    .zip(self.queue_wait_p95_ms)
                    .is_some_and(|(limit, p95)| p95 > limit as f64);

                if utilization > self.target() {
                    (
                        true,
                        format!(
                            "{:.0}% of slots busy, over the target of {}%",
                            utilization * 100.0,
                            self.spec.slot_utilization_percent
                        ),
                    )
                } else if over_queue {
                    (
                        true,
                        format!(
                            "queue wait p95 {:.0} ms, over the target of {} ms",
                            self.queue_wait_p95_ms.unwrap_or(0.0),
                            self.spec.queue_wait_p95_ms.unwrap_or(0)
                        ),
                    )
                } else {
                    (false, String::new())
                }
            }
            ScalingMode::Static => (false, String::new()),
        };

        if !short || self.desired >= self.spec.max_workers {
            self.out_since = None;
            return None;
        }

        let since = *self.out_since.get_or_insert(now);
        let window = Duration::from_millis(self.spec.scale_out_window_ms);

        if now.saturating_duration_since(since) < window {
            return None;
        }

        if self
            .last_out
            .is_some_and(|last| now.saturating_duration_since(last) < window)
        {
            return None;
        }

        // The step covers what the signals say is missing; while requests
        // wait at the edge the group is saturated and the signals cannot
        // say how much more is wanted, so each step soon after the last
        // doubles it (section 7.2).

        let hold = Duration::from_millis(self.spec.hold_after_growth_ms);

        let doubling = match self.last_out {
            Some(last)
                if self.signals.queue_depth > 0.0 && now.saturating_duration_since(last) < hold =>
            {
                self.last_step * 2
            }
            _ => 1,
        };

        let shortfall = match self.spec.mode {
            ScalingMode::Spare => (self.spec.min_spare as f64 - idle).ceil().max(0.0) as u32,
            _ => self
                .workers_for(self.signals.busy + self.signals.queue_depth, per_worker)
                .saturating_sub(self.desired),
        };

        let step = doubling.max(shortfall).max(1).min(self.spec.max_step_up);
        let to = (self.desired + step).min(self.spec.max_workers);
        let reason = format!(
            "{why} for {}",
            muru_config::format_duration(now.saturating_duration_since(since))
        );
        self.desired = to;
        self.last_out = Some(now);
        self.last_step = step;
        self.requested_at = Some(now);
        self.out_since = None;
        self.decisions.0 += 1;
        self.last = Some((now, format!("scaled out to {to}: {reason}")));
        Some(Decision::ScaleOut { to, reason })
    }

    /// Section 7.4: scale in one worker when every condition has held
    /// over the window, judged by the peak of the window.
    fn scale_in(&mut self, now: Instant) -> Option<Decision> {
        if self.desired <= self.spec.min_workers || self.desired > self.signals.ready {
            return None;
        }

        let hold = Duration::from_millis(self.spec.hold_after_growth_ms);

        if self
            .last_out
            .is_some_and(|last| now.saturating_duration_since(last) < hold)
        {
            return None;
        }

        let interval = Duration::from_millis(self.spec.reap_interval_ms);

        if self
            .last_in
            .is_some_and(|last| now.saturating_duration_since(last) < interval)
        {
            return None;
        }

        let min_lifetime = self.spec.min_lifetime_ms.map_or_else(
            || {
                self.time_to_ready.map_or(MIN_LIFETIME_FLOOR, |ready| {
                    (ready * 30).max(MIN_LIFETIME_FLOOR)
                })
            },
            Duration::from_millis,
        );

        if self.signals.youngest.is_some_and(|age| age < min_lifetime) {
            return None;
        }

        let window = self.scale_in_window();
        let idle_after = Duration::from_millis(self.spec.idle_reap_after_ms);
        let covered = |window: Duration| -> Option<Vec<Point>> {
            let oldest = self.points.front()?;

            if now.saturating_duration_since(oldest.at) < window {
                return None;
            }

            Some(
                self.points
                    .iter()
                    .filter(|point| now.saturating_duration_since(point.at) <= window)
                    .copied()
                    .collect(),
            )
        };
        let per_worker = self.slots_per_worker(&self.signals);

        let reason = match self.spec.mode {
            ScalingMode::Spare => {
                let least_idle = covered(window).map(|points| {
                    points
                        .iter()
                        .map(|point| point.idle)
                        .fold(f64::INFINITY, f64::min)
                });
                let all_idle = covered(idle_after)
                    .is_some_and(|points| points.iter().all(|point| point.busy == 0.0));

                if let Some(least) = least_idle.filter(|least| *least > self.spec.max_spare as f64)
                {
                    format!(
                        "at least {least:.1} idle workers over the last {}, over max_spare {}",
                        muru_config::format_duration(window),
                        self.spec.max_spare
                    )
                } else if all_idle {
                    format!(
                        "no request for {}",
                        muru_config::format_duration(idle_after)
                    )
                } else {
                    return None;
                }
            }
            ScalingMode::Target => {
                let points = covered(window)?;
                let peak = points.iter().map(|point| point.busy).fold(0.0, f64::max);
                let needed = self.workers_for(peak, per_worker).max(1);

                if needed >= self.desired {
                    return None;
                }

                format!(
                    "at most {peak:.0} slots busy over the last {}, which {needed} workers carry at {}%",
                    muru_config::format_duration(window),
                    self.spec.slot_utilization_percent
                )
            }
            ScalingMode::Static => return None,
        };

        let to = self.desired - 1;
        self.desired = to;
        self.last_in = Some(now);
        self.reaps.push_back(now);
        self.decisions.1 += 1;
        self.last = Some((now, format!("scaled in to {to}: {reason}")));
        Some(Decision::ScaleIn { to, reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);
    const MINUTE: Duration = Duration::from_secs(60);

    /// A worker group under a scaler, driven through a load shape one
    /// second at a time: forked workers arrive ready two seconds after
    /// they are asked for, and a drained one leaves at once.
    struct Group {
        scaler: Scaler,
        start: Instant,
        ready: u32,
        arriving: Vec<(Instant, u32)>,
        started: Vec<Instant>,
        per_worker: f64,
        log: Vec<(Duration, Decision)>,
    }

    impl Group {
        fn new(spec: ScalingSpec, ready: u32, per_worker: f64) -> Self {
            let start = Instant::now();
            Self {
                scaler: Scaler::new(spec, ready),
                start,
                ready,
                arriving: Vec::new(),
                started: vec![start - MINUTE * 30; ready as usize],
                per_worker,
                log: Vec::new(),
            }
        }

        /// Run the shape for `length`, where `load` is busy slots at an
        /// offset, and return the offset the run ended at.
        fn run(&mut self, from: Duration, length: Duration, load: impl Fn(Duration) -> f64) {
            let mut offset = from;

            while offset < from + length {
                let now = self.start + offset;
                let mut arrived = Vec::new();

                self.arriving.retain(|(at, count)| {
                    if *at <= now {
                        arrived.push(*count);
                        false
                    } else {
                        true
                    }
                });

                for count in arrived {
                    self.ready += count;
                    self.started
                        .extend(std::iter::repeat_n(now, count as usize));
                }

                while self.ready > self.scaler.desired() {
                    self.ready -= 1;
                    self.started.pop();
                }

                let total = self.ready as f64 * self.per_worker;
                let demand = load(offset);
                let busy = demand.min(total);
                let signals = Signals {
                    busy,
                    total,
                    queue_depth: demand - busy,
                    ready: self.ready,
                    slots_per_worker: self.per_worker,
                    youngest: self
                        .started
                        .last()
                        .map(|at| now.saturating_duration_since(*at)),
                    ..Signals::default()
                };

                if let Some(decision) = self.scaler.observe(now, signals) {
                    if let Decision::ScaleOut { to, .. } = &decision {
                        let coming: u32 = self.arriving.iter().map(|(_, count)| count).sum();
                        self.arriving
                            .push((now + SECOND * 2, to - self.ready - coming));
                    }

                    self.log.push((offset, decision));
                }

                offset += SECOND;
            }
        }

        fn outs(&self) -> Vec<(Duration, u32)> {
            self.log
                .iter()
                .filter_map(|(at, decision)| match decision {
                    Decision::ScaleOut { to, .. } => Some((*at, *to)),
                    _ => None,
                })
                .collect()
        }

        fn ins(&self) -> Vec<(Duration, u32)> {
            self.log
                .iter()
                .filter_map(|(at, decision)| match decision {
                    Decision::ScaleIn { to, .. } => Some((*at, *to)),
                    _ => None,
                })
                .collect()
        }

        fn suspensions(&self) -> usize {
            self.log
                .iter()
                .filter(|(_, decision)| matches!(decision, Decision::Suspended { .. }))
                .count()
        }
    }

    fn spare() -> ScalingSpec {
        ScalingSpec {
            mode: ScalingMode::Spare,
            min_workers: 1,
            max_workers: 8,
            min_spare: 1,
            max_spare: 3,
            ..ScalingSpec::default()
        }
    }

    #[test]
    fn a_step_in_load_scales_out_quickly_and_back_in_slowly() {
        let mut group = Group::new(spare(), 1, 1.0);
        let quiet = SECOND * 10;
        let busy = MINUTE * 20;
        group.run(Duration::ZERO, quiet, |_| 0.0);
        assert!(group.log.is_empty(), "{:?}", group.log);

        group.run(quiet, busy, |_| 4.0);
        let outs = group.outs();
        assert_eq!(outs.len(), 1, "{outs:?}");
        assert_eq!(outs[0].0, quiet + SECOND * 5, "after the scale-out window");
        assert_eq!(outs[0].1, 5, "four wanted plus one spare, in one step");
        assert!(group.ins().is_empty(), "no scale-in under load");

        group.run(quiet + busy, MINUTE * 60, |_| 0.0);
        let ins = group.ins();
        assert_eq!(group.scaler.desired(), 1, "{ins:?}");
        assert!(
            ins[0].0 >= quiet + busy + MINUTE * 10,
            "the scale-in window must pass first: {ins:?}"
        );

        for pair in ins.windows(2) {
            assert!(
                pair[1].0 - pair[0].0 >= MINUTE,
                "one reap per interval: {ins:?}"
            );
        }

        assert_eq!(ins.len(), 4);
        assert_eq!(group.suspensions(), 0);
        assert_eq!(
            group
                .scaler
                .status(group.start + quiet + busy + MINUTE * 60)
                .reaps,
            4
        );
    }

    #[test]
    fn a_sawtooth_never_scales_in() {
        let mut group = Group::new(spare(), 1, 1.0);
        let sawtooth = |at: Duration| {
            if (at.as_secs() / 120).is_multiple_of(2) {
                4.0
            } else {
                0.0
            }
        };
        group.run(Duration::ZERO, MINUTE * 120, sawtooth);
        assert!(group.ins().is_empty(), "{:?}", group.log);
        assert_eq!(group.scaler.desired(), 5);
        assert!(
            group.outs().iter().all(|(at, _)| *at < MINUTE * 3),
            "{:?}",
            group.outs()
        );
    }

    #[test]
    fn bursts_shorter_than_the_window_are_ignored() {
        let mut group = Group::new(spare(), 2, 1.0);
        group.run(Duration::ZERO, MINUTE * 10, |at| {
            if at.as_secs() % 30 < 2 { 4.0 } else { 0.0 }
        });
        assert!(group.outs().is_empty(), "{:?}", group.log);
    }

    #[test]
    fn a_ramp_grows_by_doubling_steps_bounded_by_max_step_up() {
        let spec = ScalingSpec {
            max_workers: 32,
            min_spare: 2,
            max_spare: 6,
            ..spare()
        };
        let mut group = Group::new(spec, 1, 1.0);
        group.run(Duration::ZERO, MINUTE * 3, |at| {
            (at.as_secs() as f64).min(25.0)
        });
        let outs = group.outs();
        let mut previous = 1;
        let mut steps = Vec::new();

        for (_, to) in &outs {
            steps.push(to - previous);
            previous = *to;
        }

        assert!(steps.iter().all(|step| *step <= 4), "{steps:?}");
        assert!(steps.iter().any(|step| *step > 1), "{steps:?}");
        assert!(
            (27..=32).contains(&group.scaler.desired()),
            "25 wanted plus two spare: {outs:?}"
        );
    }

    #[test]
    fn the_churn_budget_suspends_scale_in_for_the_hour() {
        let spec = ScalingSpec {
            scale_in_window_ms: 10_000,
            idle_reap_after_ms: 10_000,
            hold_after_growth_ms: 0,
            reap_interval_ms: 1_000,
            min_lifetime_ms: Some(0),
            churn_budget: 3,
            ..spare()
        };
        let mut group = Group::new(spec, 8, 1.0);
        group.run(Duration::ZERO, MINUTE * 2, |_| 0.0);
        assert_eq!(group.ins().len(), 3, "{:?}", group.log);
        assert_eq!(group.suspensions(), 1);
        assert_eq!(group.scaler.desired(), 5);

        group.run(MINUTE * 2, MINUTE * 57, |_| 0.0);
        assert_eq!(
            group.ins().len(),
            3,
            "suspended for the hour: {:?}",
            group.log
        );

        group.run(MINUTE * 59, MINUTE * 3, |_| 0.0);
        assert!(group.ins().len() > 3, "{:?}", group.log);
        assert_eq!(group.suspensions(), 2, "the budget is spent again");
    }

    #[test]
    fn target_mode_tracks_utilization_both_ways() {
        let spec = ScalingSpec {
            mode: ScalingMode::Target,
            min_workers: 1,
            max_workers: 8,
            slot_utilization_percent: 50,
            ..ScalingSpec::default()
        };
        let mut group = Group::new(spec, 2, 4.0);
        group.run(Duration::ZERO, MINUTE, |_| 7.0);
        let outs = group.outs();
        assert_eq!(outs.len(), 1, "{outs:?}");
        assert_eq!(outs[0].1, 4, "seven busy slots at 50% of four per worker");

        group.run(MINUTE, MINUTE * 20, |_| 0.0);
        assert_eq!(group.scaler.desired(), 1, "{:?}", group.log);
    }

    #[test]
    fn a_pin_overrides_the_autoscaler() {
        let mut group = Group::new(spare(), 1, 1.0);
        group.scaler.pin(3, group.start);
        group.ready = 3;
        group.run(Duration::ZERO, MINUTE, |_| 8.0);
        assert!(group.log.is_empty(), "{:?}", group.log);
        assert_eq!(
            (group.scaler.desired(), group.scaler.pinned()),
            (3, Some(3))
        );

        group.scaler.unpin(group.start + MINUTE);
        group.run(MINUTE, MINUTE, |_| 8.0);
        assert!(!group.outs().is_empty());
    }

    #[test]
    fn signals_come_from_the_group_s_samples_and_the_queue_wait_is_a_target() {
        let sample = |name: &str, group: &str, extra: &[(&str, &str)], value: f64| {
            let mut labels = vec![("app", "shop"), ("release", "r1"), ("group", group)];
            labels.extend_from_slice(extra);
            Sample::new(name, &labels, value)
        };
        let at = |queue_count: f64| {
            vec![
                sample("muru_slots_busy", "default", &[], 3.0),
                sample("muru_slots_total", "default", &[], 8.0),
                sample("muru_slots_busy", "hot", &[], 50.0),
                sample(
                    "muru_request_duration_seconds_bucket",
                    "default",
                    &[("phase", "queue"), ("le", "0.05")],
                    queue_count * 0.5,
                ),
                sample(
                    "muru_request_duration_seconds_bucket",
                    "default",
                    &[("phase", "queue"), ("le", "0.25")],
                    queue_count,
                ),
                sample(
                    "muru_request_duration_seconds_bucket",
                    "default",
                    &[("phase", "queue"), ("le", "+Inf")],
                    queue_count,
                ),
            ]
        };
        let signals = Signals::from_samples(&at(100.0), "shop", "r1", "default", 2, 4.0, None);
        assert_eq!((signals.busy, signals.total, signals.ready), (3.0, 8.0, 2));
        assert_eq!(signals.queue_buckets.len(), 3);

        let spec = ScalingSpec {
            mode: ScalingMode::Target,
            min_workers: 1,
            max_workers: 8,
            slot_utilization_percent: 90,
            queue_wait_p95_ms: Some(100),
            ..ScalingSpec::default()
        };
        let mut scaler = Scaler::new(spec, 2);
        let start = Instant::now();
        let mut decision = None;

        for second in 0..10u64 {
            let samples = at(100.0 + second as f64 * 20.0);
            let signals = Signals::from_samples(&samples, "shop", "r1", "default", 2, 4.0, None);
            decision = scaler
                .observe(start + SECOND * second as u32, signals)
                .or(decision);
        }

        match decision {
            Some(Decision::ScaleOut { to: 3, reason }) => {
                assert!(reason.contains("queue wait p95 250 ms"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }
}
