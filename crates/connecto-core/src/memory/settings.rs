//! A project's memory settings: the `memory` key of its `settings.json`
//! (spec §8.1). Reading and writing the file is
//! `connecto_session::project_settings`; this module holds the values, their
//! defaults and their limits.

use std::collections::BTreeSet;
use std::fmt;
use std::ops::RangeInclusive;

use super::{ModelChoice, Provider};

word_enum! {
    /// A day of the week, Monday first.
    pub enum Weekday {
        Mon => "mon", "Monday";
        Tue => "tue", "Tuesday";
        Wed => "wed", "Wednesday";
        Thu => "thu", "Thursday";
        Fri => "fri", "Friday";
        Sat => "sat", "Saturday";
        Sun => "sun", "Sunday";
    }
}

/// A local time of day, `HH:MM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalTime {
    hour: u8,
    minute: u8,
}

impl LocalTime {
    /// `None` outside 00:00 ..= 23:59.
    #[must_use]
    pub const fn new(hour: u8, minute: u8) -> Option<Self> {
        if hour < 24 && minute < 60 {
            Some(Self { hour, minute })
        } else {
            None
        }
    }

    /// Exactly `HH:MM`, two digits each.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (h, m) = text.split_once(':')?;
        let two = |s: &str| {
            (s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse::<u8>().ok())
                .flatten()
        };
        Self::new(two(h)?, two(m)?)
    }

    #[must_use]
    pub const fn hour(self) -> u8 {
        self.hour
    }

    #[must_use]
    pub const fn minute(self) -> u8 {
        self.minute
    }
}

impl fmt::Display for LocalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

/// A local calendar date: the scheduler's idempotence key with the program
/// (spec D11), and how skipped runs are named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalDate {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

impl LocalDate {
    /// Read `YYYY-MM-DD`. Checks the day against the month (leap years
    /// included).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('-');
        let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
            return None;
        }
        let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
        if !(digits(y) && digits(m) && digits(d)) {
            return None;
        }
        let date = Self {
            year: y.parse().ok()?,
            month: m.parse().ok()?,
            day: d.parse().ok()?,
        };
        date.is_valid().then_some(date)
    }

    #[must_use]
    pub fn is_valid(self) -> bool {
        let leap = (self.year % 4 == 0 && self.year % 100 != 0) || self.year % 400 == 0;
        let days = match self.month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return false,
        };
        (1..=days).contains(&self.day)
    }
}

impl fmt::Display for LocalDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// The smallest input limit of any run, in characters. The fixed part of a
/// cycle prompt is about 4,200 characters, so a smaller limit could never
/// run.
pub const MIN_INPUT_CHARS: u32 = 5_000;

/// The development cycle: cheap model, small batches, while working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevelopmentSettings {
    pub choice: ModelChoice,
    /// Grouping window from the first event; 0 = at once.
    pub group_seconds: u32,
    pub max_runs_per_hour: u32,
    /// A batch over this is split into several runs, never cut.
    pub max_input_chars: u32,
    /// Attempts before a batch is `fallito`.
    pub max_attempts: u32,
}

impl DevelopmentSettings {
    pub const GROUP_SECONDS: RangeInclusive<u32> = 0..=3600;
    pub const MAX_RUNS_PER_HOUR: RangeInclusive<u32> = 1..=600;
    /// From [`MIN_INPUT_CHARS`].
    pub const MAX_INPUT_CHARS: RangeInclusive<u32> = MIN_INPUT_CHARS..=2_000_000;
    pub const MAX_ATTEMPTS: RangeInclusive<u32> = 1..=20;

    /// Every number brought into its range.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        self.group_seconds = clamp(self.group_seconds, &Self::GROUP_SECONDS);
        self.max_runs_per_hour = clamp(self.max_runs_per_hour, &Self::MAX_RUNS_PER_HOUR);
        self.max_input_chars = clamp(self.max_input_chars, &Self::MAX_INPUT_CHARS);
        self.max_attempts = clamp(self.max_attempts, &Self::MAX_ATTEMPTS);
        self
    }
}

impl Default for DevelopmentSettings {
    fn default() -> Self {
        Self {
            choice: ModelChoice::new(Provider::Claude, "haiku", "low"),
            group_seconds: 3600,
            max_runs_per_hour: 20,
            max_input_chars: 60_000,
            max_attempts: 3,
        }
    }
}

/// The dream cycle: scheduled consolidation. Also the default model of the
/// initial build (spec §5.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamSettings {
    pub choice: ModelChoice,
    /// The days it runs; empty: never on schedule ("Dream now" still works).
    pub days: BTreeSet<Weekday>,
    /// Local time it starts.
    pub time: LocalTime,
    pub max_minutes: u32,
    pub max_input_chars: u32,
    /// Run only when at least this many events or annotations arrived after the
    /// last cutoff.
    pub min_new_items: u32,
}

impl DreamSettings {
    pub const MAX_MINUTES: RangeInclusive<u32> = 1..=240;
    /// From [`MIN_INPUT_CHARS`].
    pub const MAX_INPUT_CHARS: RangeInclusive<u32> = MIN_INPUT_CHARS..=5_000_000;
    pub const MIN_NEW_ITEMS: RangeInclusive<u32> = 0..=10_000;

    /// Every number brought into its range.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        self.max_minutes = clamp(self.max_minutes, &Self::MAX_MINUTES);
        self.max_input_chars = clamp(self.max_input_chars, &Self::MAX_INPUT_CHARS);
        self.min_new_items = clamp(self.min_new_items, &Self::MIN_NEW_ITEMS);
        self
    }
}

impl Default for DreamSettings {
    fn default() -> Self {
        Self {
            choice: ModelChoice::new(Provider::Claude, "sonnet", "medium"),
            days: [
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
            ]
            .into_iter()
            .collect(),
            time: LocalTime {
                hour: 2,
                minute: 30,
            },
            max_minutes: 30,
            max_input_chars: 200_000,
            min_new_items: 1,
        }
    }
}

/// The `memory` key of a project's `settings.json`. Off by default; turning
/// it off stops capture and both cycles and keeps the data.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemorySettings {
    pub enabled: bool,
    pub development: DevelopmentSettings,
    pub dream: DreamSettings,
}

impl MemorySettings {
    /// Every number brought into its range.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            enabled: self.enabled,
            development: self.development.clamped(),
            dream: self.dream.clamped(),
        }
    }
}

fn clamp(value: u32, range: &RangeInclusive<u32>) -> u32 {
    value.clamp(*range.start(), *range.end())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let s = MemorySettings::default();
        assert!(!s.enabled);
        assert_eq!(
            s.development.choice,
            ModelChoice::new(Provider::Claude, "haiku", "low")
        );
        assert_eq!(
            (
                s.development.group_seconds,
                s.development.max_runs_per_hour,
                s.development.max_input_chars,
                s.development.max_attempts
            ),
            (3600, 20, 60_000, 3)
        );
        assert_eq!(
            s.dream.choice,
            ModelChoice::new(Provider::Claude, "sonnet", "medium")
        );
        let days: Vec<_> = s.dream.days.iter().map(|d| d.word()).collect();
        assert_eq!(days, ["mon", "tue", "wed", "thu", "fri"]);
        assert_eq!(s.dream.time.to_string(), "02:30");
        assert_eq!(
            (
                s.dream.max_minutes,
                s.dream.max_input_chars,
                s.dream.min_new_items
            ),
            (30, 200_000, 1)
        );
        // The defaults are inside their ranges.
        assert_eq!(s.clone().clamped(), s);
    }

    #[test]
    fn clamping_brings_numbers_into_range() {
        let mut s = MemorySettings::default();
        s.development.max_attempts = 0;
        s.development.group_seconds = 1_000_000;
        s.dream.max_minutes = 0;
        s.development.max_input_chars = 4_999;
        s.dream.max_input_chars = 0;
        let s = s.clamped();
        assert_eq!(s.development.max_input_chars, MIN_INPUT_CHARS);
        assert_eq!(s.dream.max_input_chars, 5_000);
        assert_eq!(s.development.max_attempts, 1);
        assert_eq!(s.development.group_seconds, 3600);
        assert_eq!(s.dream.max_minutes, 1);
    }

    #[test]
    fn local_time_is_strict() {
        assert_eq!(LocalTime::parse("02:30"), LocalTime::new(2, 30));
        assert_eq!(LocalTime::parse("23:59").unwrap().to_string(), "23:59");
        for bad in [
            "2:30", "24:00", "12:60", "12:3", "1230", "ab:cd", "+1:30", "",
        ] {
            assert_eq!(LocalTime::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn local_dates_check_the_calendar() {
        assert_eq!(
            LocalDate::parse("2026-11-14").unwrap().to_string(),
            "2026-11-14"
        );
        assert!(LocalDate::parse("2024-02-29").is_some());
        assert!(LocalDate::parse("2000-02-29").is_some());
        for bad in [
            "2026-02-29",
            "1900-02-29",
            "2026-13-01",
            "2026-04-31",
            "2026-1-01",
            "x",
        ] {
            assert_eq!(LocalDate::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn weekdays_round_trip() {
        for d in Weekday::ALL {
            assert_eq!(Weekday::from_word(d.word()), Some(*d));
        }
    }
}
