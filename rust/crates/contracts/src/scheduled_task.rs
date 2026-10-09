//! `scheduledTasks.*` payloads and results (port of `packages/contracts/src/scheduledTask.ts`).
//! On the wire an optional property may also be null, which decodes as absent; a property that is
//! optional and nullable keeps the difference (`Option<Option<_>>`) so a payload round-trips.
use crate::base::{non_blank, string_type};
use crate::history::object_struct;
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MIN_SCHEDULED_TASK_INTERVAL_MS: i64 = 60_000;
/// Matches how long the relay holds a request for an offline environment.
pub const MAX_WEBHOOK_DELIVERY_AGE_MINUTES: i64 = 24 * 60;

string_type!(ScheduledTaskWebhookDeliveryId, non_blank);

/// The workspace a launched thread uses. On the wire `branch: null` decodes as no branch here, while the
/// launch contract keeps the null, so a task holds the plain form.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ScheduledTaskWorkspaceStrategy(pub ThreadLaunchWorkspaceStrategy);
impl<'de> Deserialize<'de> for ScheduledTaskWorkspaceStrategy {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        fn plain<T>(field: &mut Option<Option<T>>) {
            if matches!(field, Some(None)) {
                *field = None;
            }
        }
        let mut strategy = ThreadLaunchWorkspaceStrategy::deserialize(d)?;
        match &mut strategy {
            ThreadLaunchWorkspaceStrategy::Root(root) => plain(&mut root.branch),
            ThreadLaunchWorkspaceStrategy::ExistingWorktree(worktree) => plain(&mut worktree.branch),
            ThreadLaunchWorkspaceStrategy::Worktree(worktree) => {
                plain(&mut worktree.branch);
                plain(&mut worktree.start_from_origin);
            }
        }
        Ok(Self(strategy))
    }
}

/// `^([01]?\d|2[0-3]):([0-5]\d)$` after trimming, as `parseTimeOfDay` reads a stored value.
pub fn parse_time_of_day(value: &str) -> Option<(u32, u32)> {
    let value = trim_wire_string(value);
    let (hour, minute) = value.split_once(':')?;
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if !digits(hour) || hour.len() > 2 || minute.len() != 2 || !digits(minute) {
        return None;
    }
    let (hour, minute) = (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?);
    (hour <= 23 && minute <= 59).then_some((hour, minute))
}

/// Local wall-clock time in 24-hour `HH:MM` form, such as `09:30` (the hour may be unpadded).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct TimeOfDay(String);
impl TimeOfDay {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for TimeOfDay {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = TrimmedNonEmptyString::deserialize(d)?;
        if parse_time_of_day(value.as_str()).is_none() {
            return Err(serde::de::Error::custom("expected a HH:MM time of day"));
        }
        Ok(Self(value.into_string()))
    }
}

/// A positive safe integer: the read model keeps accepting legacy sub-minute intervals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct IntervalMs(pub i64);
impl<'de> Deserialize<'de> for IntervalMs {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = SafeInt::deserialize(d)?.0;
        if value > 0 {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom("expected a positive integer"))
        }
    }
}
/// Newly created or updated interval schedules run at most once per minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WritableIntervalMs(pub i64);
impl<'de> Deserialize<'de> for WritableIntervalMs {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = IntervalMs::deserialize(d)?.0;
        if value >= MIN_SCHEDULED_TASK_INTERVAL_MS {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom("expected an interval of at least 60000"))
        }
    }
}

/// Weekday number where 0 is Sunday and 6 is Saturday.
pub type Weekday = RangeInt<0, 6>;
/// A positive safe integer is not enough for `maxDeliveryAgeMinutes`: 1 to 1440.
pub type WebhookMaxDeliveryAgeMinutes = RangeInt<1, 1440>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookSignatureEncoding {
    Hex,
    Base64,
}

object_struct! {pub struct IntervalScheduleFields {
    pub every_ms:IntervalMs,
}}
object_struct! {pub struct WritableIntervalScheduleFields {
    pub every_ms:WritableIntervalMs,
}}
object_struct! {pub struct FixedTimeScheduleFields {
    pub time_of_day:TimeOfDay,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_required_nullable")]
    pub weekdays:Option<Vec<Weekday>>,
}}
// HMAC-SHA256 over the raw request body. The secret is never part of the read model.
object_struct! {pub struct ScheduledTaskWebhookSignature {
    pub header:TrimmedNonEmptyString,
    pub encoding:WebhookSignatureEncoding,
    pub prefix:String,
}}
object_struct! {pub struct WebhookScheduleFields {
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub signature:Option<ScheduledTaskWebhookSignature>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub max_delivery_age_minutes:Option<Option<WebhookMaxDeliveryAgeMinutes>>,
}}

/// Read model for persisted schedules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ScheduledTaskSchedule {
    #[serde(rename = "interval")]
    Interval(IntervalScheduleFields),
    #[serde(rename = "fixed_time")]
    FixedTime(FixedTimeScheduleFields),
    #[serde(rename = "webhook")]
    Webhook(WebhookScheduleFields),
}

object_struct! {pub struct UpsertWebhookSignature {
    pub header:TrimmedNonEmptyString,
    pub encoding:WebhookSignatureEncoding,
    pub prefix:String,
    /// Shared signing secret. Omit to keep the stored secret.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_required_nullable")]
    pub secret:Option<TrimmedNonEmptyString>,
    /// A secret the user entered through request_secret, used instead of `secret`.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_required_nullable")]
    pub secret_ref:Option<SecretRef>,
}}
object_struct! {pub struct UpsertWebhookScheduleFields {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub signature:Option<Option<UpsertWebhookSignature>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub max_delivery_age_minutes:Option<Option<WebhookMaxDeliveryAgeMinutes>>,
}}

/// Mutation model: newly created or updated interval schedules run at most once per minute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ScheduledTaskUpsertSchedule {
    #[serde(rename = "interval")]
    Interval(WritableIntervalScheduleFields),
    #[serde(rename = "fixed_time")]
    FixedTime(FixedTimeScheduleFields),
    #[serde(rename = "webhook")]
    Webhook(UpsertWebhookScheduleFields),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduledTaskRunStatus {
    Never,
    Running,
    Succeeded,
    Failed,
}
impl ScheduledTaskRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

// Where a webhook task receives requests. Present only on webhook tasks.
object_struct! {pub struct ScheduledTaskWebhookEndpoint {
    /// Environment-relative path including the secret token.
    pub path:TrimmedNonEmptyString,
    /// Public T3 Connect URL, or null when the environment is not linked to T3 Connect.
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub url:Option<TrimmedNonEmptyString>,
    pub has_secret:bool,
}}

object_struct! {pub struct ScheduledTask {
    pub id:ScheduledTaskId,
    pub title:TrimmedNonEmptyString,
    pub prompt:TrimmedNonEmptyString,
    pub enabled:bool,
    pub schedule:ScheduledTaskSchedule,
    pub project_id:ProjectId,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub thread_id:Option<ThreadId>,
    pub workspace_strategy:ScheduledTaskWorkspaceStrategy,
    pub model_selection:ModelSelection,
    pub runtime_mode:RuntimeMode,
    pub interaction_mode:ProviderInteractionMode,
    pub created_by:Actor,
    pub creation_source:CreationSource,
    pub created_at:IsoDateTime,
    pub updated_at:IsoDateTime,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub next_run_at:Option<IsoDateTime>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub last_run_at:Option<IsoDateTime>,
    pub last_run_status:ScheduledTaskRunStatus,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub last_run_error:Option<String>,
    pub run_count:NonNegativeInt,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_required_nullable")]
    pub webhook:Option<ScheduledTaskWebhookEndpoint>,
}}

/// `Schema.Struct({})` accepts any non-null value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScheduledTaskListInput;
impl<'de> Deserialize<'de> for ScheduledTaskListInput {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Null => Err(serde::de::Error::custom("expected a non-null value")),
            _ => Ok(Self),
        }
    }
}
impl Serialize for ScheduledTaskListInput {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serde_json::Map::new().serialize(s)
    }
}

object_struct! {pub struct ScheduledTaskListResult {
    // Trigger types grow over time; a client must not lose the whole list over one task it cannot decode.
    pub tasks:ForwardCompatibleArray<ScheduledTask>,
}}

object_struct! {pub struct ScheduledTaskUpsertInput {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_required_nullable")]
    pub id:Option<ScheduledTaskId>,
    /// Reject the save if the task no longer exists, for edits from a client form.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_required_nullable")]
    pub require_existing:Option<bool>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_required_nullable")]
    pub command_id:Option<CommandId>,
    pub title:TrimmedNonEmptyString,
    pub prompt:TrimmedNonEmptyString,
    pub enabled:bool,
    pub schedule:ScheduledTaskUpsertSchedule,
    pub project_id:ProjectId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub thread_id:Option<Option<ThreadId>>,
    pub workspace_strategy:ScheduledTaskWorkspaceStrategy,
    pub model_selection:ModelSelection,
    pub runtime_mode:RuntimeMode,
    pub interaction_mode:ProviderInteractionMode,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_required_nullable")]
    pub created_by:Option<Actor>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_required_nullable")]
    pub creation_source:Option<CreationSource>,
}}

// Partial update that flips only the enabled flag, never overwriting other fields.
object_struct! {pub struct ScheduledTaskSetEnabledInput {
    pub id:ScheduledTaskId,
    pub enabled:bool,
}}
object_struct! {pub struct ScheduledTaskDeleteInput {
    pub id:ScheduledTaskId,
}}
object_struct! {pub struct ScheduledTaskRunNowInput {
    pub id:ScheduledTaskId,
}}
object_struct! {pub struct ScheduledTaskRotateWebhookTokenInput {
    pub id:ScheduledTaskId,
}}
object_struct! {pub struct ScheduledTaskListWebhookDeliveriesInput {
    pub id:ScheduledTaskId,
}}
object_struct! {pub struct ScheduledTaskGetWebhookDeliveryInput {
    pub id:ScheduledTaskId,
    pub delivery_id:ScheduledTaskWebhookDeliveryId,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskWebhookDeliveryOutcome {
    Accepted,
    DispatchFailed,
    RejectedSignature,
    Disabled,
    RateLimited,
    Expired,
}
impl ScheduledTaskWebhookDeliveryOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::DispatchFailed => "dispatch_failed",
            Self::RejectedSignature => "rejected_signature",
            Self::Disabled => "disabled",
            Self::RateLimited => "rate_limited",
            Self::Expired => "expired",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(value.to_owned())).ok()
    }
}

object_struct! {pub struct ScheduledTaskWebhookDeliverySummary {
    pub id:ScheduledTaskWebhookDeliveryId,
    pub task_id:ScheduledTaskId,
    pub received_at:IsoDateTime,
    pub method:TrimmedNonEmptyString,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub content_type:Option<String>,
    pub body_bytes:NonNegativeInt,
    pub outcome:ScheduledTaskWebhookDeliveryOutcome,
    /// True when a configured signature matched; false when none was configured.
    pub signature_verified:bool,
    /// Template placeholders that had no value in this request and rendered empty.
    pub missing_fields:Vec<String>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub error:Option<String>,
}}
object_struct! {pub struct ScheduledTaskWebhookDelivery {
    pub id:ScheduledTaskWebhookDeliveryId,
    pub task_id:ScheduledTaskId,
    pub received_at:IsoDateTime,
    pub method:TrimmedNonEmptyString,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub content_type:Option<String>,
    pub body_bytes:NonNegativeInt,
    pub outcome:ScheduledTaskWebhookDeliveryOutcome,
    pub signature_verified:bool,
    pub missing_fields:Vec<String>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub error:Option<String>,
    pub query:String,
    pub headers:BTreeMap<String,String>,
    /// Body as UTF-8 text, cut at the log limit; see `body_truncated`.
    pub body:String,
    pub body_truncated:bool,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub rendered_prompt:Option<String>,
}}
object_struct! {pub struct ScheduledTaskListWebhookDeliveriesResult {
    pub deliveries:Vec<ScheduledTaskWebhookDeliverySummary>,
}}
object_struct! {pub struct ScheduledTaskGetWebhookDeliveryResult {
    pub delivery:ScheduledTaskWebhookDelivery,
}}
object_struct! {pub struct ScheduledTaskMutationResult {
    pub task:ScheduledTask,
}}
object_struct! {pub struct ScheduledTaskDeleteResult {
    pub id:ScheduledTaskId,
}}
pub type ScheduledTaskRunNowResult = ScheduledTaskMutationResult;
