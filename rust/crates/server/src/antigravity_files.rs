//! Workspace file access requested through the ACP client `fs` capability: `AntigravityClientFiles.ts`.
//! The agent gates each write behind `session/request_permission`, so only path containment is checked here.
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use t3_acp::{AcpError, protocol::RpcError};

const CLIENT_FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;

fn request_error(code: i64, message: String) -> AcpError {
    AcpError::Request(RpcError { code, message, data: None })
}
fn invalid_params(message: String) -> AcpError {
    request_error(-32602, message)
}
/// `path.resolve`: absolute, with `.` and `..` folded away.
fn resolve(path: &str) -> PathBuf {
    let absolute = if Path::new(path).is_absolute() { PathBuf::from(path) } else { std::env::current_dir().unwrap_or_default().join(path) };
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
/// Whether `candidate` is `root` or below it; a first path segment that starts with `..` is never inside
/// (JavaScript reads `relative.startsWith("..")`).
fn inside(root: &Path, candidate: &Path) -> bool {
    match candidate.strip_prefix(root) {
        Ok(relative) => relative.components().next().is_none_or(|first| !first.as_os_str().to_string_lossy().starts_with("..")),
        Err(_) => false,
    }
}
/// Resolves an agent-supplied path and rejects anything outside the session roots. Symlinks resolve before the
/// check, including the final component, so a link inside the workspace cannot read or write through to a file
/// outside it. An entry that exists but cannot be resolved, like a dangling link, is rejected rather than
/// written through.
async fn resolve_client_path(allowed_roots: &[PathBuf], request_path: &str) -> Result<PathBuf, AcpError> {
    let outside = || invalid_params(format!("Path '{request_path}' is outside the session workspace."));
    let resolved = resolve(request_path);
    let real = match tokio::fs::canonicalize(&resolved).await {
        Ok(real) => real,
        Err(_) => {
            // Only a missing file (a new write) falls back to its parent.
            let exists = tokio::fs::symlink_metadata(&resolved).await.is_ok() || tokio::fs::try_exists(&resolved).await.unwrap_or(true);
            if exists {
                return Err(outside());
            }
            let parent = resolved.parent().map(Path::to_path_buf).unwrap_or_default();
            // A directory that does not exist yet cannot be told from a path that leads out, as the source reads it.
            if !tokio::fs::try_exists(&parent).await.unwrap_or(false) {
                return Err(outside());
            }
            let parent = tokio::fs::canonicalize(&parent).await.unwrap_or(parent);
            parent.join(resolved.file_name().unwrap_or_default())
        }
    };
    let mut roots = vec![];
    for root in allowed_roots {
        roots.push(tokio::fs::canonicalize(root).await.unwrap_or_else(|_| root.clone()));
    }
    if roots.iter().any(|root| inside(root, &real)) { Ok(real) } else { Err(outside()) }
}
/// `readAntigravityClientTextFile`.
pub async fn read_text_file(allowed_roots: &[PathBuf], request: &Value) -> Result<Value, AcpError> {
    let request_path = request["path"].as_str().unwrap_or("");
    let path = resolve_client_path(allowed_roots, request_path).await?;
    let info = tokio::fs::metadata(&path).await.map_err(|_| request_error(-32002, format!("File '{request_path}' not found.")))?;
    if !info.is_file() || info.len() > CLIENT_FILE_MAX_BYTES {
        return Err(invalid_params(format!("File '{request_path}' is not a readable text file under {CLIENT_FILE_MAX_BYTES} bytes.")));
    }
    let text = tokio::fs::read_to_string(&path).await.map_err(|_| request_error(-32603, format!("Could not read '{request_path}'.")))?;
    let (line, limit) = (request["line"].as_i64(), request["limit"].as_i64());
    if line.is_none() && limit.is_none() {
        return Ok(json!({"content":text}));
    }
    // ACP lines are 1-indexed. `limit` is a line count.
    let lines: Vec<&str> = text.split('\n').collect();
    let start = (line.unwrap_or(1) - 1).max(0) as usize;
    let end = match limit {
        None => lines.len(),
        Some(limit) => lines.len().min(start.saturating_add(limit.max(0) as usize)),
    };
    Ok(json!({"content":lines.get(start..end.max(start)).map(|lines| lines.join("\n")).unwrap_or_default()}))
}
/// `writeAntigravityClientTextFile`.
pub async fn write_text_file(allowed_roots: &[PathBuf], request: &Value) -> Result<Value, AcpError> {
    let request_path = request["path"].as_str().unwrap_or("");
    let path = resolve_client_path(allowed_roots, request_path).await?;
    let content = request["content"].as_str().unwrap_or("");
    let written = async {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, content).await
    }
    .await;
    written.map_err(|_| request_error(-32603, format!("Could not write '{request_path}'.")))?;
    Ok(json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn file_requests_are_confined_to_the_session_roots_as_the_source_confines_them() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/antigravity-pure.json")).unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let (workspace, outside, attachments) = (root.join("workspace"), root.join("outside"), root.join("attachments"));
        for directory in [&workspace, &outside, &attachments, &workspace.join("sub")] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(workspace.join("a.txt"), "one\ntwo\nthree\nfour").unwrap();
        std::fs::write(workspace.join("..odd"), "dots").unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::fs::write(attachments.join("pasted.txt"), "pasted").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), workspace.join("link-out")).unwrap();
        std::os::unix::fs::symlink(workspace.join("a.txt"), workspace.join("link-in")).unwrap();
        std::os::unix::fs::symlink(root.join("missing-target"), workspace.join("dangling")).unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("dir-out")).unwrap();
        let roots = vec![workspace.clone(), attachments.clone()];
        let root_text = root.to_string_lossy().into_owned();
        let report = |result: Result<Value, AcpError>| match result {
            Ok(value) => value,
            Err(AcpError::Request(error)) => json!({"code":error.code,"message":error.message.replace(&root_text, "<root>")}),
            Err(other) => json!({"other":other.to_string()}),
        };
        for case in fixture["reads"].as_array().unwrap() {
            let mut request = case["request"].clone();
            request["path"] = json!(request["path"].as_str().unwrap().replace("<root>", &root_text));
            let result = report(read_text_file(&roots, &request).await);
            assert_eq!(result, case["result"], "{}", case["request"]);
        }
        for case in fixture["writes"].as_array().unwrap() {
            let mut request = case["request"].clone();
            request["path"] = json!(request["path"].as_str().unwrap().replace("<root>", &root_text));
            let result = report(write_text_file(&roots, &request).await);
            let expected = if case["result"]["ok"] == true { json!({}) } else { case["result"].clone() };
            assert_eq!(result, expected, "{}", case["request"]);
        }
        assert_eq!(std::fs::read_to_string(workspace.join("new.txt")).unwrap(), "fresh");
        assert!(!workspace.join("sub/deep").exists());
        assert_eq!(std::fs::read_to_string(outside.join("secret.txt")).unwrap(), "secret");
        assert!(!outside.join("new.txt").exists());
    }
}
