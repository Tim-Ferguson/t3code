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
/// DateTimeUtc JSON fields require an actual RFC3339 timestamp.
pub type UtcDateTime = chrono::DateTime<chrono::Utc>;

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
