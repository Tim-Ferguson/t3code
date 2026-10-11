//! "Custom snooze": a wake time from a calendar date and a clock time, or from a length of time. Port of
//! `CustomSnoozeInput` and `resolveCustomSnooze` (`state/threadSettled.ts`).
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Minutes,
    Hours,
    Days,
}
impl Unit {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "minutes" => Some(Self::Minutes),
            "hours" => Some(Self::Hours),
            "days" => Some(Self::Days),
            _ => None,
        }
    }
    fn minutes(self) -> f64 {
        match self {
            Self::Minutes => 1.0,
            Self::Hours => 60.0,
            Self::Days => 1440.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input<'a> {
    /// A local date (`YYYY-MM-DD`) and time (`HH:MM`).
    Date { date: &'a str, time: &'a str },
    Duration { amount: &'a str, unit: Unit },
}

pub const DATE_ERROR: &str = "Choose a valid date and time in the future.";
pub const DURATION_ERROR: &str = "Enter a positive duration.";

/// When the thread wakes, as the instant the server stores, for a viewer `offset_minutes` east of UTC. Nothing for a
/// past or malformed input.
pub fn resolve(input: &Input, now: DateTime<Utc>, offset_minutes: i32) -> Option<String> {
    let wake = match input {
        Input::Duration { amount, unit } => {
            let amount: f64 = amount.trim().parse().ok().filter(|amount: &f64| amount.is_finite() && *amount > 0.0)?;
            now + Duration::milliseconds((amount * unit.minutes() * 60_000.0).round() as i64)
        }
        Input::Date { date, time } => {
            let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok().filter(|_| date.len() == 10)?;
            let time = NaiveTime::parse_from_str(time, "%H:%M").ok().filter(|_| time.len() == 5)?;
            date.and_time(time).and_utc() - Duration::minutes(i64::from(offset_minutes))
        }
    };
    (wake > now).then(|| wake.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-03-10T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn a_duration_counts_from_now_and_must_be_positive() {
        let hours = |amount| resolve(&Input::Duration { amount, unit: Unit::Hours }, now(), 0);
        assert_eq!(hours("2").as_deref(), Some("2026-03-10T14:00:00.000Z"));
        assert_eq!(hours("0.5").as_deref(), Some("2026-03-10T12:30:00.000Z"));
        assert_eq!((hours("0"), hours("-1"), hours(""), hours("soon")), (None, None, None, None));
        assert_eq!(resolve(&Input::Duration { amount: "2", unit: Unit::Days }, now(), 0).as_deref(), Some("2026-03-12T12:00:00.000Z"));
    }

    #[test]
    fn a_date_and_time_are_local_and_must_be_in_the_future() {
        let at = |date, time, offset| resolve(&Input::Date { date, time }, now(), offset);
        // Nine in the evening in a zone two hours east of UTC is 19:00 UTC.
        assert_eq!(at("2026-03-10", "21:00", 120).as_deref(), Some("2026-03-10T19:00:00.000Z"));
        assert_eq!(at("2026-03-10", "13:00", 120), None, "that moment has passed in that zone");
        assert_eq!((at("2026-3-10", "21:00", 0), at("2026-03-10", "9:00", 0), at("2026-02-30", "21:00", 0), at("", "", 0)), (None, None, None, None));
    }
}
