//! Checkpoint refs, ids and git access (port of `orchestration-v2/CheckpointService.ts`).
//!
//! A checkpoint is a hidden git ref capturing the whole worktree. The decisions about which
//! checkpoint exists, its status and its changed-file summary live here behind `CheckpointGit`,
//! so the capture, diff and rollback planners run unchanged against a scripted repository in
//! tests and against `t3_vcs` in the server.
use crate::{execution::at, persistence::StoreError};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};
use t3_vcs::{DiffCheckpointsInput, DiffFormat, GitVcs, VcsError, parse_turn_diff_files_from_numstat};

const CHECKPOINT_REFS_PREFIX: &str = "refs/t3/orchestration-v2/checkpoints";

/// `joinId` of the allocator: `encodeURIComponent` of each part.
pub(crate) fn encode_part(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            result.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(result, "%{byte:02X}").unwrap();
        }
    }
    result
}

/// `checkpointRefForScopeOrdinal`: the ref names a scope's checkpoint by a digest of the scope id.
pub fn ref_for_scope_ordinal(scope_id: &str, ordinal: u64) -> String {
    let digest = Sha256::digest(scope_id.as_bytes());
    let hex = digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!(
        "{CHECKPOINT_REFS_PREFIX}/{}/ordinal/{ordinal}",
        URL_SAFE_NO_PAD.encode(&hex[..32])
    )
}

/// `IdAllocator.allocate.checkpoint({checkpointScopeId, name: String(ordinal)})`.
pub fn checkpoint_id(scope_id: &str, ordinal: u64) -> String {
    format!(
        "checkpoint:scope:{}:name:{}",
        encode_part(scope_id),
        ordinal
    )
}

/// `makeRootRunScope`'s id: one root scope per thread, shared by its runs. Scope creation lives
/// in `message_steering::prepare_root_scope`; this pins its id against the original.
#[cfg(test)]
pub fn root_scope_id(thread_id: &str) -> String {
    format!("checkpoint-scope:thread:{}:name:root", encode_part(thread_id))
}

/// A failed git operation, carrying the wire `name` of the underlying error and its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFailure {
    pub name: String,
    pub message: String,
}
impl From<VcsError> for GitFailure {
    fn from(error: VcsError) -> Self {
        Self {
            name: error.tag().into(),
            message: error.to_string(),
        }
    }
}
impl std::fmt::Display for GitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// The `CheckpointStore` operations the planners use.
pub(crate) trait CheckpointGit: Send + Sync {
    fn is_repository(&self, cwd: &Path) -> impl Future<Output = bool> + Send;
    fn has_ref(
        &self,
        cwd: &Path,
        checkpoint_ref: &str,
    ) -> impl Future<Output = Result<bool, GitFailure>> + Send;
    fn capture(
        &self,
        cwd: &Path,
        checkpoint_ref: &str,
    ) -> impl Future<Output = Result<(), GitFailure>> + Send;
    fn diff(
        &self,
        input: &DiffCheckpointsInput,
    ) -> impl Future<Output = Result<String, GitFailure>> + Send;
    fn restore(
        &self,
        cwd: &Path,
        checkpoint_ref: &str,
    ) -> impl Future<Output = Result<bool, GitFailure>> + Send;
    fn delete(
        &self,
        cwd: &Path,
        checkpoint_refs: &[String],
    ) -> impl Future<Output = Result<(), GitFailure>> + Send;
}

/// `t3_vcs` behind the trait. Repository detection goes through the server's shared driver
/// registry (`vcs_registry`), whose cache `CheckpointStore.ts` also relies on, and every
/// operation resolves the repository first, as `resolveCheckpoints` does.
pub(crate) struct GitCheckpoints {
    service: &'static crate::vcs_service::VcsService,
    git: GitVcs,
}
impl GitCheckpoints {
    pub fn new() -> Self {
        let service = crate::vcs_service::VcsService::shared();
        let git = service
            .registry()
            .driver(t3_vcs::VcsDriverKind::Git)
            .expect("the registry always holds the git driver")
            .clone();
        Self { service, git }
    }
    async fn resolve(&self, cwd: &Path) -> Result<(), VcsError> {
        self.service
            .registry()
            .resolve(&cwd.to_string_lossy(), None)
            .await
            .map(|_| ())
    }
}
impl CheckpointGit for GitCheckpoints {
    async fn is_repository(&self, cwd: &Path) -> bool {
        // A detection failure counts as "not checkpointable", as in the source.
        self.service
            .registry()
            .detect(&cwd.to_string_lossy(), Some(t3_vcs::VcsDriverKind::Git))
            .await
            .is_ok_and(|handle| handle.is_some())
    }
    async fn has_ref(&self, cwd: &Path, checkpoint_ref: &str) -> Result<bool, GitFailure> {
        self.resolve(cwd).await?;
        Ok(self.git.has_checkpoint_ref(cwd, checkpoint_ref).await?)
    }
    async fn capture(&self, cwd: &Path, checkpoint_ref: &str) -> Result<(), GitFailure> {
        self.resolve(cwd).await?;
        Ok(self.git.capture_checkpoint(cwd, checkpoint_ref).await?)
    }
    async fn diff(&self, input: &DiffCheckpointsInput) -> Result<String, GitFailure> {
        self.resolve(&input.cwd).await?;
        Ok(self.git.diff_checkpoints(input).await?)
    }
    async fn restore(&self, cwd: &Path, checkpoint_ref: &str) -> Result<bool, GitFailure> {
        self.resolve(cwd).await?;
        Ok(self.git.restore_checkpoint(cwd, checkpoint_ref, false).await?)
    }
    async fn delete(&self, cwd: &Path, checkpoint_refs: &[String]) -> Result<(), GitFailure> {
        self.resolve(cwd).await?;
        Ok(self.git.delete_checkpoint_refs(cwd, checkpoint_refs).await?)
    }
}

/// The service: per-workspace serialization of git work plus the checkpoint decisions.
pub(crate) struct CheckpointService<G: CheckpointGit> {
    git: G,
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

pub(crate) type Service = CheckpointService<GitCheckpoints>;

/// The server's one service. Locks must be shared by capture, rollback and thread deletion.
pub(crate) fn shared() -> &'static Service {
    static SHARED: OnceLock<Service> = OnceLock::new();
    SHARED.get_or_init(|| CheckpointService::new(GitCheckpoints::new()))
}

fn failure(message: impl Into<String>) -> StoreError {
    StoreError::InvalidCommand(message.into())
}

impl<G: CheckpointGit> CheckpointService<G> {
    pub fn new(git: G) -> Self {
        Self {
            git,
            locks: Mutex::new(HashMap::new()),
        }
    }
    pub fn git(&self) -> &G {
        &self.git
    }
    async fn lock(&self, cwd: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .locks
            .lock()
            .unwrap()
            .entry(PathBuf::from(cwd))
            .or_default()
            .clone();
        lock.lock_owned().await
    }
    async fn checkpointable(&self, cwd: &str) -> bool {
        self.git.is_repository(Path::new(cwd)).await
    }

    /// `captureBaseline`: record the workspace before a turn unless that ref already exists.
    pub async fn capture_baseline(&self, scope: &Value, ordinal: u64) -> Result<(), GitFailure> {
        let cwd = scope["cwd"].as_str().unwrap_or("");
        let _guard = self.lock(cwd).await;
        if !self.checkpointable(cwd).await {
            return Ok(());
        }
        let reference = ref_for_scope_ordinal(scope["id"].as_str().unwrap(), ordinal);
        if self.git.has_ref(Path::new(cwd), &reference).await? {
            return Ok(());
        }
        self.git.capture(Path::new(cwd), &reference).await
    }

    /// `materializeBaselineCheckpoint`: the record of a baseline ref, `missing` without one.
    pub async fn materialize_baseline(&self, scope: &Value, ordinal: u64) -> Value {
        let cwd = scope["cwd"].as_str().unwrap_or("");
        let scope_id = scope["id"].as_str().unwrap();
        let _guard = self.lock(cwd).await;
        let reference = ref_for_scope_ordinal(scope_id, ordinal);
        let available = self.checkpointable(cwd).await
            && match self.git.has_ref(Path::new(cwd), &reference).await {
                Ok(found) => found,
                Err(error) => {
                    tracing::warn!(scope_id, %error, "baseline ref lookup failed");
                    false
                }
            };
        json!({"id":checkpoint_id(scope_id,ordinal),"threadId":scope["threadId"],"scopeId":scope_id,"runId":null,"nodeId":scope["nodeId"],"parentCheckpointId":null,"ordinalWithinScope":ordinal,"appRunOrdinal":null,"ref":reference,"status":if available{"ready"}else{"missing"},"files":[],"capturedAt":scope["createdAt"]})
    }

    /// `capture`: snapshot the workspace and summarise what changed since the previous ordinal.
    /// Git failures degrade the checkpoint to `error` or `missing`; they never fail the turn.
    pub async fn capture(
        &self,
        scope: &Value,
        run_id: &str,
        node_id: &str,
        ordinal: u64,
        captured_at: DateTime<Utc>,
    ) -> Value {
        let cwd = scope["cwd"].as_str().unwrap_or("");
        let scope_id = scope["id"].as_str().unwrap();
        let _guard = self.lock(cwd).await;
        let reference = ref_for_scope_ordinal(scope_id, ordinal);
        let previous_ref = ref_for_scope_ordinal(scope_id, ordinal.saturating_sub(1));
        let checkpoint = |status: &str, files: Vec<Value>| {
            json!({"id":checkpoint_id(scope_id,ordinal),"threadId":scope["threadId"],"scopeId":scope_id,"runId":run_id,"nodeId":node_id,
                "parentCheckpointId":if ordinal>0{json!(checkpoint_id(scope_id,ordinal-1))}else{Value::Null},
                "ordinalWithinScope":ordinal,"appRunOrdinal":ordinal,"ref":reference,"status":status,"files":files,"capturedAt":at(captured_at)})
        };
        if !self.checkpointable(cwd).await {
            return checkpoint("missing", vec![]);
        }
        if let Err(error) = self.git.capture(Path::new(cwd), &reference).await {
            tracing::warn!(scope_id, reference, %error, "checkpoint capture failed");
            return checkpoint("error", vec![]);
        }
        let previous_exists = match self.git.has_ref(Path::new(cwd), &previous_ref).await {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(scope_id, previous_ref, %error, "previous checkpoint ref lookup failed");
                false
            }
        };
        let mut files = vec![];
        if previous_exists {
            let numstat = self
                .git
                .diff(&DiffCheckpointsInput {
                    cwd: PathBuf::from(cwd),
                    from_checkpoint_ref: previous_ref,
                    to_checkpoint_ref: reference.clone(),
                    fallback_from_to_head: false,
                    ignore_whitespace: false,
                    format: DiffFormat::Numstat,
                })
                .await;
            match numstat {
                Ok(numstat) => {
                    files = parse_turn_diff_files_from_numstat(&numstat)
                        .into_iter()
                        .map(|file| json!({"path":file.path,"kind":"modified","additions":file.additions,"deletions":file.deletions}))
                        .collect()
                }
                Err(error) => {
                    tracing::warn!(scope_id, reference, %error, "checkpoint diff summary failed")
                }
            }
        }
        checkpoint("ready", files)
    }

    /// `restore`: put the worktree back to a ready checkpoint (rollback).
    pub async fn restore(&self, scope: &Value, checkpoint: &Value) -> Result<(), StoreError> {
        let cwd = scope["cwd"].as_str().unwrap_or("");
        let _guard = self.lock(cwd).await;
        if checkpoint["status"] != "ready" {
            return Err(restore_error(
                scope,
                checkpoint,
                &format!("Checkpoint status is {}.", checkpoint["status"].as_str().unwrap_or("")),
            ));
        }
        let restored = self
            .git
            .restore(Path::new(cwd), checkpoint["ref"].as_str().unwrap_or(""))
            .await
            .map_err(|error| restore_error(scope, checkpoint, &error.message))?;
        if !restored {
            return Err(restore_error(scope, checkpoint, "Checkpoint ref is unavailable."));
        }
        Ok(())
    }

    /// `deleteStaleRefs`: best effort removal of refs no checkpoint record points at any more.
    pub async fn delete_refs(&self, cwd: &str, refs: &[String]) -> Result<(), GitFailure> {
        let _guard = self.lock(cwd).await;
        self.git.delete(Path::new(cwd), refs).await
    }
}

fn restore_error(scope: &Value, checkpoint: &Value, cause: &str) -> StoreError {
    failure(format!(
        "Failed to restore checkpoint {} for scope {}. {cause}",
        checkpoint["id"].as_str().unwrap_or(""),
        scope["id"].as_str().unwrap_or("")
    ))
}

#[cfg(test)]
pub(crate) mod scripted {
    //! A scripted repository for the planners' tests.
    use super::*;

    fn ordinal_of(reference: &str) -> &str {
        reference.rsplit('/').next().unwrap_or(reference)
    }
    #[derive(Default)]
    pub struct Script {
        pub repository: bool,
        pub refs: Mutex<Vec<String>>,
        pub fail_capture: bool,
        pub fail_has_ref: bool,
        pub fail_diff: bool,
        pub numstat: String,
        pub calls: Mutex<Vec<String>>,
    }
    impl CheckpointGit for Script {
        async fn is_repository(&self, _: &Path) -> bool {
            self.repository
        }
        async fn has_ref(&self, _: &Path, reference: &str) -> Result<bool, GitFailure> {
            self.calls.lock().unwrap().push(format!("has {}", ordinal_of(reference)));
            if self.fail_has_ref {
                return Err(GitFailure { name: "VcsProcessExitError".into(), message: "has ref".into() });
            }
            Ok(self.refs.lock().unwrap().iter().any(|known| known == reference))
        }
        async fn capture(&self, _: &Path, reference: &str) -> Result<(), GitFailure> {
            self.calls.lock().unwrap().push(format!("capture {}", ordinal_of(reference)));
            if self.fail_capture {
                return Err(GitFailure { name: "VcsProcessExitError".into(), message: "capture".into() });
            }
            self.refs.lock().unwrap().push(reference.into());
            Ok(())
        }
        async fn diff(&self, input: &DiffCheckpointsInput) -> Result<String, GitFailure> {
            self.calls.lock().unwrap().push(format!(
                "diff {}..{} {} ws={}",
                ordinal_of(&input.from_checkpoint_ref),
                ordinal_of(&input.to_checkpoint_ref),
                if input.format == DiffFormat::Numstat { "numstat" } else { "patch" },
                input.ignore_whitespace
            ));
            if self.fail_diff {
                return Err(GitFailure { name: "VcsProcessExitError".into(), message: "diff".into() });
            }
            Ok(self.numstat.clone())
        }
        async fn restore(&self, _: &Path, reference: &str) -> Result<bool, GitFailure> {
            self.calls.lock().unwrap().push(format!("restore {reference}"));
            Ok(self.refs.lock().unwrap().iter().any(|known| known == reference))
        }
        async fn delete(&self, _: &Path, refs: &[String]) -> Result<(), GitFailure> {
            self.calls.lock().unwrap().push(format!("delete {}", refs.join(",")));
            self.refs.lock().unwrap().retain(|known| !refs.contains(known));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scripted::Script;

    #[tokio::test]
    async fn a_baseline_is_captured_once_and_only_inside_a_repository() {
        let scope = json!({"id":"scope:1","threadId":"thread","nodeId":"node","cwd":"/repo","createdAt":"2026-01-01T00:00:00.000Z"});
        let service = CheckpointService::new(Script { repository: true, ..Default::default() });
        service.capture_baseline(&scope, 0).await.unwrap();
        service.capture_baseline(&scope, 0).await.unwrap();
        assert_eq!(service.git().calls.lock().unwrap().iter().filter(|call| call.starts_with("capture")).count(), 1);
        assert_eq!(service.materialize_baseline(&scope, 0).await["status"], "ready");
        let outside = CheckpointService::new(Script::default());
        outside.capture_baseline(&scope, 0).await.unwrap();
        assert!(outside.git().calls.lock().unwrap().is_empty());
        assert_eq!(outside.materialize_baseline(&scope, 0).await["status"], "missing");
    }

    #[tokio::test]
    async fn restore_requires_a_ready_checkpoint_whose_ref_exists_and_stale_refs_are_deleted() {
        let service = CheckpointService::new(Script { repository: true, ..Default::default() });
        let scope = json!({"id":"scope:1","threadId":"thread","nodeId":"node","cwd":"/repo","createdAt":"2026-01-01T00:00:00.000Z"});
        let now = Utc::now();
        let first = service.capture(&scope, "run:1", "node", 1, now).await;
        assert_eq!(first["status"], "ready");
        service.restore(&scope, &first).await.unwrap();
        let mut stale = first.clone();
        stale["status"] = json!("stale");
        assert!(service.restore(&scope, &stale).await.unwrap_err().to_string().contains("Checkpoint status is stale."));
        let mut absent = first.clone();
        absent["ref"] = json!(ref_for_scope_ordinal("scope:1", 9));
        assert!(service.restore(&scope, &absent).await.unwrap_err().to_string().contains("Checkpoint ref is unavailable."));
        service.delete_refs("/repo", &[first["ref"].as_str().unwrap().to_owned()]).await.unwrap();
        assert!(service.restore(&scope, &first).await.is_err());
    }
}
