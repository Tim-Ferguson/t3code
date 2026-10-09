//! Field readers shared by the per-host decoders of CLI and API output. They mirror the Effect
//! schemas the original decodes with: `TrimmedNonEmptyString` (trims, then must be non-empty),
//! `PositiveInt`, `optional(NullOr(String))`, and `DateTimeUtcFromString`. A reader returns
//! `Err(())` for a present field of the wrong type, which the decoders turn into a skipped row
//! or a decode error, exactly where the original's schema would fail.
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde_json::Value;
use t3_contracts::trim_wire_string;

pub type Field<T> = Result<T, ()>;

/// A required `TrimmedNonEmptyString`.
pub fn trimmed_non_empty(value: Option<&Value>) -> Field<String> {
    match value {
        Some(Value::String(text)) => {
            let trimmed = trim_wire_string(text);
            if trimmed.is_empty() { Err(()) } else { Ok(trimmed.to_string()) }
        }
        _ => Err(()),
    }
}

/// `Schema.optional(Schema.NullOr(Schema.TrimmedNonEmptyString))`.
pub fn optional_trimmed_non_empty(value: Option<&Value>) -> Field<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        other => trimmed_non_empty(other).map(Some),
    }
}

/// A required `PositiveInt`.
pub fn positive_int(value: Option<&Value>) -> Field<u64> {
    let number = value.and_then(Value::as_f64).ok_or(())?;
    if number.fract() == 0.0 && number >= 1.0 && number <= 9_007_199_254_740_991.0 {
        Ok(number as u64)
    } else {
        Err(())
    }
}

/// `Schema.optional(Schema.String)`: absent, or a string.
pub fn optional_string(value: Option<&Value>) -> Field<Option<String>> {
    match value {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(()),
    }
}

/// `Schema.optional(Schema.NullOr(Schema.String))`.
pub fn optional_nullable_string(value: Option<&Value>) -> Field<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(()),
    }
}

/// `Schema.optional(Schema.Boolean)`.
pub fn optional_bool(value: Option<&Value>) -> Field<Option<bool>> {
    match value {
        None => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(()),
    }
}

/// `trimOptionalString`: the trimmed text, or `None` when absent, null or blank.
pub fn trim_optional(value: Option<&str>) -> Option<String> {
    let trimmed = trim_wire_string(value.unwrap_or(""));
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// A date as `new Date(text)` reads the ISO forms hosts send. Like V8, any day of 1-31 is
/// accepted for any month and rolls over (`2026-02-30` is March 2nd).
pub fn parse_date(text: &str) -> Option<DateTime<Utc>> {
    static ISO: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let text = text.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return Some(parsed.with_timezone(&Utc));
    }
    let iso = ISO.get_or_init(|| {
        regex::Regex::new(
            r"^(\d{4})-(\d{2})-(\d{2})(?:T(\d{2}):(\d{2})(?::(\d{2})(?:\.(\d{1,9}))?)?(Z|[+-]\d{2}:\d{2})?)?$",
        )
        .unwrap()
    });
    let captures = iso.captures(text)?;
    let number = |index: usize| captures.get(index).map_or(Ok(0), |m| m.as_str().parse::<u32>()).ok();
    let (year, month, day) = (captures[1].parse::<i32>().ok()?, number(2)?, number(3)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (hour, minute, second) = (number(4)?, number(5)?, number(6)?);
    if hour > 24 || minute > 59 || second > 59 || (hour == 24 && (minute, second) != (0, 0)) {
        return None;
    }
    let millis = captures.get(7).map_or(0, |m| {
        format!("{:0<3}", &m.as_str()[..m.as_str().len().min(3)]).parse::<i64>().unwrap_or(0)
    });
    let offset_seconds = match captures.get(8).map(|m| m.as_str()) {
        Some(zone) if zone != "Z" => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let hours: i64 = zone[1..3].parse().ok()?;
            let minutes: i64 = zone[4..6].parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
        _ => 0,
    };
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let date = first + chrono::Duration::days(i64::from(day) - 1);
    let naive = date.and_hms_opt(0, 0, 0)? + chrono::Duration::seconds(i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second)) + chrono::Duration::milliseconds(millis);
    Some((naive - chrono::Duration::seconds(offset_seconds)).and_utc())
}

/// `DateTime.formatIso`.
pub fn format_iso(date: &DateTime<Utc>) -> String {
    date.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `OptionFromNullOr(DateTimeUtcFromString)` as an optional field: absent or null is `None`;
/// a string that is no date fails the row.
pub fn optional_date(value: Option<&Value>) -> Field<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => parse_date(text).map(|date| Some(format_iso(&date))).ok_or(()),
        Some(_) => Err(()),
    }
}
