//! GitHub pull request rows in `gh --json` or GraphQL node shape (port of
//! `gitHubPullRequests.ts`), normalized to the provider-neutral `ChangeRequest`.
use crate::source_control_decode::*;
use serde_json::Value;
use t3_contracts::{ChangeRequest, ChangeRequestState, SourceControlProviderKind};

fn nested_string(value: Option<&Value>, key: &str) -> Field<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(object)) => optional_nullable_string(object.get(key)),
        Some(_) => Err(()),
    }
}

fn state_of(state: Option<&str>, merged_at: Option<&str>) -> ChangeRequestState {
    let normalized = state.map(|state| state.trim().to_uppercase());
    if merged_at.is_some_and(|merged| !merged.trim().is_empty()) || normalized.as_deref() == Some("MERGED") {
        ChangeRequestState::Merged
    } else if normalized.as_deref() == Some("CLOSED") {
        ChangeRequestState::Closed
    } else {
        ChangeRequestState::Open
    }
}

/// One row, or `None` when it does not decode (so one malformed pull request cannot hide the
/// rest of a list).
pub fn decode_pull_request(entry: &Value) -> Option<ChangeRequest> {
    decode_row(entry).ok()
}

fn decode_row(entry: &Value) -> Field<ChangeRequest> {
    let object = entry.as_object().ok_or(())?;
    let number = positive_int(object.get("number"))?;
    let title = trimmed_non_empty(object.get("title"))?;
    let url = trimmed_non_empty(object.get("url"))?;
    let base_ref_name = trimmed_non_empty(object.get("baseRefName"))?;
    let head_ref_name = trimmed_non_empty(object.get("headRefName"))?;
    let head_ref_oid = optional_nullable_string(object.get("headRefOid"))?;
    let state = optional_nullable_string(object.get("state"))?;
    let is_draft = optional_bool(object.get("isDraft"))?;
    let closed_at = optional_nullable_string(object.get("closedAt"))?;
    let merged_at = optional_nullable_string(object.get("mergedAt"))?;
    let updated_at = optional_date(object.get("updatedAt"))?;
    let is_cross_repository = optional_bool(object.get("isCrossRepository"))?;
    // gh < 2.47 exports headRepository as {id, name} only; both fields stay optional so a
    // version-drifted gh can never fail the decode and silently drop the PR from a list.
    let name_with_owner = trim_optional(nested_string(object.get("headRepository"), "nameWithOwner")?.as_deref());
    let repository_name = trim_optional(nested_string(object.get("headRepository"), "name")?.as_deref());
    let owner_login = trim_optional(nested_string(object.get("headRepositoryOwner"), "login")?.as_deref())
        .or_else(|| {
            name_with_owner
                .as_ref()
                .filter(|full| full.contains('/'))
                .and_then(|full| full.split('/').next().map(str::to_string))
                .and_then(|owner| trim_optional(Some(&owner)))
        });
    let head_name_with_owner = name_with_owner.clone().or_else(|| match (&owner_login, &repository_name) {
        (Some(owner), Some(name)) => Some(format!("{owner}/{name}")),
        _ => None,
    });
    let head_sha = trim_optional(head_ref_oid.as_deref());
    Ok(ChangeRequest {
        provider: SourceControlProviderKind::Github,
        number,
        title,
        url,
        base_ref_name,
        head_ref_name,
        head_sha,
        state: state_of(state.as_deref(), merged_at.as_deref()),
        is_draft: (is_draft == Some(true)).then_some(true),
        closed_at: Some(closed_at),
        merged_at: Some(merged_at),
        updated_at,
        is_cross_repository,
        head_repository_name_with_owner: head_name_with_owner.map(Some),
        head_repository_owner_login: owner_login.map(Some),
    })
}

/// A list of rows; a row that does not decode is skipped.
pub fn decode_pull_requests(entries: &[Value]) -> Vec<ChangeRequest> {
    entries.iter().filter_map(decode_pull_request).collect()
}

/// A `gh pr list --json` array. `Err` when the text is no JSON array.
pub fn decode_pull_request_list_json(raw: &str) -> Result<Vec<ChangeRequest>, String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(entries)) => Ok(decode_pull_requests(&entries)),
        Ok(_) => Err("Expected an array".into()),
        Err(error) => Err(error.to_string()),
    }
}
