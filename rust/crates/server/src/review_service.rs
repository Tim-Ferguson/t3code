//! `review.getDiffPreview` and `review.getDiffFileContents` (port of `ReviewService.ts`): the
//! "Uncommitted" and "Changes" diffs of a checkout, and the unchanged lines of one file in them.
//! The git work is `t3_vcs::review`; what this adds is the boundary: a directory the server was
//! not started to serve, and that is not one of its managed worktrees, is refused before git is
//! asked anything. Nothing here reads outside the directory it was given.
use crate::{
    transport::ApiState,
    vcs_error::VcsRpcError,
    vcs_rpc,
    vcs_service::{VcsContext, VcsService},
    vcs_worktree::resolve_worktrees_directory,
};
use chrono::Utc;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use t3_vcs::{
    VcsDriverKind, VcsError,
    wire::{ReviewDiffFileContentsInput, ReviewDiffPreviewInput, ReviewDiffPreviewResult},
};

pub const METHODS: [&str; 2] = ["review.getDiffPreview", "review.getDiffFileContents"];

type ReviewResult<T> = Result<T, VcsRpcError>;

fn detection_error(operation: &str, cwd: &str, detail: &str) -> VcsRpcError {
    VcsRpcError::Vcs(VcsError::RepositoryDetection { operation: operation.into(), cwd: cwd.into(), detail: detail.into() })
}

/// `path.resolve`: absolute, with `.` and `..` folded away, without touching the disk.
fn resolve(value: &str) -> PathBuf {
    let path = Path::new(value);
    let absolute = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(path) };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// The path with symlinks resolved; a path that does not exist stays as written, because a
/// missing directory is the caller's to report and not a way past the boundary.
async fn canonicalize(value: &str) -> ReviewResult<PathBuf> {
    let resolved = resolve(value);
    match tokio::fs::canonicalize(&resolved).await {
        Ok(real) => Ok(real),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(resolved),
        Err(_) => Err(detection_error(
            "ReviewService.assertWorkspaceBoundCwd.canonicalizePath",
            &resolved.to_string_lossy(),
            "Failed to resolve a path while validating the review workspace.",
        )),
    }
}

fn is_filesystem_root(directory: &Path) -> bool {
    directory.parent().is_none()
}

/// Every directory that holds T3-managed worktrees on this machine: the default one, the one the
/// setting names and the ones it named before (`managedWorktreesDirectories`).
pub fn managed_worktrees_directories(worktrees_directory: &str, previous: &[String], default_dir: &Path) -> Vec<PathBuf> {
    let mut directories = vec![default_dir.to_path_buf()];
    for setting in std::iter::once(worktrees_directory).chain(previous.iter().map(String::as_str)) {
        if let Some(directory) = resolve_worktrees_directory(setting, default_dir) {
            if !directories.contains(&directory) {
                directories.push(directory);
            }
        }
    }
    directories
}

/// Whether `candidate` is `root` or below it.
pub fn is_within_root(candidate: &Path, root: &Path) -> bool {
    candidate.starts_with(root)
}

/// `assertWorkspaceBoundCwd`: the review runs only inside the workspace the server was started in
/// or one of its managed worktree directories.
async fn assert_workspace_bound(operation: &str, cwd: &str, workspace_root: &str, context: &VcsContext) -> ReviewResult<()> {
    let (worktrees_directory, previous) = match context.settings_snapshot().await {
        Some(settings) => (settings.worktrees_directory.as_str().to_string(), settings.previous_worktrees_directories.iter().map(|directory| directory.as_str().to_string()).collect()),
        None => (String::new(), vec![]),
    };
    let candidate = canonicalize(cwd).await?;
    let workspace = canonicalize(workspace_root).await?;
    let mut roots = vec![];
    // A managed root that cannot be resolved, or resolves to a filesystem root through a
    // symlink, is skipped rather than failing every review.
    for directory in managed_worktrees_directories(&worktrees_directory, &previous, &context.default_worktrees_dir()) {
        if let Ok(root) = canonicalize(&directory.to_string_lossy()).await {
            if !is_filesystem_root(&root) {
                roots.push(root);
            }
        }
    }
    if is_within_root(&candidate, &workspace) || roots.iter().any(|root| is_within_root(&candidate, root)) {
        return Ok(());
    }
    let detail = if operation == "ReviewService.getDiffPreview" {
        "Review diff preview cwd must stay within the configured workspace root."
    } else {
        "Review diff file contents cwd must stay within the configured workspace root."
    };
    Err(detection_error(operation, cwd, detail))
}

pub async fn get_diff_preview(input: &ReviewDiffPreviewInput, workspace_root: &str, context: &VcsContext) -> ReviewResult<ReviewDiffPreviewResult> {
    assert_workspace_bound("ReviewService.getDiffPreview", &input.cwd, workspace_root, context).await?;
    let service = VcsService::shared();
    let Some(handle) = service.registry().detect(&input.cwd, None).await? else {
        return Ok(ReviewDiffPreviewResult { cwd: input.cwd.clone(), generated_at: Utc::now(), sources: vec![] });
    };
    if handle.kind != VcsDriverKind::Git {
        return Err(VcsRpcError::Vcs(VcsError::UnsupportedOperation {
            operation: "ReviewService.getDiffPreview".into(),
            kind: handle.kind,
            detail: format!("The {} VCS driver does not support review diff previews.", handle.kind.as_str()),
        }));
    }
    Ok(Box::pin(service.core().get_review_diff_preview(input)).await?)
}

pub async fn get_diff_file_contents(input: &ReviewDiffFileContentsInput, workspace_root: &str, context: &VcsContext) -> ReviewResult<t3_vcs::wire::ReviewDiffFileContentsResult> {
    assert_workspace_bound("ReviewService.getDiffFileContents", &input.cwd, workspace_root, context).await?;
    let service = VcsService::shared();
    let handle = service.registry().detect(&input.cwd, None).await?;
    let kind = handle.map_or(VcsDriverKind::Unknown, |handle| handle.kind);
    if kind != VcsDriverKind::Git {
        return Err(VcsRpcError::Vcs(VcsError::UnsupportedOperation {
            operation: "ReviewService.getDiffFileContents".into(),
            kind,
            detail: "Unchanged diff expansion currently requires a Git repository.".into(),
        }));
    }
    Ok(Box::pin(service.core().get_review_diff_file_contents(input)).await?)
}

// ---------------------------------------------------------------------------------------------
// The socket methods
// ---------------------------------------------------------------------------------------------

fn decode_failure(message: impl ToString) -> Value {
    json!({"_tag":"SchemaDecodeError","message":message.to_string()})
}

/// A trimmed non-empty string, the way the contract reads one.
fn trimmed(value: &str, field: &str) -> Result<String, Value> {
    let text = t3_contracts::trim_wire_string(value);
    if text.is_empty() { Err(decode_failure(format!("Expected a non-empty string for {field}."))) } else { Ok(text.to_string()) }
}

fn non_empty(value: &str, field: &str) -> Result<(), Value> {
    if value.is_empty() { Err(decode_failure(format!("Expected a non-empty string for {field}."))) } else { Ok(()) }
}

/// `ReviewDiffPreviewInput`, checked against the contract's bounds.
pub fn decode_preview_input(payload: Value) -> Result<ReviewDiffPreviewInput, Value> {
    let mut input: ReviewDiffPreviewInput = serde_json::from_value(payload).map_err(decode_failure)?;
    input.cwd = trimmed(&input.cwd, "cwd")?;
    if let Some(base) = &input.base_ref {
        input.base_ref = Some(trimmed(base, "baseRef")?);
    }
    if let Some(file) = &input.file {
        non_empty(&file.path, "file.path")?;
        if let Some(previous) = &file.previous_path {
            non_empty(previous, "file.previousPath")?;
        }
    }
    Ok(input)
}

/// `ReviewDiffFileContentsInput`, checked against the contract's bounds.
pub fn decode_contents_input(payload: Value) -> Result<ReviewDiffFileContentsInput, Value> {
    let mut input: ReviewDiffFileContentsInput = serde_json::from_value(payload).map_err(decode_failure)?;
    input.cwd = trimmed(&input.cwd, "cwd")?;
    input.base_ref = input.base_ref.as_deref().map(|base| trimmed(base, "baseRef")).transpose()?;
    input.head_ref = input.head_ref.as_deref().map(|head| trimmed(head, "headRef")).transpose()?;
    input.old_path = trimmed(&input.old_path, "oldPath")?;
    input.new_path = trimmed(&input.new_path, "newPath")?;
    Ok(input)
}

/// The directory the server serves: the configuration's `cwd`, else where it was started.
fn workspace_root(state: &ApiState) -> String {
    state
        .config
        .as_ref()
        .and_then(|config| config["cwd"].as_str().map(str::to_string))
        .unwrap_or_else(|| std::env::current_dir().map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default())
}

pub async fn command(state: &ApiState, tag: &str, payload: Value) -> Result<Value, Value> {
    let (context, root) = (vcs_rpc::context(state), workspace_root(state));
    match tag {
        "review.getDiffPreview" => {
            let input = decode_preview_input(payload)?;
            let result = Box::pin(get_diff_preview(&input, &root, &context)).await;
            result.map(|result| serde_json::to_value(result).expect("typed review result")).map_err(|error| error.wire())
        }
        "review.getDiffFileContents" => {
            let input = decode_contents_input(payload)?;
            let result = Box::pin(get_diff_file_contents(&input, &root, &context)).await;
            result.map(|result| serde_json::to_value(result).expect("typed review result")).map_err(|error| error.wire())
        }
        _ => Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_directories_are_the_default_the_setting_and_the_ones_it_replaced() {
        let default = Path::new("/home/t3/worktrees");
        // Nothing configured: the default alone.
        assert_eq!(managed_worktrees_directories("", &[], default), vec![default.to_path_buf()]);
        // The setting and the earlier ones follow, once each; a relative path or a filesystem
        // root names no directory this machine manages and is left out.
        let previous = ["/old/trees".to_string(), "/srv/trees".to_string(), "relative/trees".to_string(), "/".to_string(), "/home/t3/worktrees".to_string()];
        assert_eq!(
            managed_worktrees_directories("/srv/trees", &previous, default),
            vec![default.to_path_buf(), PathBuf::from("/srv/trees"), PathBuf::from("/old/trees")]
        );
    }

    #[test]
    fn a_directory_is_within_a_root_only_when_it_is_the_root_or_below_it() {
        let root = Path::new("/work/space");
        assert!(is_within_root(Path::new("/work/space"), root));
        assert!(is_within_root(Path::new("/work/space/repo/deep"), root));
        assert!(!is_within_root(Path::new("/work/space-other"), root), "a sibling that shares a prefix is outside");
        assert!(!is_within_root(Path::new("/work"), root));
        assert_eq!(resolve("/work/space/../elsewhere/./x"), PathBuf::from("/work/elsewhere/x"));
    }
}
