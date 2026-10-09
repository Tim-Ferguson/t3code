//! The cleanup effects a thread deletion queues (`ResourceCleanupService.ts`): closing the
//! thread's terminals with their history, and removing the attachment files its messages carry.
//! Both are safe to run again, and both leave a thread's worktree and checkpoint refs alone.
use crate::{persistence::{Effect, Store}, workspace_prepare::WorkspaceRuntime};
use serde_json::json;
use std::{path::Path, sync::Arc, time::Duration};

/// When a failed cleanup is tried again, or `None` once it has had its attempts.
pub(crate) fn retry_delay(attempt_count: u64) -> Option<chrono::Duration> {
    (attempt_count < 5).then(|| chrono::Duration::seconds(2 * attempt_count.max(1) as i64))
}

pub(crate) async fn execute(store: &Store, workspace: &Arc<WorkspaceRuntime>, effect: &Effect) -> Result<(), String> {
    match effect.request["type"].as_str() {
        Some("terminal.cleanup") => {
            // Archiving and deleting queue this effect and nothing else; both are terminal for the
            // thread (the detach effect's `revokeMcpCredential`), and it runs whether or not a
            // provider process is still alive to receive that detach.
            revoke_mcp_credentials(workspace, &effect.thread_id);
            close_terminals(workspace, &effect.thread_id).await
        }
        Some("attachment.cleanup") => {
            let ids: Vec<String> = effect.request["attachmentIds"]
                .as_array()
                .map(|ids| ids.iter().filter_map(|id| id.as_str().map(str::to_owned)).collect())
                .unwrap_or_default();
            let store = store.clone();
            let thread = effect.thread_id.clone();
            tokio::task::spawn_blocking(move || remove_attachments(&store, &thread, &ids))
                .await
                .map_err(|error| error.to_string())?
        }
        other => Err(format!("{} is not a cleanup effect.", other.unwrap_or(""))),
    }
}

/// `clearMcpSession`: no credential issued to the thread may outlive it.
fn revoke_mcp_credentials(workspace: &Arc<WorkspaceRuntime>, thread: &str) {
    if let (Some(sessions), Ok(thread)) = (workspace.mcp(), thread.parse()) {
        sessions.revoke_thread(&thread);
    }
}

/// `cleanupTerminals`: closes every terminal of the thread and deletes their history. A server
/// without a terminal service never opened one.
async fn close_terminals(workspace: &Arc<WorkspaceRuntime>, thread: &str) -> Result<(), String> {
    let Some(terminals) = workspace.terminals() else { return Ok(()) };
    let input = serde_json::from_value::<t3_contracts::TerminalCloseInput>(json!({"threadId":thread,"deleteHistory":true}))
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(Duration::from_secs(30), terminals.close(input))
        .await
        .map_err(|_| "Closing the thread's terminals timed out.".to_owned())?
        .map_err(|failure| failure.wire().to_string())
}

/// `cleanupAttachments`: removes each attachment's file from the attachments directory. An id
/// that names no file, or that is not a plain attachment id, is skipped (`resolve` refuses
/// anything with a separator or a dot), and nothing outside the directory is ever touched.
/// Narrower than the original: a file that a message of another live thread still carries stays.
fn remove_attachments(store: &Store, thread: &str, ids: &[String]) -> Result<(), String> {
    let Some(directory) = store.state_dir().map(|state| state.join("attachments")) else { return Ok(()) };
    let still_used: Vec<String> = store
        .projections("thread")
        .map_err(|error| error.to_string())?
        .iter()
        .filter(|projection| projection["thread"]["id"] != thread && projection["thread"]["deletedAt"].is_null())
        .flat_map(|projection| crate::thread_deletion::attachment_ids(projection))
        .collect();
    let mut failures = vec![];
    for id in ids.iter().filter(|id| !still_used.contains(id)) {
        let Some(path) = crate::attachment_store::resolve_attachment_path_by_id(&directory, id) else { continue };
        remove_file(&path, &directory, &mut failures);
    }
    if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
}

fn remove_file(path: &Path, directory: &Path, failures: &mut Vec<String>) {
    // `resolve` keeps the path inside the directory; this keeps it a single file.
    if path.parent() != Some(directory) {
        return;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => failures.push(format!("{}: {error}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_cleanup_is_retried_a_few_times() {
        assert_eq!(retry_delay(1), Some(chrono::Duration::seconds(2)));
        assert_eq!(retry_delay(4), Some(chrono::Duration::seconds(8)));
        assert_eq!(retry_delay(5), None);
    }

    #[tokio::test]
    async fn terminal_cleanup_revokes_the_threads_mcp_credential_and_is_safe_to_repeat() {
        let registry = crate::mcp_sessions::McpSessionRegistry::new("environment".parse().unwrap(), None, Arc::new(|| 0), 1000);
        let sessions = crate::provider_mcp::ProviderMcpSessions::new(registry.clone(), std::path::PathBuf::from("/t3-server"));
        let workspace = Arc::new(WorkspaceRuntime::new());
        workspace.attach_mcp(sessions.clone());
        let lease = sessions.reserve("thread-1".parse().unwrap(), "codex".parse().unwrap(), false, false).unwrap();
        let other = sessions.reserve("thread-2".parse().unwrap(), "codex".parse().unwrap(), false, false).unwrap();
        let token = |lease: &crate::provider_mcp::CredentialLease| lease.config.authorization_header.trim_start_matches("Bearer ").to_owned();
        let effect = Effect {
            id: "effect".into(), command_id: "command".into(), thread_id: "thread-1".into(), request: json!({"type":"terminal.cleanup"}),
            status: "running".into(), attempt_count: 1, available_at: String::new(), lease_owner: None, lease_expires_at: None,
            created_at: String::new(), updated_at: String::new(), last_error: None,
        };
        let store = Store::memory().unwrap();
        execute(&store, &workspace, &effect).await.unwrap();
        execute(&store, &workspace, &effect).await.unwrap();
        assert!(registry.resolve(&token(&lease)).is_none());
        assert!(registry.resolve(&token(&other)).is_some());
    }
}
