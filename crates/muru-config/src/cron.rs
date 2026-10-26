//! Cron expressions for recurring windows (sections 7.5 and 9.10): the
//! five fields minute, hour, day of month, month and day of week, read as
//! local times in a time zone.

use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;

/// How far a search for an occurrence looks, in days: eight years, so a
/// day that only some leap years have is still found.
const SEARCH_DAYS: usize = 366 * 8 + 1;

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// A cron expression: the minutes, hours, days, months and weekdays it
/// names, as bit sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    any_day: bool,
    any_weekday: bool,
}

impl Cron {
    /// Read an expression: five fields of numbers, `*`, ranges `a-b`,
    /// steps `/n` and lists `a,b`, with month and weekday names (jan,
    /// mon) and Sunday as 0 or 7; or one of @hourly, @daily, @weekly,
    /// @monthly and @yearly. When both the day of month and the day of
    /// week are restricted, a day matching either matches, as in cron.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();

        let expanded = match text {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };

        let fields: Vec<&str> = expanded.split_whitespace().collect();

        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return Err(format!(
                "{text:?} is not a cron expression: five fields, minute hour day month weekday"
            ));
        };

        let weekdays = field(weekday, 0, 7, &WEEKDAYS, "weekday")?;

        // Sunday is 0 and 7.

        let weekdays = (weekdays | (weekdays >> 7)) & 0x7f;

        Ok(Self {
            minutes: field(minute, 0, 59, &[], "minute")?,
            hours: field(hour, 0, 23, &[], "hour")?,
            days: field(day, 1, 31, &[], "day of month")?,
            months: field(month, 1, 12, &MONTHS, "month")?,
            weekdays,
            any_day: day.starts_with('*'),
            any_weekday: weekday.starts_with('*'),
        })
    }

    /// The first occurrence after `after`, if there is one within eight
    /// years. A local time a daylight saving change skips or repeats
    /// takes the later or earlier instant, as Temporal's compatible rule
    /// does.
    pub fn next(&self, after: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
        let local = after.to_zoned(zone.clone()).datetime();
        let floor = local.with().second(0).subsec_nanosecond(0).build().ok()?;
        let mut date = local.date();

        for _ in 0..SEARCH_DAYS {
            if self.day_matches(date) {
                for hour in (0..24).filter(|hour| bit(self.hours, *hour)) {
                    for minute in (0..60).filter(|minute| bit(self.minutes, *minute)) {
                        let civil = date.at(hour as i8, minute as i8, 0, 0);

                        if civil < floor {
                            continue;
                        }

                        if let Some(instant) = instant(civil, zone)
                            && instant > after
                        {
                            return Some(instant);
                        }
                    }
                }
            }

            date = date.tomorrow().ok()?;
        }

        None
    }

    /// The last occurrence at or before `at`, if there is one within
    /// eight years.
    pub fn previous(&self, at: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
        let local = at.to_zoned(zone.clone()).datetime();
        let mut date = local.date();

        for _ in 0..SEARCH_DAYS {
            if self.day_matches(date) {
                for hour in (0..24).rev().filter(|hour| bit(self.hours, *hour)) {
                    for minute in (0..60).rev().filter(|minute| bit(self.minutes, *minute)) {
                        let civil = date.at(hour as i8, minute as i8, 0, 0);

                        if civil > local {
                            continue;
                        }

                        if let Some(instant) = instant(civil, zone)
                            && instant <= at
                        {
                            return Some(instant);
                        }
                    }
                }
            }

            date = date.yesterday().ok()?;
        }

        None
    }

    fn day_matches(&self, date: Date) -> bool {
        if !bit(self.months, date.month() as u32) {
            return false;
        }

        let day = bit(self.days, date.day() as u32);
        let weekday = bit(self.weekdays, date.weekday().to_sunday_zero_offset() as u32);

        match (self.any_day, self.any_weekday) {
            (true, true) => true,
            (true, false) => weekday,
            (false, true) => day,
            (false, false) => day || weekday,
        }
    }
}

fn bit(set: u64, value: u32) -> bool {
    set & (1 << value) != 0
}

fn instant(civil: DateTime, zone: &TimeZone) -> Option<Timestamp> {
    civil
        .to_zoned(zone.clone())
        .ok()
        .map(|zoned| zoned.timestamp())
}

/// One field as a bit set of the values it names, from `min` to `max`.
fn field(text: &str, min: u32, max: u32, names: &[&str], what: &str) -> Result<u64, String> {
    let mut set = 0u64;

    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step.parse().ok().filter(|step| *step > 0).ok_or_else(|| {
                    format!("{what} {part:?}: the step must be a positive number")
                })?;
                (range, Some(step))
            }
            None => (part, None),
        };

        let (low, high) = if range == "*" {
            (min, max)
        } else if let Some((low, high)) = range.split_once('-') {
            (value(low, names, what)?, value(high, names, what)?)
        } else {
            let low = value(range, names, what)?;
            (low, if step.is_some() { max } else { low })
        };

        if low < min || high > max || low > high {
            return Err(format!("{what} {part:?}: values run from {min} to {max}"));
        }

        for value in (low..=high).step_by(step.unwrap_or(1) as usize) {
            set |= 1 << value;
        }
    }

    Ok(set)
}

/// A number, or a month or weekday name, which counts from the first
/// value the field takes.
fn value(text: &str, names: &[&str], what: &str) -> Result<u32, String> {
    if let Ok(number) = text.parse() {
        return Ok(number);
    }

    let lower = text.to_ascii_lowercase();
    let first = if names.len() == 12 { 1 } else { 0 };

    names
        .iter()
        .position(|name| *name == lower)
        .map(|index| index as u32 + first)
        .ok_or_else(|| format!("{what} {text:?} is not a number or a name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn sydney() -> TimeZone {
        TimeZone::get("Australia/Sydney").unwrap()
    }

    #[test]
    fn fields_take_ranges_steps_lists_and_names() {
        let cron = Cron::parse("*/15 9-17 * jan,jul mon-fri").unwrap();
        assert_eq!(cron.minutes, 1 | 1 << 15 | 1 << 30 | 1 << 45);
        assert_eq!(cron.hours.count_ones(), 9);
        assert_eq!(cron.months, 1 << 1 | 1 << 7);
        assert_eq!(cron.weekdays, 0b0111110);
        assert_eq!(Cron::parse("0 0 * * 7").unwrap().weekdays, 1);
        assert_eq!(
            Cron::parse("@daily").unwrap(),
            Cron::parse("0 0 * * *").unwrap()
        );

        for bad in [
            "0 0 * *",
            "60 * * * *",
            "* * 0 * *",
            "*/0 * * * *",
            "* * * smarch *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_next_and_previous_occurrences_are_local_times() {
        let nightly = Cron::parse("0 2 * * *").unwrap();
        let utc = TimeZone::UTC;
        let now = at("2026-11-02T10:00:00Z");
        assert_eq!(nightly.next(now, &utc), Some(at("2026-11-03T02:00:00Z")));
        assert_eq!(
            nightly.previous(now, &utc),
            Some(at("2026-11-02T02:00:00Z"))
        );

        // 02:00 in Sydney is 15:00 UTC the day before, in daylight time.

        assert_eq!(
            nightly.next(now, &sydney()),
            Some(at("2026-11-02T15:00:00Z"))
        );

        // An occurrence at the very moment counts as previous, not next.

        let exact = at("2026-11-03T02:00:00Z");
        assert_eq!(nightly.previous(exact, &utc), Some(exact));
        assert_eq!(nightly.next(exact, &utc), Some(at("2026-11-04T02:00:00Z")));
    }

    #[test]
    fn day_of_month_and_weekday_match_either_when_both_are_set() {
        let cron = Cron::parse("0 0 13 * fri").unwrap();
        let utc = TimeZone::UTC;

        // 2026-11-06 is a Friday, before the 13th.

        assert_eq!(
            cron.next(at("2026-11-01T00:00:00Z"), &utc),
            Some(at("2026-11-06T00:00:00Z"))
        );

        let leap = Cron::parse("0 0 29 feb *").unwrap();
        assert_eq!(
            leap.next(at("2026-03-01T00:00:00Z"), &utc),
            Some(at("2028-02-29T00:00:00Z"))
        );
        assert_eq!(
            Cron::parse("0 0 31 feb *")
                .unwrap()
                .next(at("2026-01-01T00:00:00Z"), &utc),
            None
        );
    }

    #[test]
    fn daylight_saving_changes_take_the_compatible_instant() {
        // Sydney skips 02:00 to 03:00 on 2026-10-04: 02:30 becomes 03:30
        // daylight time, 16:30 UTC.

        let cron = Cron::parse("30 2 * * *").unwrap();
        assert_eq!(
            cron.next(at("2026-10-03T12:00:00Z"), &sydney()),
            Some(at("2026-10-03T16:30:00Z"))
        );

        // It repeats 02:00 to 03:00 on 2026-04-05: 02:30 is the earlier,
        // daylight one, 15:30 UTC, and only that one.

        assert_eq!(
            cron.next(at("2026-04-04T12:00:00Z"), &sydney()),
            Some(at("2026-04-04T15:30:00Z"))
        );
        assert_eq!(
            cron.next(at("2026-04-04T15:30:00Z"), &sydney()),
            Some(at("2026-04-05T16:30:00Z"))
        );
    }
}
