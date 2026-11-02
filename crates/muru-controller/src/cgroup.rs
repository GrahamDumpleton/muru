//! The container's limits as cgroup v2 gives them (design specification
//! section 13.9): the CPU quota and CPUs for the worker count, and the
//! memory limit for the workers' hard limits. They are read from the
//! process's own cgroup, which in a container with its own cgroup
//! namespace is the root of the mount.

use std::path::{Path, PathBuf};

/// Where cgroup v2 is mounted.
const MOUNT: &str = "/sys/fs/cgroup";

/// What is left of the memory limit for muructl, the edge and the page
/// cache: this share of it, and at least `HEADROOM_MIN`.
const HEADROOM_PERCENT: u64 = 10;

const HEADROOM_MIN: u64 = 128 * 1024 * 1024;

/// The limits of one cgroup, each absent when it is not set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// The cgroup's directory.
    pub dir: PathBuf,
    /// The quota and period of `cpu.max`, in microseconds.
    pub cpu_max: Option<(u64, u64)>,
    /// The CPUs of `cpuset.cpus.effective`.
    pub cpus: Option<usize>,
    /// `memory.max`, in bytes.
    pub memory_max: Option<u64>,
}

impl Limits {
    /// This process's cgroup v2 limits, or none where there is no cgroup
    /// v2 (macOS, cgroup v1).
    pub fn read() -> Option<Self> {
        let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        Self::read_from(Path::new(MOUNT), &membership)
    }

    /// The limits of the cgroup `membership` (the text of
    /// `/proc/self/cgroup`) names under `mount`. A path the mount does not
    /// hold, as when the container shares the host's cgroup namespace, is
    /// read as the mount's root, which is then the container's.
    pub fn read_from(mount: &Path, membership: &str) -> Option<Self> {
        let path = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?
            .trim_start_matches('/');
        let nested = mount.join(path);

        let dir = if nested.join("cgroup.controllers").is_file() {
            nested
        } else if mount.join("cgroup.controllers").is_file() {
            mount.to_path_buf()
        } else {
            return None;
        };

        let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();

        Some(Self {
            cpu_max: read("cpu.max").and_then(|text| parse_cpu_max(&text)),
            cpus: read("cpuset.cpus.effective").and_then(|text| count_cpus(&text)),
            memory_max: read("memory.max").and_then(|text| text.trim().parse().ok()),
            dir,
        })
    }

    /// The worker count the CPU limit gives, with the file it came from:
    /// the quota rounded up, at least 1 and at most the CPUs, or the CPUs
    /// with no quota.
    pub fn processes(&self) -> Option<(usize, String)> {
        let quota = self.cpu_max.map(|(quota, period)| {
            let count = usize::try_from(quota.div_ceil(period.max(1)))
                .unwrap_or(usize::MAX)
                .max(1);
            (count, format!("cpu.max {quota} {period}"))
        });
        let cpus = self
            .cpus
            .map(|cpus| (cpus, format!("cpuset.cpus.effective, {cpus} CPUs")));

        match (quota, cpus) {
            (Some(quota), Some(cpus)) if cpus.0 < quota.0 => Some(cpus),
            (quota, cpus) => quota.or(cpus),
        }
    }

    /// The memory the workers and the host may use: the limit less the
    /// headroom, and the headroom.
    pub fn memory_budget(&self) -> Option<(u64, u64)> {
        let limit = self.memory_max?;
        let headroom = (limit / 100 * HEADROOM_PERCENT).max(HEADROOM_MIN);
        let budget = limit.checked_sub(headroom).filter(|budget| *budget > 0)?;
        Some((budget, headroom))
    }
}

/// `cpu.max`'s quota and period, or none for `max`, no quota.
fn parse_cpu_max(text: &str) -> Option<(u64, u64)> {
    let mut fields = text.split_whitespace();
    let quota = fields.next()?.parse().ok()?;
    let period = fields
        .next()
        .map_or(Some(100_000), |field| field.parse().ok())?;
    Some((quota, period))
}

/// The CPUs a list such as `0-3,6,8-9` names.
fn count_cpus(text: &str) -> Option<usize> {
    let mut count = 0;

    for range in text.trim().split(',').filter(|range| !range.is_empty()) {
        count += match range.split_once('-') {
            Some((first, last)) => {
                let first: usize = first.parse().ok()?;
                let last: usize = last.parse().ok()?;
                last.checked_sub(first)? + 1
            }
            None => {
                range.parse::<usize>().ok()?;
                1
            }
        };
    }

    (count > 0).then_some(count)
}

/// The kernel's count of processes in the cgroup killed for memory, from
/// `memory.events`.
pub fn oom_kills(dir: &Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("memory.events"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("oom_kill "))?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cgroup(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temporary directory");
        std::fs::write(dir.path().join("cgroup.controllers"), "cpu memory\n").expect("write");

        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).expect("write");
        }

        dir
    }

    #[test]
    fn the_quota_is_rounded_up_and_held_to_the_cpus() {
        let dir = cgroup(&[
            ("cpu.max", "150000 100000\n"),
            ("cpuset.cpus.effective", "0-63\n"),
        ]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(
            limits.processes(),
            Some((2, "cpu.max 150000 100000".to_owned()))
        );

        let dir = cgroup(&[
            ("cpu.max", "800000 100000\n"),
            ("cpuset.cpus.effective", "0-1,4\n"),
        ]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(limits.processes().map(|(count, _)| count), Some(3));

        let dir = cgroup(&[
            ("cpu.max", "max 100000\n"),
            ("cpuset.cpus.effective", "2-5\n"),
        ]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(limits.processes().map(|(count, _)| count), Some(4));

        let dir = cgroup(&[("cpu.max", "1000 100000\n")]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(limits.processes().map(|(count, _)| count), Some(1));
    }

    #[test]
    fn the_memory_budget_leaves_headroom() {
        let dir = cgroup(&[("memory.max", "4294967296\n")]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        let (budget, headroom) = limits.memory_budget().expect("budget");
        assert_eq!(headroom, 4_294_967_296 / 100 * 10);
        assert_eq!(budget + headroom, 4_294_967_296);

        let dir = cgroup(&[("memory.max", "268435456\n")]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(limits.memory_budget(), Some((128 << 20, 128 << 20)));

        let dir = cgroup(&[("memory.max", "max\n")]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(limits.memory_budget(), None);
    }

    #[test]
    fn a_path_the_mount_does_not_hold_is_read_at_its_root() {
        let dir = cgroup(&[("memory.max", "1073741824\n")]);
        let limits = Limits::read_from(dir.path(), "0::/kubepods/pod1/abc\n").expect("limits");
        assert_eq!(limits.dir, dir.path());
        assert_eq!(limits.memory_max, Some(1 << 30));

        let empty = tempfile::tempdir().expect("temporary directory");
        assert_eq!(Limits::read_from(empty.path(), "0::/\n"), None);
        assert_eq!(Limits::read_from(dir.path(), "1:cpu:/\n"), None);
    }

    #[test]
    fn oom_kills_are_counted_from_memory_events() {
        let dir = cgroup(&[(
            "memory.events",
            "low 0\nhigh 0\nmax 12\noom 2\noom_kill 2\noom_group_kill 0\n",
        )]);
        assert_eq!(oom_kills(dir.path()), Some(2));
    }
}
