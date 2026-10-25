//! Built-in log rotation (design specification section 11.3): the daemon
//! renames a log that has reached its size or its period, has every
//! writer reopen its file by name, compresses the renamed file in the
//! background and keeps a bounded number of archives.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// When a log is rotated by the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Every {
    /// Never by the clock; by size alone.
    Off,
    /// At the top of every hour, UTC.
    Hourly,
    /// At midnight, UTC.
    Daily,
    /// At midnight on Mondays, UTC.
    Weekly,
}

/// How archives are compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compress {
    /// Not at all.
    Off,
    /// With zstd, as `.zst`.
    Zstd,
    /// With gzip, as `.gz`.
    Gzip,
}

/// The rotation policy of the daemon's `[logs]` table, with the defaults
/// of appendix C: daily, fourteen archives, zstd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Rotate when a file reaches this many bytes, when set.
    pub size: Option<u64>,
    /// Rotate by the clock.
    pub every: Every,
    /// Archives kept per log.
    pub keep: usize,
    /// How archives are compressed.
    pub compress: Compress,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            size: None,
            every: Every::Daily,
            keep: 14,
            compress: Compress::Zstd,
        }
    }
}

impl Policy {
    /// The policy a `[logs]` table describes, or none when rotation is
    /// external.
    pub fn from_table(table: Option<&muru_config::Logs>) -> anyhow::Result<Option<Self>> {
        let Some(table) = table else {
            return Ok(Some(Self::default()));
        };

        match table.rotate.as_deref() {
            None | Some("built-in") => {}
            Some("external") => return Ok(None),
            Some(other) => {
                anyhow::bail!("[logs] rotate must be built-in or external, not {other:?}")
            }
        }

        let defaults = Self::default();
        Ok(Some(Self {
            size: match &table.size {
                Some(text) => Some(
                    muru_config::parse_size(text)
                        .map_err(|error| anyhow::anyhow!("[logs] size: {error}"))?,
                ),
                None => None,
            },
            every: match table.every.as_deref() {
                None => defaults.every,
                Some("off") => Every::Off,
                Some("hourly") => Every::Hourly,
                Some("daily") => Every::Daily,
                Some("weekly") => Every::Weekly,
                Some(other) => anyhow::bail!(
                    "[logs] every must be off, hourly, daily or weekly, not {other:?}"
                ),
            },
            keep: table.keep.unwrap_or(defaults.keep),
            compress: match table.compress.as_deref() {
                None => defaults.compress,
                Some("off") => Compress::Off,
                Some("zstd") => Compress::Zstd,
                Some("gzip") => Compress::Gzip,
                Some(other) => {
                    anyhow::bail!("[logs] compress must be off, zstd or gzip, not {other:?}")
                }
            },
        }))
    }
}

/// Decides when each log is due and rotates it.
pub struct Rotator {
    policy: Policy,
    /// The period each file was last seen in, so a change of period
    /// rotates it once.
    periods: HashMap<PathBuf, String>,
}

impl Rotator {
    /// A rotator for the policy, starting now: nothing is due for the
    /// clock until the period changes.
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            periods: HashMap::new(),
        }
    }

    /// Whether a file is due: it has reached the size, or the period has
    /// changed since it was last seen. A file seen for the first time
    /// only notes its period.
    pub fn due(&mut self, path: &Path, now: SystemTime) -> bool {
        let period = period(self.policy.every, now);
        let previous = self.periods.insert(path.to_path_buf(), period.clone());
        let by_clock = previous.is_some_and(|previous| previous != period);
        let by_size = self.policy.size.is_some_and(|limit| {
            std::fs::metadata(path).is_ok_and(|metadata| metadata.len() >= limit)
        });

        by_clock || by_size
    }

    /// Rotate a file: rename it with a timestamp, compress the archive in
    /// the background, and keep only the newest archives. Returns the
    /// archive's path, before compression.
    pub fn rotate(&self, path: &Path, now: SystemTime) -> std::io::Result<PathBuf> {
        let stamp = stamp(now);
        let mut archive = path.as_os_str().to_owned();
        archive.push(format!(".{stamp}"));
        let archive = PathBuf::from(archive);
        std::fs::rename(path, &archive)?;
        let policy = self.policy.clone();
        let base = path.to_path_buf();
        let renamed = archive.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = compress(&renamed, policy.compress) {
                tracing::warn!(%error, path = %renamed.display(), "compressing a rotated log failed");
            }

            prune(&base, policy.keep);
        });
        Ok(archive)
    }
}

/// The period a time falls in, as a key: the hour, the day or the week.
fn period(every: Every, now: SystemTime) -> String {
    let seconds = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let days = seconds / 86_400;

    match every {
        Every::Off => String::new(),
        Every::Hourly => format!("h{}", seconds / 3600),
        Every::Daily => format!("d{days}"),
        // 1970-01-01 was a Thursday: shift so weeks start on Monday.
        Every::Weekly => format!("w{}", (days + 3) / 7),
    }
}

/// A timestamp for an archive's name: YYYYMMDD-HHMMSS, UTC.
fn stamp(now: SystemTime) -> String {
    let seconds = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let (year, month, day) = civil(seconds / 86_400);
    let rest = seconds % 86_400;
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// The civil date of a day count since 1970-01-01 (Howard Hinnant's
/// algorithm).
fn civil(days: u64) -> (u64, u64, u64) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year as u64, month as u64, day as u64)
}

/// Compress an archive in place, replacing it with the compressed file.
fn compress(archive: &Path, how: Compress) -> std::io::Result<()> {
    let suffix = match how {
        Compress::Off => return Ok(()),
        Compress::Zstd => "zst",
        Compress::Gzip => "gz",
    };

    let mut target = archive.as_os_str().to_owned();
    target.push(format!(".{suffix}"));
    let target = PathBuf::from(target);
    let mut input = std::fs::File::open(archive)?;
    let output = std::fs::File::create(&target)?;

    match how {
        Compress::Zstd => {
            let mut encoder = zstd::stream::write::Encoder::new(output, 3)?;
            std::io::copy(&mut input, &mut encoder)?;
            encoder.finish()?;
        }
        Compress::Gzip => {
            let mut encoder = flate2::write::GzEncoder::new(output, flate2::Compression::default());
            std::io::copy(&mut input, &mut encoder)?;
            encoder.finish()?;
        }
        Compress::Off => {}
    }

    std::fs::remove_file(archive)
}

/// Keep only the newest `keep` archives of a log, by name, which sorts
/// by time since the stamp leads.
fn prune(base: &Path, keep: usize) {
    let Some(directory) = base.parent() else {
        return;
    };
    let Some(name) = base
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return;
    };

    let prefix = format!("{name}.");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut archives: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|candidate| candidate.starts_with(&prefix) && candidate.len() > prefix.len())
        .filter(|candidate| candidate[prefix.len()..].starts_with(|c: char| c.is_ascii_digit()))
        .collect();
    archives.sort();

    while archives.len() > keep {
        let oldest = archives.remove(0);
        let _ = std::fs::remove_file(directory.join(oldest));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_and_periods_follow_the_clock() {
        let moment = SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_000_000);
        assert_eq!(stamp(moment), "20261026-073320");
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_723), (2024, 1, 1));
        let later = moment + Duration::from_secs(3600);
        assert_ne!(period(Every::Hourly, moment), period(Every::Hourly, later));
        assert_eq!(period(Every::Daily, moment), period(Every::Daily, later));
        assert_eq!(period(Every::Off, moment), "");
    }

    #[test]
    fn a_file_is_due_by_size_or_by_a_change_of_period() {
        let dir = std::env::temp_dir().join(format!("muru-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("a.log");
        std::fs::write(&log, "x".repeat(10)).unwrap();
        let mut rotator = Rotator::new(Policy {
            size: Some(20),
            every: Every::Hourly,
            keep: 2,
            compress: Compress::Off,
        });
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_000_000);
        assert!(!rotator.due(&log, now), "first sight only notes the period");
        std::fs::write(&log, "x".repeat(25)).unwrap();
        assert!(rotator.due(&log, now), "over the size");
        std::fs::write(&log, "x".repeat(5)).unwrap();
        assert!(
            rotator.due(&log, now + Duration::from_secs(3600)),
            "a new hour"
        );
        assert!(!rotator.due(&log, now + Duration::from_secs(3601)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pruning_keeps_the_newest_archives() {
        let dir = std::env::temp_dir().join(format!("muru-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("b.log");

        for stamp in [
            "20260101-000000.zst",
            "20260102-000000.zst",
            "20260103-000000",
        ] {
            std::fs::write(dir.join(format!("b.log.{stamp}")), "x").unwrap();
        }

        std::fs::write(dir.join("b.log"), "current").unwrap();
        std::fs::write(dir.join("other.log.20260101-000000"), "x").unwrap();
        prune(&log, 2);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "b.log",
                "b.log.20260102-000000.zst",
                "b.log.20260103-000000",
                "other.log.20260101-000000"
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
