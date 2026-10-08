use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expected {expected}")]
pub struct ValidationError {
    pub expected: &'static str,
}

/// JavaScript String.trim uses ECMAScript whitespace, including U+FEFF.
pub fn trim_wire_string(value: &str) -> &str {
    value.trim_matches(|c: char| matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'))
}

macro_rules! string_type {
    ($name:ident, $validate:path) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl AsRef<str>) -> Result<Self, ValidationError> {
                let value = crate::base::trim_wire_string(value.as_ref());
                $validate(value)?;
                Ok(Self(value.to_owned()))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
            pub fn into_string(self) -> String {
                self.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
        impl std::str::FromStr for $name {
            type Err = ValidationError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}
pub(crate) use string_type;

pub(crate) fn non_blank(value: &str) -> Result<(), ValidationError> {
    if value.is_empty() {
        Err(ValidationError {
            expected: "a non-blank string",
        })
    } else {
        Ok(())
    }
}

string_type!(TrimmedNonEmptyString, non_blank);
string_type!(ThreadId, non_blank);
string_type!(ProjectId, non_blank);
string_type!(EnvironmentId, non_blank);
string_type!(CommandId, non_blank);
string_type!(EventId, non_blank);
string_type!(MessageId, non_blank);
string_type!(TurnId, non_blank);
string_type!(RunId, non_blank);
string_type!(RunAttemptId, non_blank);
string_type!(NodeId, non_blank);
string_type!(AuthSessionId, non_blank);
string_type!(ProviderItemId, non_blank);
string_type!(ProviderSessionId, non_blank);
string_type!(ProviderThreadId, non_blank);
string_type!(ProviderTurnId, non_blank);
string_type!(RuntimeSessionId, non_blank);
string_type!(RuntimeItemId, non_blank);
string_type!(TurnItemId, non_blank);
string_type!(RuntimeRequestId, non_blank);
string_type!(RuntimeTaskId, non_blank);
string_type!(ScheduledTaskId, non_blank);
string_type!(SecretRef, non_blank);
string_type!(ApprovalRequestId, non_blank);
string_type!(CheckpointRef, non_blank);
string_type!(CheckpointId, non_blank);
string_type!(CheckpointScopeId, non_blank);
string_type!(ContextHandoffId, non_blank);
string_type!(ContextTransferId, non_blank);
string_type!(RawEventId, non_blank);
string_type!(PlanId, non_blank);

/// IsoDateTime in the original contract is deliberately an unchecked string.
pub type IsoDateTime = String;
/// JSON DateTimeUtc values are normalized to UTC with JavaScript millisecond
/// precision, including the .000 suffix that chrono's default codec omits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcDateTime(pub chrono::DateTime<chrono::Utc>);
impl std::ops::Deref for UtcDateTime {
    type Target = chrono::DateTime<chrono::Utc>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl From<chrono::DateTime<chrono::Utc>> for UtcDateTime {
    fn from(value: chrono::DateTime<chrono::Utc>) -> Self {
        Self(
            chrono::DateTime::from_timestamp_millis(value.timestamp_millis())
                .expect("valid chrono date"),
        )
    }
}
impl Serialize for UtcDateTime {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    }
}
impl<'de> Deserialize<'de> for UtcDateTime {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        if let Ok(date) = chrono::DateTime::parse_from_rfc3339(&value) {
            return Ok(Self::from(date.with_timezone(&chrono::Utc)));
        }
        if let Ok(date) = chrono::NaiveDateTime::parse_from_str(&value, "%Y-%m-%dT%H:%M:%S%.f") {
            return Ok(Self::from(date.and_utc()));
        }
        if let Ok(date) = chrono::NaiveDate::parse_from_str(&value, "%Y-%m-%d") {
            return Ok(Self::from(date.and_hms_opt(0, 0, 0).unwrap().and_utc()));
        }
        Err(serde::de::Error::custom(
            "expected a valid UTC date-time string",
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiteralBool<const VALUE: bool>;
impl<const VALUE: bool> Serialize for LiteralBool<VALUE> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(VALUE)
    }
}
impl<'de, const VALUE: bool> Deserialize<'de> for LiteralBool<VALUE> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if bool::deserialize(d)? == VALUE {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!("expected {VALUE}")))
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiteralInt<const VALUE: u64>;
impl<const VALUE: u64> Serialize for LiteralInt<VALUE> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(VALUE)
    }
}

pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub fn deserialize_safe_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let value = serde_json::Number::deserialize(d)?;
    let number = value
        .as_f64()
        .ok_or_else(|| serde::de::Error::custom("expected a non-negative integer"))?;
    if number >= 0.0 && number <= MAX_SAFE_INTEGER as f64 && number.fract() == 0.0 {
        Ok(number as u64)
    } else {
        Err(serde::de::Error::custom(
            "expected a non-negative safe integer",
        ))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PositiveInt(pub u64);
impl<'de> Deserialize<'de> for PositiveInt {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = deserialize_safe_u64(d)?;
        if value > 0 {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom("expected a positive integer"))
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct NonNegativeInt(pub u64);
impl<'de> Deserialize<'de> for NonNegativeInt {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_safe_u64(d).map(Self)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BoundedString<const MAX: usize>(pub String);
impl<'de, const MAX: usize> Deserialize<'de> for BoundedString<MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        if value.encode_utf16().count() <= MAX {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(format!(
                "expected at most {MAX} UTF-16 code units"
            )))
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BoundedTrimmedString<const MAX: usize>(pub TrimmedNonEmptyString);
impl<'de, const MAX: usize> Deserialize<'de> for BoundedTrimmedString<MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Source string length checks run after trimming.
        let value = TrimmedNonEmptyString::deserialize(d)?;
        if value.as_str().encode_utf16().count() <= MAX {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(format!(
                "expected at most {MAX} UTF-16 code units"
            )))
        }
    }
}
impl<'de, const VALUE: u64> Deserialize<'de> for LiteralInt<VALUE> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if deserialize_safe_u64(d)? == VALUE {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!("expected {VALUE}")))
        }
    }
}

/// Unlike serde's default Option decoding, a present optional key cannot be null.
pub fn deserialize_optional<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}
/// Adding this to an Option field preserves required-but-nullable semantics.
pub fn deserialize_required_nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientSurface {
    Web,
    Desktop,
    Mobile,
    Cli,
}

/// Drop unknown tags while retaining errors for malformed known members.
pub fn decode_forward_union_array<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    tag: &str,
    known_tags: &[&str],
) -> Result<Vec<T>, serde_json::Error> {
    let values: Vec<serde_json::Value> = serde_json::from_value(value)?;
    values
        .into_iter()
        .filter(|v| {
            // Missing/nonstring tags remain errors, matching the source schema.
            v.get(tag)
                .and_then(serde_json::Value::as_str)
                .is_none_or(|t| known_tags.contains(&t))
        })
        .map(serde_json::from_value)
        .collect()
}

/// Generic tolerant array used by config descriptors. Any rejected member is
/// omitted; tagged orchestration unions instead use decode_forward_union_array.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(transparent)]
pub struct ForwardCompatibleArray<T>(pub Vec<T>);
impl<'de, T: serde::de::DeserializeOwned> Deserialize<'de> for ForwardCompatibleArray<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let values = Vec::<serde_json::Value>::deserialize(d)?;
        Ok(Self(
            values
                .into_iter()
                .filter_map(|v| serde_json::from_value(v).ok())
                .collect(),
        ))
    }
}
pub fn deserialize_forward_optional<
    'de,
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    Ok(Some(serde_json::from_value(raw).ok()))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SafeInt(pub i64);
impl<'de> Deserialize<'de> for SafeInt {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Number::deserialize(d)?;
        let n = value
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("expected safe integer"))?;
        if n.abs() <= MAX_SAFE_INTEGER as f64 && n.fract() == 0.0 {
            Ok(Self(n as i64))
        } else {
            Err(serde::de::Error::custom("expected safe integer"))
        }
    }
}

/// The source optionalKey(UndefinedOr(...)) codec accepts future values but
/// cannot encode a present undefined value as JSON. Missing fields are skipped
/// by the enclosing struct; a rejected present member retains that distinction.
pub fn serialize_forward_optional<S: serde::Serializer, T: Serialize>(
    value: &Option<Option<T>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(Some(inner)) => inner.serialize(s),
        _ => Err(serde::ser::Error::custom(
            "cannot encode an undefined forward-compatible field as JSON",
        )),
    }
}

fn any_string(_: &str) -> Result<(), ValidationError> {
    Ok(())
}
string_type!(TrimmedString, any_string);
impl Default for TrimmedString {
    fn default() -> Self {
        Self(String::new())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RangeInt<const MIN: i64, const MAX: i64>(pub i64);
impl<'de, const MIN: i64, const MAX: i64> Deserialize<'de> for RangeInt<MIN, MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = SafeInt::deserialize(d)?.0;
        if (MIN..=MAX).contains(&value) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(format!(
                "expected integer between {MIN} and {MAX}"
            )))
        }
    }
}
pub type PortSchema = RangeInt<1, 65535>;
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RangeNumber<const MIN: i64, const MAX: i64>(pub serde_json::Number);
impl<'de, const MIN: i64, const MAX: i64> Deserialize<'de> for RangeNumber<MIN, MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Number::deserialize(d)?;
        if value
            .as_f64()
            .is_some_and(|n| n >= MIN as f64 && n <= MAX as f64)
        {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(format!(
                "expected number between {MIN} and {MAX}"
            )))
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NonNegativeNumber(pub serde_json::Number);
impl<'de> Deserialize<'de> for NonNegativeNumber {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Number::deserialize(d)?;
        if value.as_f64().is_some_and(|n| n >= 0.0) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "expected a finite non-negative number",
            ))
        }
    }
}
macro_rules! plain_string_type {
    ($name:ident, $validate:path) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl AsRef<str>) -> Result<Self, ValidationError> {
                let value = value.as_ref();
                $validate(value)?;
                Ok(Self(value.to_owned()))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
            pub fn into_string(self) -> String {
                self.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
        impl std::str::FromStr for $name {
            type Err = ValidationError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

pub(crate) use plain_string_type;
plain_string_type!(NonEmptyString, non_blank);

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedVec<T, const MAX: usize>(pub Vec<T>);
impl<'de, T: Deserialize<'de>, const MAX: usize> Deserialize<'de> for BoundedVec<T, MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let values = Vec::<T>::deserialize(d)?;
        if values.len() <= MAX {
            Ok(Self(values))
        } else {
            Err(serde::de::Error::custom(format!(
                "expected at most {MAX} entries"
            )))
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedTrimmedAllowEmptyString<const MAX: usize>(pub TrimmedString);
impl<'de, const MAX: usize> Deserialize<'de> for BoundedTrimmedAllowEmptyString<MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = TrimmedString::deserialize(d)?;
        if value.as_str().encode_utf16().count() <= MAX {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(format!(
                "expected at most {MAX} UTF-16 code units"
            )))
        }
    }
}

/// Maintain ergonomic u64 counters without accepting integers beyond JS's
/// representable range in source wire schemas.
pub fn deserialize_nonnegative_u64<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<u64, D::Error> {
    NonNegativeInt::deserialize(d).map(|n| n.0)
}
pub fn deserialize_optional_nonnegative_u64<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<u64>>, D::Error> {
    Option::<NonNegativeInt>::deserialize(d).map(|value| Some(value.map(|n| n.0)))
}

/// A flattened field makes serde require an object rather than accepting a
/// positional sequence for a struct. Unknown properties are discarded as in
/// Effect Struct decoding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DiscardUnknownFields;
impl<'de> Deserialize<'de> for DiscardUnknownFields {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        std::collections::BTreeMap::<String, serde::de::IgnoredAny>::deserialize(d)?;
        Ok(Self)
    }
}
