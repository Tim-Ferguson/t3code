//! Tracked repository clones: the stream's snapshots and how they are worded. Port of the helpers
//! of `projectClone.ts` and the toast and banner of `ProjectCloneToastCoordinator.tsx` and
//! `ChatView.tsx`, replayed against the originals by `tests/import_clone_parity.rs`.
use serde::Deserialize;
use serde_json::Value;

pub const START: &str = "projectClone.start";
pub const CANCEL: &str = "projectClone.cancel";
pub const RETRY: &str = "projectClone.retry";
pub const STREAM: &str = "subscribeProjectClones";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repository {
    pub name_with_owner: String,
}
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub project_id: String,
    pub remote_url: String,
    pub destination_path: String,
    #[serde(default)]
    pub repository: Option<Repository>,
    pub phase: String,
    pub stage: String,
    #[serde(default)]
    pub percent: Option<f64>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub sequence: f64,
}

/// The list the stream sends first and again after every change; an entry that does not read is skipped.
pub fn decode_list(values: &[Value]) -> Vec<Snapshot> {
    values.iter().filter_map(Value::as_array).flatten().filter_map(|entry| serde_json::from_value(entry.clone()).ok()).collect()
}

fn stage_label(stage: &str) -> &'static str {
    match stage {
        "connecting" => "Connecting",
        "counting" => "Counting objects",
        "receiving" => "Receiving objects",
        "resolving" => "Resolving deltas",
        _ => "Checking out files",
    }
}
/// The looked-up `owner/repo`, else the folder being cloned into.
pub fn display_name(snapshot: &Snapshot) -> String {
    if let Some(repository) = &snapshot.repository {
        return repository.name_with_owner.clone();
    }
    snapshot.destination_path.split(['/', '\\']).filter(|segment| !segment.is_empty()).next_back().map(str::to_owned).unwrap_or_else(|| snapshot.destination_path.clone())
}
/// `Receiving objects \u{b7} 45% \u{b7} 12.3 MiB | 5.0 MiB/s`.
pub fn progress_summary(snapshot: &Snapshot) -> String {
    let mut parts = vec![stage_label(&snapshot.stage).to_owned()];
    if let Some(percent) = snapshot.percent {
        parts.push(format!("{percent}%"));
    }
    if let Some(detail) = snapshot.detail.as_ref().filter(|detail| !detail.is_empty()) {
        parts.push(detail.clone());
    }
    parts.join(" \u{b7} ")
}
/// Why a thread cannot be started in a project whose clone has not landed, or `None`.
pub fn send_block_reason(snapshot: Option<&Snapshot>) -> Option<&'static str> {
    match snapshot.map(|snapshot| snapshot.phase.as_str()) {
        None | Some("done") => None,
        Some("running") => Some("Cloning repository"),
        Some(_) => Some("Repository not cloned"),
    }
}

/// The input that starts a tracked clone: the server creates the project at the destination and
/// clones into it in the background.
pub fn start_input(project_id: &str, title: &str, created_at: &str, remote_url: &str, destination: &str) -> Value {
    serde_json::json!({"projectId":project_id,"title":title,"createdAt":created_at,"remoteUrl":remote_url,"destinationPath":destination})
}
