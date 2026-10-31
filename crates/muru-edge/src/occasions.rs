//! Seasonal themes for the development traceback page.
//!
//! Around certain dates of the year the built-in HTML error page that
//! carries a traceback is re-themed: colours change and a small
//! decoration, usually animated, sits in a corner. Nothing here is
//! documented on purpose; reading this file is the intended way to find
//! it. The themes apply only in development mode, only to the built-in
//! HTML page, and only when a traceback is shown, so the daemon and
//! API clients never see them.
//!
//! An [`Occasion`] is a row of [`OCCASIONS`]: a rule saying when it
//! applies, an optional group of time zones it is limited to, a tier and
//! an enabled flag for the calendar, and the artwork as an HTML fragment
//! from the `occasions` directory beside `src`. [`select`] picks the
//! occasion for a moment in a zone, and [`Setting`] is what a run was
//! told on its command line: follow the calendar, show one occasion by
//! name, or show none.

use jiff::civil::{Date, Weekday};
use jiff::{Timestamp, Zoned};

/// How a theme's styles combine with the page's own stylesheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The page's stylesheet stays and the theme's styles layer on it.
    Append,
    /// The page's stylesheet is left out and only the theme's apply.
    Replace,
}

/// A group of IANA time zones an occasion is limited to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Zones {
    Everywhere,
    UnitedStates,
    UnitedKingdom,
}

/// Prefixes of the IANA names of United States zones. A bare `America/`
/// would take in Canada, Mexico and South America.
const US_ZONES: &[&str] = &[
    "America/New_York",
    "America/Detroit",
    "America/Kentucky/",
    "America/Indiana/",
    "America/Chicago",
    "America/Menominee",
    "America/North_Dakota/",
    "America/Denver",
    "America/Boise",
    "America/Phoenix",
    "America/Los_Angeles",
    "America/Anchorage",
    "America/Juneau",
    "America/Sitka",
    "America/Metlakatla",
    "America/Yakutat",
    "America/Nome",
    "America/Adak",
    "Pacific/Honolulu",
    "US/",
];

/// Prefixes of the IANA names of United Kingdom zones.
const UK_ZONES: &[&str] = &["Europe/London", "Europe/Belfast", "GB"];

impl Zones {
    /// Whether a zone, by IANA name, is in the group. A zone with no
    /// IANA name (a fixed offset, or a container pinned to UTC) is in no
    /// group but `Everywhere`.
    fn contains(self, zone: Option<&str>) -> bool {
        let prefixes = match self {
            Zones::Everywhere => return true,
            Zones::UnitedStates => US_ZONES,
            Zones::UnitedKingdom => UK_ZONES,
        };

        zone.is_some_and(|name| prefixes.iter().any(|prefix| name.starts_with(prefix)))
    }
}

/// When an occasion applies. Every rule resolves to whole local days.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// Every year from one month and day to another, inclusive; the range
    /// may wrap the year end. Feb 29 alone matches only in leap years.
    Yearly { from: (i8, i8), to: (i8, i8) },
    /// Every year, the nth weekday of a month; a negative `nth` counts
    /// from the end of the month, so -1 is the last.
    NthWeekday {
        month: i8,
        nth: i8,
        weekday: Weekday,
    },
    /// Every year, the nth day of the year.
    DayOfYear(i16),
    /// Any month whose given day falls on the given weekday.
    WeekdayOnDay { weekday: Weekday, day: i8 },
    /// Lunar new year's day and the days after it, from [`LUNAR_NEW_YEAR`].
    LunarNewYear { days: i16 },
    /// A range of days around Easter Sunday, as offsets from it.
    Easter { from: i16, to: i16 },
    /// One range of dates, inclusive, never repeated.
    Once { from: Date, to: Date },
    /// The local day that contains one instant, as seconds since the
    /// Unix epoch.
    Instant(i64),
}

/// Lunar new year's day for each year the table covers, from the Hong
/// Kong Observatory's conversion tables. A year outside the table matches
/// nothing.
const LUNAR_NEW_YEAR: &[(i16, i8, i8)] = &[
    (2026, 2, 17),
    (2027, 2, 6),
    (2028, 1, 26),
    (2029, 2, 13),
    (2030, 2, 3),
    (2031, 1, 23),
    (2032, 2, 11),
    (2033, 1, 31),
    (2034, 2, 19),
    (2035, 2, 8),
    (2036, 1, 28),
    (2037, 2, 15),
    (2038, 2, 4),
    (2039, 1, 24),
    (2040, 2, 12),
    (2041, 2, 1),
    (2042, 1, 22),
    (2043, 2, 10),
    (2044, 1, 30),
    (2045, 2, 17),
];

/// Lunar new year's day of a year, when the table has it.
fn lunar_new_year(year: i16) -> Option<Date> {
    LUNAR_NEW_YEAR
        .iter()
        .find(|(candidate, _, _)| *candidate == year)
        .and_then(|&(year, month, day)| Date::new(year, month, day).ok())
}

/// Easter Sunday of a year in the Gregorian calendar, by the anonymous
/// algorithm of 1876.
fn easter(year: i16) -> Option<Date> {
    let year = i32::from(year);
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;

    Date::new(year as i16, month as i8, day as i8).ok()
}

/// Days from one date to a later one in the same year, both included.
fn days_inclusive(from: Date, to: Date) -> i16 {
    to.day_of_year() - from.day_of_year() + 1
}

impl Rule {
    /// Whether the rule covers `now`, and if so how many days the match
    /// spans, which decides between occasions of one tier: the shorter
    /// span is the more specific.
    fn matches(self, now: &Zoned) -> Option<i16> {
        let today = now.date();
        let year = today.year();

        match self {
            Rule::Yearly { from, to } => {
                let start = Date::new(year, from.0, from.1).ok()?;
                let end = Date::new(year, to.0, to.1).ok()?;

                if start <= end {
                    (start <= today && today <= end).then(|| days_inclusive(start, end))
                } else {
                    // The range wraps the year end: Dec 31 to Jan 1 is
                    // two days, one at each end of the year.

                    let year_end = Date::new(year, 12, 31).ok()?;
                    let year_start = Date::new(year, 1, 1).ok()?;
                    let span = days_inclusive(start, year_end) + days_inclusive(year_start, end);
                    (today >= start || today <= end).then_some(span)
                }
            }
            Rule::NthWeekday {
                month,
                nth,
                weekday,
            } => {
                let first = Date::new(year, month, 1).ok()?;
                let day = first.nth_weekday_of_month(nth, weekday).ok()?;
                (day == today).then_some(1)
            }
            Rule::DayOfYear(day) => (today.day_of_year() == day).then_some(1),
            Rule::WeekdayOnDay { weekday, day } => {
                (today.weekday() == weekday && today.day() == day).then_some(1)
            }
            Rule::LunarNewYear { days } => {
                let start = lunar_new_year(year)?;
                let offset = today.day_of_year() - start.day_of_year();
                (0..days).contains(&offset).then_some(days)
            }
            Rule::Easter { from, to } => {
                let sunday = easter(year)?;
                let offset = today.day_of_year() - sunday.day_of_year();
                (from..=to).contains(&offset).then(|| to - from + 1)
            }
            Rule::Once { from, to } => {
                (from <= today && today <= to).then(|| days_inclusive(from, to))
            }
            Rule::Instant(seconds) => {
                let instant = Timestamp::from_second(seconds).ok()?;
                let local = instant.to_zoned(now.time_zone().clone()).date();
                (local == today).then_some(1)
            }
        }
    }
}

/// One themed occasion.
#[derive(Debug)]
pub struct Occasion {
    /// The name the command line option and the page marker use.
    pub id: &'static str,
    rule: Rule,
    zones: Zones,
    /// 3 for a date that comes once, 2 for a seasonal occasion, 1 for
    /// the day list; the highest tier that matches wins.
    tier: u8,
    /// Whether the calendar may choose it. A disabled occasion is still
    /// shown when named on the command line.
    enabled: bool,
    /// How the theme's styles combine with the page's.
    pub mode: Mode,
    /// The theme: a `<style>` element and a decorative element, or
    /// nothing for a page served bare.
    pub fragment: &'static str,
}

/// The styles every theme with a fragment shares: where the decoration
/// sits and when it is hidden.
pub const BASE_STYLES: &str = include_str!("../occasions/base.css");

macro_rules! fragment {
    ($name:literal) => {
        include_str!(concat!("../occasions/", $name, ".html"))
    };
}

const fn yearly(from: (i8, i8), to: (i8, i8)) -> Rule {
    Rule::Yearly { from, to }
}

const fn day(month: i8, day: i8) -> Rule {
    Rule::Yearly {
        from: (month, day),
        to: (month, day),
    }
}

const fn nth(month: i8, nth: i8, weekday: Weekday) -> Rule {
    Rule::NthWeekday {
        month,
        nth,
        weekday,
    }
}

const fn once(year: i16, month: i8, day: i8) -> Rule {
    let date = Date::constant(year, month, day);
    Rule::Once {
        from: date,
        to: date,
    }
}

/// An occasion the calendar may choose, open to every zone.
const fn seasonal(id: &'static str, rule: Rule, fragment: &'static str) -> Occasion {
    Occasion {
        id,
        rule,
        zones: Zones::Everywhere,
        tier: 2,
        enabled: true,
        mode: Mode::Append,
        fragment,
    }
}

/// An occasion from the day list, open to every zone.
const fn daily(id: &'static str, rule: Rule, fragment: &'static str) -> Occasion {
    Occasion {
        id,
        rule,
        zones: Zones::Everywhere,
        tier: 1,
        enabled: true,
        mode: Mode::Append,
        fragment,
    }
}

/// A date that comes once, open to every zone.
const fn unique(id: &'static str, rule: Rule, fragment: &'static str) -> Occasion {
    Occasion {
        id,
        rule,
        zones: Zones::Everywhere,
        tier: 3,
        enabled: true,
        mode: Mode::Append,
        fragment,
    }
}

const fn zoned(occasion: Occasion, zones: Zones) -> Occasion {
    Occasion { zones, ..occasion }
}

const fn replacing(occasion: Occasion) -> Occasion {
    Occasion {
        mode: Mode::Replace,
        ..occasion
    }
}

/// Every occasion: the seasonal ones, then the geek and computing days,
/// then the dates that come once. The order breaks ties within a tier
/// and span.
pub static OCCASIONS: &[Occasion] = &[
    seasonal("new-year", yearly((12, 31), (1, 1)), fragment!("new-year")),
    seasonal("australia-day", day(1, 26), fragment!("australia-day")),
    seasonal("cny", Rule::LunarNewYear { days: 3 }, fragment!("cny")),
    seasonal(
        "easter",
        Rule::Easter { from: -2, to: 1 },
        fragment!("easter"),
    ),
    zoned(
        seasonal("independence-day", day(7, 4), fragment!("independence-day")),
        Zones::UnitedStates,
    ),
    seasonal("halloween", day(10, 31), fragment!("halloween")),
    zoned(
        seasonal("guy-fawkes", day(11, 5), fragment!("guy-fawkes")),
        Zones::UnitedKingdom,
    ),
    zoned(
        seasonal(
            "thanksgiving",
            nth(11, 4, Weekday::Thursday),
            fragment!("thanksgiving"),
        ),
        Zones::UnitedStates,
    ),
    seasonal(
        "christmas",
        yearly((12, 24), (12, 26)),
        fragment!("christmas"),
    ),
    seasonal("star-wars", day(5, 4), fragment!("star-wars")),
    daily("pi-day", day(3, 14), fragment!("pi-day")),
    daily("towel-day", day(5, 25), fragment!("towel-day")),
    daily("tau-day", day(6, 28), fragment!("tau-day")),
    daily("snake-day", day(7, 16), fragment!("snake-day")),
    daily("emoji-day", day(7, 17), fragment!("emoji-day")),
    daily("pirate-day", day(9, 19), fragment!("pirate-day")),
    daily("forty-two", day(10, 10), fragment!("forty-two")),
    replacing(daily(
        "caps-lock-day",
        day(10, 22),
        fragment!("caps-lock-day"),
    )),
    daily(
        "friday-13th",
        Rule::WeekdayOnDay {
            weekday: Weekday::Friday,
            day: 13,
        },
        fragment!("friday-13th"),
    ),
    daily("leap-day", day(2, 29), fragment!("leap-day")),
    daily("privacy-day", day(1, 28), fragment!("privacy-day")),
    daily(
        "safer-internet-day",
        nth(2, 2, Weekday::Tuesday),
        fragment!("safer-internet-day"),
    ),
    daily("eniac", day(2, 14), fragment!("eniac")),
    daily("python-birthday", day(2, 20), fragment!("python-birthday")),
    daily("web-birthday", day(3, 12), fragment!("web-birthday")),
    daily("backup-day", day(3, 31), fragment!("backup-day")),
    replacing(daily("css-naked-day", day(4, 9), "")),
    daily(
        "password-day",
        nth(5, 1, Weekday::Thursday),
        fragment!("password-day"),
    ),
    replacing(daily(
        "accessibility-day",
        nth(5, 3, Weekday::Thursday),
        fragment!("accessibility-day"),
    )),
    daily("ipv6-day", day(6, 6), fragment!("ipv6-day")),
    daily("turing", day(6, 23), fragment!("turing")),
    daily(
        "sysadmin-day",
        nth(7, -1, Weekday::Friday),
        fragment!("sysadmin-day"),
    ),
    daily("linux-birthday", day(8, 25), fragment!("linux-birthday")),
    daily("first-bug", day(9, 9), fragment!("first-bug")),
    daily(
        "programmers-day",
        Rule::DayOfYear(256),
        fragment!("programmers-day"),
    ),
    daily(
        "software-freedom-day",
        nth(9, 3, Weekday::Saturday),
        fragment!("software-freedom-day"),
    ),
    daily(
        "ada-lovelace-day",
        nth(10, 2, Weekday::Tuesday),
        fragment!("ada-lovelace-day"),
    ),
    daily("internet-day", day(10, 29), fragment!("internet-day")),
    daily("security-day", day(11, 30), fragment!("security-day")),
    daily("wsgi-birthday", day(12, 7), fragment!("wsgi-birthday")),
    daily("hopper", day(12, 9), fragment!("hopper")),
    daily("lovelace", day(12, 10), fragment!("lovelace")),
    unique(
        "unix-two-billion",
        Rule::Instant(2_000_000_000),
        fragment!("unix-two-billion"),
    ),
    unique("y2038", Rule::Instant(i32::MAX as i64), fragment!("y2038")),
    unique(
        "square-root-day",
        once(2036, 6, 6),
        fragment!("square-root-day"),
    ),
];

/// The occasion for a moment: of those enabled whose rule covers the
/// local day and whose zones include the zone, the highest tier, then
/// the shortest span, then the first in the table.
pub fn select(now: &Zoned) -> Option<&'static Occasion> {
    let zone = now.time_zone().iana_name();
    let mut best: Option<(&Occasion, i16)> = None;

    for occasion in OCCASIONS {
        if !occasion.enabled || !occasion.zones.contains(zone) {
            continue;
        }

        let Some(span) = occasion.rule.matches(now) else {
            continue;
        };

        let better = match best {
            None => true,
            Some((current, current_span)) => {
                occasion.tier > current.tier
                    || (occasion.tier == current.tier && span < current_span)
            }
        };

        if better {
            best = Some((occasion, span));
        }
    }

    best.map(|(occasion, _)| occasion)
}

/// The occasion with an id.
pub fn named(id: &str) -> Option<&'static Occasion> {
    OCCASIONS.iter().find(|occasion| occasion.id == id)
}

/// What a run was told about occasions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Setting {
    /// Follow the calendar in the machine's zone.
    #[default]
    Calendar,
    /// Show no occasion.
    Off,
    /// Show one occasion, by id, whatever the date.
    Forced(&'static str),
}

impl Setting {
    /// Read a command line value: `off`, or an occasion's id. Anything
    /// else is refused with the list of ids, the one place it is printed.
    pub fn parse(value: &str) -> Result<Self, String> {
        if value == "off" {
            return Ok(Setting::Off);
        }

        match named(value) {
            Some(occasion) => Ok(Setting::Forced(occasion.id)),
            None => {
                let ids: Vec<&str> = OCCASIONS.iter().map(|occasion| occasion.id).collect();
                Err(format!(
                    "{value} is not an occasion; they are off, {}",
                    ids.join(", ")
                ))
            }
        }
    }

    /// The occasion to show now, if any.
    pub fn theme(self) -> Option<&'static Occasion> {
        match self {
            Setting::Calendar => select(&Zoned::now()),
            Setting::Off => None,
            Setting::Forced(id) => named(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn at(year: i16, month: i8, day: i8, zone: &str) -> Zoned {
        date(year, month, day)
            .at(12, 0, 0, 0)
            .in_tz(zone)
            .expect("a zone the tests rely on")
    }

    fn chosen(year: i16, month: i8, day: i8) -> Option<&'static str> {
        select(&at(year, month, day, "Australia/Sydney")).map(|occasion| occasion.id)
    }

    #[test]
    fn ids_are_unique_and_fragments_are_present() {
        for (index, occasion) in OCCASIONS.iter().enumerate() {
            assert!(
                OCCASIONS[..index]
                    .iter()
                    .all(|other| other.id != occasion.id),
                "{} appears twice",
                occasion.id
            );
            assert!(
                !occasion.fragment.is_empty() || occasion.mode == Mode::Replace,
                "{} has no theme",
                occasion.id
            );
            assert!(
                occasion.fragment.len() <= 8 * 1024,
                "{} is over budget",
                occasion.id
            );
            assert!(!occasion.fragment.contains('\u{2014}'), "{}", occasion.id);
        }
    }

    #[test]
    fn easter_sundays_are_known() {
        assert_eq!(easter(2026), Some(date(2026, 4, 5)));
        assert_eq!(easter(2027), Some(date(2027, 3, 28)));
        assert_eq!(easter(2030), Some(date(2030, 4, 21)));
        assert_eq!(easter(2038), Some(date(2038, 4, 25)));
    }

    #[test]
    fn the_lunar_table_is_contiguous_and_plausible() {
        for (index, &(year, month, day)) in LUNAR_NEW_YEAR.iter().enumerate() {
            assert_eq!(year, LUNAR_NEW_YEAR[0].0 + index as i16);
            let date = date(year, month, day);
            assert!(date >= date.with().month(1).day(21).build().unwrap());
            assert!(date <= date.with().month(2).day(20).build().unwrap());
        }
    }

    #[test]
    fn fixed_days_and_ranges_match() {
        assert_eq!(chosen(2026, 10, 31), Some("halloween"));
        assert_eq!(chosen(2026, 12, 24), Some("christmas"));
        assert_eq!(chosen(2026, 12, 26), Some("christmas"));
        assert_eq!(chosen(2026, 12, 27), None);
        assert_eq!(chosen(2026, 12, 31), Some("new-year"));
        assert_eq!(chosen(2027, 1, 1), Some("new-year"));
        assert_eq!(chosen(2027, 1, 2), None);
        assert_eq!(chosen(2028, 2, 29), Some("leap-day"));
        assert_eq!(chosen(2027, 3, 1), None);
    }

    #[test]
    fn computed_days_match() {
        assert_eq!(chosen(2026, 7, 31), Some("sysadmin-day"));
        assert_eq!(chosen(2027, 7, 30), Some("sysadmin-day"));
        assert_eq!(chosen(2026, 9, 13), Some("programmers-day"));
        assert_eq!(chosen(2028, 9, 12), Some("programmers-day"));
        assert_eq!(chosen(2026, 11, 13), Some("friday-13th"));
        assert_eq!(chosen(2026, 1, 13), None);
        assert_eq!(chosen(2026, 2, 17), Some("cny"));
        assert_eq!(chosen(2026, 2, 19), Some("cny"));
        assert_eq!(chosen(2026, 2, 20), Some("python-birthday"));
        assert_eq!(chosen(2026, 4, 3), Some("easter"));
        assert_eq!(chosen(2026, 4, 6), Some("easter"));
        assert_eq!(chosen(2026, 4, 7), None);
        assert_eq!(chosen(2026, 10, 13), Some("ada-lovelace-day"));
        assert_eq!(chosen(2026, 10, 14), None);
        assert_eq!(chosen(2027, 10, 12), Some("ada-lovelace-day"));
    }

    #[test]
    fn a_higher_tier_then_a_shorter_span_then_table_order_wins() {
        // Easter 2026 runs Apr 3 to Apr 6; nothing from the day list
        // falls in it, so use 2029, when Good Friday is Mar 30 and World
        // Backup Day on Mar 31 falls inside the Easter weekend.

        assert_eq!(easter(2029), Some(date(2029, 4, 1)));
        assert_eq!(chosen(2029, 3, 31), Some("easter"));

        // Lunar new year 2028 starts on Jan 26: the one-day Australia Day
        // beats the three-day occasion of the same tier.

        assert_eq!(chosen(2028, 1, 26), Some("australia-day"));
        assert_eq!(chosen(2028, 1, 27), Some("cny"));
    }

    #[test]
    fn zoned_occasions_need_their_zone() {
        let fourth = |zone| select(&at(2026, 7, 4, zone)).map(|occasion| occasion.id);
        assert_eq!(fourth("America/Chicago"), Some("independence-day"));
        assert_eq!(fourth("Pacific/Honolulu"), Some("independence-day"));
        assert_eq!(fourth("America/Toronto"), None);
        assert_eq!(fourth("Australia/Sydney"), None);
        assert_eq!(fourth("UTC"), None);

        let fifth = |zone| select(&at(2026, 11, 5, zone)).map(|occasion| occasion.id);
        assert_eq!(fifth("Europe/London"), Some("guy-fawkes"));
        assert_eq!(fifth("Europe/Dublin"), None);

        let thursday = |zone| select(&at(2026, 11, 26, zone)).map(|occasion| occasion.id);
        assert_eq!(thursday("America/New_York"), Some("thanksgiving"));
        assert_eq!(thursday("Europe/London"), None);
    }

    #[test]
    fn instants_fall_on_the_local_day() {
        // 2038-01-19 03:14:07 UTC is the same day in Sydney (14:14) and
        // the evening before in Honolulu (17:14 on the 18th).

        assert_eq!(chosen(2038, 1, 19), Some("y2038"));
        assert_eq!(chosen(2038, 1, 18), None);
        let honolulu = |day| select(&at(2038, 1, day, "Pacific/Honolulu")).map(|o| o.id);
        assert_eq!(honolulu(18), Some("y2038"));
        assert_eq!(honolulu(19), None);
        assert_eq!(chosen(2033, 5, 18), Some("unix-two-billion"));
        assert_eq!(chosen(2036, 6, 6), Some("square-root-day"));
    }

    #[test]
    fn the_setting_parses_ids_and_off() {
        assert_eq!(Setting::parse("off"), Ok(Setting::Off));
        assert_eq!(
            Setting::parse("christmas"),
            Ok(Setting::Forced("christmas"))
        );
        let error = Setting::parse("bogus").unwrap_err();
        assert!(error.starts_with("bogus is not an occasion"));
        assert!(error.contains("christmas"));
        assert!(error.contains("sysadmin-day"));
        assert!(Setting::Off.theme().is_none());
        assert_eq!(
            Setting::Forced("css-naked-day").theme().map(|o| o.mode),
            Some(Mode::Replace)
        );
    }
}
