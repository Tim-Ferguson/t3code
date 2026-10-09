//! Schedule arithmetic for scheduled tasks: when a task runs next, whether two schedules fire at the
//! same times, and whether a due fixed-time run was missed. Everything here is a pure function of the
//! schedule, a supplied instant and a time zone; nothing reads a clock.
//!
//! The original computes in the environment's local zone with Effect's `DateTime`, whose wall-clock
//! arithmetic has consequences that are kept as they are, because they decide when real users' runs
//! fire (`Schedule.ts`, `effect/dist/internal/dateTime.js`):
//! - a wall-clock time inside a spring-forward gap runs at the same wall time shifted forward by the
//!   gap ("compatible" disambiguation), so 02:30 runs at 03:30 on the day the clocks skip 02:00-03:00;
//! - a wall-clock time that occurs twice in a fall-back overlap runs at its first occurrence;
//! - an interval is added to the local wall clock, not to the instant, so an interval that crosses a
//!   fall-back transition fires an hour later than the interval, and one that crosses a spring-forward
//!   transition fires on schedule in elapsed time.
use t3_contracts::{ScheduledTaskSchedule, parse_time_of_day};

pub const MIN_INTERVAL_MS: i64 = t3_contracts::MIN_SCHEDULED_TASK_INTERVAL_MS;
/// How late a fixed-time run may fire before it counts as missed. Covers poll jitter and short
/// sleeps, while a server booted hours after the slot skips to the next occurrence instead of firing
/// stale work at a random time.
pub const MISSED_FIXED_TIME_GRACE_MS: i64 = 10 * 60_000;
const DAY_MS: i64 = 86_400_000;
/// The range of a JavaScript `Date`; the original treats anything beyond it as having no next run.
const MAX_DATE_MS: i64 = 8_640_000_000_000_000;

/// The environment's time zone, as an offset from UTC at an instant.
pub trait Zone: Send + Sync {
    /// Milliseconds east of UTC in effect at the UTC instant.
    fn offset_ms(&self, utc_ms: i64) -> i64;
}

/// The operating system's zone, which the original reads through `Intl`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalZone;
impl Zone for LocalZone {
    fn offset_ms(&self, utc_ms: i64) -> i64 {
        ChronoZone(chrono::Local).offset_ms(utc_ms)
    }
}

/// Any `chrono` zone, such as a fixed offset or a named zone from `chrono-tz` in tests.
#[derive(Debug, Clone, Copy)]
pub struct ChronoZone<Z>(pub Z);
impl<Z: chrono::TimeZone + Send + Sync> Zone for ChronoZone<Z> {
    fn offset_ms(&self, utc_ms: i64) -> i64 {
        use chrono::Offset;
        match chrono::DateTime::from_timestamp_millis(utc_ms) {
            Some(instant) => i64::from(self.0.offset_from_utc_datetime(&instant.naive_utc()).fix().local_minus_utc()) * 1000,
            None => 0,
        }
    }
}

fn in_date_range(ms: i64) -> bool {
    (-MAX_DATE_MS..=MAX_DATE_MS).contains(&ms)
}

/// Effect's `makeZonedFromAdjusted` with "compatible" disambiguation: the instant whose local wall
/// clock reads `adjusted` (as if it were UTC). `None` where `Intl` would throw for the date range.
fn instant_for_wall_clock(adjusted: i64, zone: &dyn Zone) -> Option<i64> {
    if !in_date_range(adjusted - DAY_MS) || !in_date_range(adjusted + DAY_MS) {
        return None;
    }
    let before = zone.offset_ms(adjusted - DAY_MS);
    let after = zone.offset_ms(adjusted + DAY_MS);
    if before == after {
        return Some(adjusted - before);
    }
    if before < after {
        // Clocks go forward: a wall time inside the gap is read with the earlier offset, which lands
        // after the gap; a wall time outside it takes the later offset.
        return Some(if zone.offset_ms(adjusted - after) == after { adjusted - after } else { adjusted - before });
    }
    // Clocks go back: a wall time that occurs twice is its first occurrence.
    Some(if zone.offset_ms(adjusted - before) == before { adjusted - before } else { adjusted - after })
}

fn adjusted(instant: i64, zone: &dyn Zone) -> i64 {
    instant + zone.offset_ms(instant)
}

/// The next time the schedule fires strictly after `from`, in epoch milliseconds. Webhook schedules
/// never fire on a clock.
pub fn next_scheduled_run_at(schedule: &ScheduledTaskSchedule, from: i64, zone: &dyn Zone) -> Option<i64> {
    let next = match schedule {
        ScheduledTaskSchedule::Webhook(_) => return None,
        ScheduledTaskSchedule::Interval(interval) => {
            // Persisted rows created before the one-minute floor remain readable, but they must not
            // retain their old high-frequency execution rate.
            let wall = adjusted(from, zone).checked_add(interval.every_ms.0.max(MIN_INTERVAL_MS))?;
            if !in_date_range(wall) {
                return None;
            }
            instant_for_wall_clock(wall, zone)?
        }
        ScheduledTaskSchedule::FixedTime(fixed) => {
            let (hour, minute) = parse_time_of_day(fixed.time_of_day.as_str())?;
            let weekdays: Option<Vec<i64>> = fixed.weekdays.as_ref().filter(|days| !days.is_empty()).map(|days| days.iter().map(|day| day.0).collect());
            let mut found = None;
            for offset in 0..=7i64 {
                // `DateTime.add(from, { days })` resolves the shifted wall clock to an instant, then
                // `setParts` replaces its time of day and resolves again.
                let shifted = instant_for_wall_clock(adjusted(from, zone) + offset * DAY_MS, zone)?;
                let wall = adjusted(shifted, zone);
                let candidate = instant_for_wall_clock(wall.div_euclid(DAY_MS) * DAY_MS + i64::from(hour) * 3_600_000 + i64::from(minute) * 60_000, zone)?;
                if candidate <= from {
                    continue;
                }
                if let Some(days) = &weekdays {
                    // 1970-01-01 was a Thursday; weekday 0 is Sunday.
                    let weekday = (adjusted(candidate, zone).div_euclid(DAY_MS) + 4).rem_euclid(7);
                    if !days.contains(&weekday) {
                        continue;
                    }
                }
                found = Some(candidate);
                break;
            }
            found?
        }
    };
    in_date_range(next).then_some(next)
}

/// Canonical form of a weekday mask: order and duplicates are irrelevant, and an empty or complete
/// mask means the same as listing all seven days, which is daily.
fn weekday_key(weekdays: &Option<Vec<t3_contracts::Weekday>>) -> String {
    let mut unique: Vec<i64> = weekdays.iter().flatten().map(|day| day.0).collect();
    unique.sort_unstable();
    unique.dedup();
    if unique.is_empty() || unique.len() == 7 {
        return "daily".into();
    }
    unique.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

/// Semantic equality for schedules: true when both fire at the same times.
pub fn is_same_schedule(a: &ScheduledTaskSchedule, b: &ScheduledTaskSchedule) -> bool {
    use ScheduledTaskSchedule::*;
    match (a, b) {
        (Interval(a), Interval(b)) => a.every_ms == b.every_ms,
        (Interval(_), _) => false,
        // Webhook tasks never have a next run, whatever their signature settings.
        (Webhook(_), other) => matches!(other, Webhook(_)),
        (FixedTime(a), FixedTime(b)) => {
            // "9:00" and "09:00" are the same time, so a format-only edit is not a schedule change.
            match (parse_time_of_day(a.time_of_day.as_str()), parse_time_of_day(b.time_of_day.as_str())) {
                (Some(left), Some(right)) => left == right && weekday_key(&a.weekdays) == weekday_key(&b.weekdays),
                _ => false,
            }
        }
        (FixedTime(_), _) => false,
    }
}

/// True when a due fixed-time run was missed by more than the grace window and should move to its next
/// occurrence instead of firing now. Interval schedules are never missed: an overdue interval task
/// catches up with a single run.
pub fn is_missed_fixed_time_run(schedule: &ScheduledTaskSchedule, due_at: i64, now: i64) -> bool {
    matches!(schedule, ScheduledTaskSchedule::FixedTime(_)) && now - due_at > MISSED_FIXED_TIME_GRACE_MS
}

// ---- ISO timestamps --------------------------------------------------------------------------------

/// `Date.prototype.toISOString` for an epoch in the representable range.
pub fn iso_from_ms(ms: i64) -> Option<String> {
    if !in_date_range(ms) {
        return None;
    }
    let (days, in_day) = (ms.div_euclid(DAY_MS), ms.rem_euclid(DAY_MS));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let year_text = if (0..=9999).contains(&year) { format!("{year:04}") } else { format!("{}{:06}", if year < 0 { '-' } else { '+' }, year.abs()) };
    Some(format!(
        "{year_text}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000
    ))
}

/// The epoch of an ISO-8601 timestamp, with an offset or `Z`, optional fraction and expanded years.
/// Anything `Date.parse` would not read as one of those is `None`, so a stored value that cannot be
/// read is treated as not due rather than failing a poll.
pub fn parse_iso(value: &str) -> Option<i64> {
    static SHAPE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let shape = SHAPE.get_or_init(|| {
        regex::Regex::new(r"^([+-]\d{6}|\d{4})-(\d{2})-(\d{2})(?:T(\d{2}):(\d{2})(?::(\d{2})(?:\.(\d{1,9}))?)?(Z|[+-]\d{2}:\d{2})?)?$").unwrap()
    });
    let captures = shape.captures(value)?;
    let number = |index: usize| captures.get(index).map_or(Some(0), |found| found.as_str().parse::<i64>().ok());
    let year_text = &captures[1];
    if year_text == "-000000" {
        return None;
    }
    let (year, month, day) = (year_text.parse::<i64>().ok()?, number(2)?, number(3)?);
    let (hour, minute, second) = (number(4)?, number(5)?, number(6)?);
    let fraction = captures.get(7).map_or(0, |found| {
        let digits = &found.as_str()[..found.as_str().len().min(3)];
        digits.parse::<i64>().unwrap_or(0) * 10i64.pow(3 - digits.len() as u32)
    });
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return None,
    };
    if day < 1 || day > days_in_month || hour > 24 || minute > 59 || second > 59 || (hour == 24 && (minute, second, fraction) != (0, 0, 0)) {
        return None;
    }
    // days_from_civil.
    let shifted_year = year - i64::from(month <= 2);
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year.rem_euclid(400);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let mut ms = days * DAY_MS + hour * 3_600_000 + minute * 60_000 + second * 1000 + fraction;
    // Without an offset a date-time is local time in JavaScript; stored values always carry one, and a
    // bare date is UTC.
    if let Some(zone) = captures.get(8) {
        if zone.as_str() != "Z" {
            let sign = if zone.as_str().starts_with('-') { -1 } else { 1 };
            let (zone_hour, zone_minute) = (zone.as_str()[1..3].parse::<i64>().ok()?, zone.as_str()[4..6].parse::<i64>().ok()?);
            ms -= sign * (zone_hour * 60 + zone_minute) * 60_000;
        }
    } else if captures.get(4).is_some() {
        return None;
    }
    in_date_range(ms).then_some(ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::str::FromStr;

    fn fixture() -> Vec<Value> {
        include_str!("../tests/fixtures/scheduled-task-schedule.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
    /// Rows whose schedule the contract itself refuses (a stored task could never hold them).
    fn schedule(value: &Value) -> Option<ScheduledTaskSchedule> {
        serde_json::from_value(value.clone()).ok()
    }

    #[test]
    fn next_run_matches_the_original_in_every_zone_across_every_daylight_saving_edge() {
        let mut checked = 0;
        let mut zones = std::collections::BTreeSet::new();
        for row in fixture().iter().filter(|row| row["type"] == "next") {
            let zone = ChronoZone(chrono_tz::Tz::from_str(row["zone"].as_str().unwrap()).unwrap());
            let schedule = schedule(&row["schedule"]).unwrap_or_else(|| panic!("{row}"));
            let from = row["from"].as_i64().unwrap();
            let next = next_scheduled_run_at(&schedule, from, &zone).and_then(iso_from_ms);
            let context = format!("{} from {} ({}) for {}", row["zone"], from, iso_from_ms(from).unwrap(), row["schedule"]);
            // chrono-tz stops applying daylight saving rules after 2037, where ICU keeps applying them
            // (the operating system's zone data, which production reads, does too). Only the absurd
            // intervals that land centuries out hit this; they may differ by the daylight saving offset.
            match (next.as_deref().and_then(parse_iso), row["next"].as_str().and_then(parse_iso)) {
                (Some(ours), Some(theirs)) if theirs > 2_145_916_800_000 => assert!((ours - theirs).abs() <= 3_600_000, "{context}"),
                _ => assert_eq!(next.as_deref(), row["next"].as_str(), "{context}"),
            }
            zones.insert(row["zone"].as_str().unwrap().to_owned());
            checked += 1;
        }
        // The fixture is the evidence; an empty or partial one must not pass quietly.
        assert!(checked > 10_000, "{checked}");
        assert_eq!(zones.len(), 13);
    }

    #[test]
    fn sameness_missed_runs_and_time_parsing_match_the_original() {
        let (mut same, mut missed, mut times) = (0, 0, 0);
        for row in fixture() {
            match row["type"].as_str().unwrap() {
                "same" => {
                    let (Some(a), Some(b)) = (schedule(&row["a"]), schedule(&row["b"])) else { continue };
                    assert_eq!(is_same_schedule(&a, &b), row["same"], "{row}");
                    same += 1;
                }
                "missed" => {
                    let schedule = schedule(&row["schedule"]).unwrap();
                    assert_eq!(is_missed_fixed_time_run(&schedule, row["dueAt"].as_i64().unwrap(), row["now"].as_i64().unwrap()), row["missed"], "{row}");
                    missed += 1;
                }
                "time" => {
                    let expected = row["parsed"].as_object().map(|parsed| (parsed["hour"].as_u64().unwrap() as u32, parsed["minute"].as_u64().unwrap() as u32));
                    assert_eq!(parse_time_of_day(row["value"].as_str().unwrap()), expected, "{row}");
                    times += 1;
                }
                "constants" => {
                    assert_eq!(row["minIntervalMs"], MIN_INTERVAL_MS);
                    assert_eq!(row["missedFixedTimeGraceMs"], MISSED_FIXED_TIME_GRACE_MS);
                }
                _ => {}
            }
        }
        assert!(same >= 14 && missed == 27 && times == 20, "{same} {missed} {times}");
    }

    #[test]
    fn iso_round_trips_and_follows_toisostring_for_expanded_years() {
        for ms in [0, 1, 999, -1, 946_684_800_000, 1_782_907_200_123, 253_402_300_799_999, 253_402_300_800_000, -62_198_755_200_000, -62_135_596_800_000, 8_640_000_000_000_000, -8_640_000_000_000_000] {
            let text = iso_from_ms(ms).unwrap();
            assert_eq!(parse_iso(&text), Some(ms), "{text}");
        }
        assert_eq!(iso_from_ms(8_640_000_000_000_001), None);
        assert_eq!(iso_from_ms(253_402_300_800_000).unwrap(), "+010000-01-01T00:00:00.000Z");
        assert_eq!(iso_from_ms(-62_198_755_200_000).unwrap(), "-000001-01-01T00:00:00.000Z");
        assert_eq!(iso_from_ms(-62_135_596_800_000).unwrap(), "0001-01-01T00:00:00.000Z");
        assert_eq!(parse_iso("2026-09-09T12:00:00Z"), parse_iso("2026-09-09T12:00:00.000Z"));
        assert_eq!(parse_iso("2026-09-09T14:00:00+02:00"), parse_iso("2026-09-09T12:00:00.000Z"));
        assert_eq!(parse_iso("2026-09-09"), parse_iso("2026-09-09T00:00:00.000Z"));
        for bad in ["", "not a date", "2026-13-01T00:00:00Z", "2026-02-30T00:00:00Z", "2026-09-09T25:00:00Z", "2026-09-09T12:00:00", "-000000-01-01T00:00:00.000Z"] {
            assert_eq!(parse_iso(bad), None, "{bad}");
        }
    }
}
