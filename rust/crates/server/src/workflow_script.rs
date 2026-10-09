//! `orchestration.getWorkflowScript`: read-only access to the workflow scripts the Claude harness
//! persists under `~/.claude/projects`, for the Agents surface's script view
//! (`orchestration-v2/workflowScriptQuery.ts`). The client's path is only a hint: the file must
//! resolve, symlinks included, inside the scripts root, be a `.js` regular file, and is read
//! through the descriptor that was checked, capped at 256 KiB with a truncation marker.
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

const SCRIPT_BYTE_CAP: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    InvalidPath,
    RootUnavailable,
    NotFound,
    OutsideRoot,
    NotJs,
    NotRegularFile,
    ChangedDuringRead,
    ReadFailed,
}
impl Reason {
    fn tag(self) -> &'static str {
        match self {
            Self::InvalidPath => "invalid-path",
            Self::RootUnavailable => "root-unavailable",
            Self::NotFound => "not-found",
            Self::OutsideRoot => "outside-root",
            Self::NotJs => "not-js",
            Self::NotRegularFile => "not-regular-file",
            Self::ChangedDuringRead => "changed-during-read",
            Self::ReadFailed => "read-failed",
        }
    }
    fn message(self) -> &'static str {
        match self {
            Self::InvalidPath => "Workflow scripts must be absolute .js paths.",
            Self::RootUnavailable => "Script root unavailable.",
            Self::NotFound => "Script not found.",
            Self::OutsideRoot => "Script path is outside the workflow scripts root.",
            Self::NotJs => "Resolved script is not a .js file.",
            Self::NotRegularFile => "Script is not a regular file.",
            Self::ChangedDuringRead => "Script changed between resolution and open.",
            Self::ReadFailed => "Script read failed.",
        }
    }
}

/// `OrchestrationGetWorkflowScriptError`: the reason, and the path it concerns.
#[derive(Debug)]
pub struct ScriptError {
    pub reason: Reason,
    pub script_path: String,
}
impl ScriptError {
    pub fn wire(&self) -> Value {
        json!({"_tag":"OrchestrationGetWorkflowScriptError","reason":self.reason.tag(),"scriptPath":self.script_path,"message":self.reason.message()})
    }
}

/// Where the Claude harness keeps its workflow scripts: `<home>/.claude/projects`.
pub fn default_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(Path::new(&home).join(".claude").join("projects"))
}

fn is_js(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "js")
}

/// Reads `requested` if it is a script inside `root`.
pub fn read(root: Option<&Path>, requested: &str) -> Result<Value, ScriptError> {
    let fail = |reason: Reason, path: &str| ScriptError { reason, script_path: path.to_owned() };
    let requested_path = Path::new(requested);
    if !requested_path.is_absolute() || !is_js(requested_path) {
        return Err(fail(Reason::InvalidPath, requested));
    }
    let root = root.and_then(|root| std::fs::canonicalize(root).ok()).ok_or_else(|| fail(Reason::RootUnavailable, requested))?;
    // The file itself is resolved, not only its directory: a symlink named like a script inside a
    // contained directory must not escape.
    let resolved = std::fs::canonicalize(requested_path).map_err(|_| fail(Reason::NotFound, requested))?;
    let resolved_text = resolved.to_string_lossy().into_owned();
    if !resolved.starts_with(&root) {
        return Err(fail(Reason::OutsideRoot, &resolved_text));
    }
    if !is_js(&resolved) {
        return Err(fail(Reason::NotJs, &resolved_text));
    }
    // Open first, then check what was opened: re-checking the path after the open would race a swap.
    let mut file = std::fs::File::open(&resolved).map_err(|_| fail(Reason::ReadFailed, &resolved_text))?;
    let opened = file.metadata().map_err(|_| fail(Reason::ReadFailed, &resolved_text))?;
    if !opened.is_file() {
        return Err(fail(Reason::NotRegularFile, &resolved_text));
    }
    let on_path = std::fs::symlink_metadata(&resolved).map_err(|_| fail(Reason::ReadFailed, &resolved_text))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // The opened inode must be the one resolution found.
        if opened.ino() != on_path.ino() || opened.dev() != on_path.dev() {
            return Err(fail(Reason::ChangedDuringRead, &resolved_text));
        }
    }
    let _ = on_path;
    let mut bytes = Vec::new();
    file.by_ref().take(SCRIPT_BYTE_CAP).read_to_end(&mut bytes).map_err(|_| fail(Reason::ReadFailed, &resolved_text))?;
    Ok(json!({"scriptPath":resolved_text,"contents":String::from_utf8_lossy(&bytes),"truncated":opened.len() > SCRIPT_BYTE_CAP}))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn reason(result: Result<Value, ScriptError>) -> &'static str {
        result.unwrap_err().reason.tag()
    }

    /// The containment rules of the original, over a temporary scripts root.
    #[test]
    fn only_js_regular_files_inside_the_root_are_served() {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let root = base.join("projects");
        std::fs::create_dir_all(root.join("a")).unwrap();
        let script = root.join("a").join("flow.js");
        std::fs::write(&script, "export default 1;\n").unwrap();
        std::fs::write(root.join("a").join("notes.txt"), "no").unwrap();
        std::fs::write(base.join("outside.js"), "secret").unwrap();
        std::os::unix::fs::symlink(base.join("outside.js"), root.join("a").join("link.js")).unwrap();
        std::os::unix::fs::symlink(&script, root.join("alias.js")).unwrap();
        std::fs::create_dir_all(root.join("a").join("dir.js")).unwrap();
        let read_path = |path: &Path| read(Some(&root), path.to_str().unwrap());

        let served = read_path(&script).unwrap();
        assert_eq!(served, json!({"scriptPath":script.to_str().unwrap(),"contents":"export default 1;\n","truncated":false}));
        // A symlink inside the root to a file inside the root resolves to the real file.
        assert_eq!(read_path(&root.join("alias.js")).unwrap()["scriptPath"], script.to_str().unwrap());

        assert_eq!(reason(read(Some(&root), "relative/flow.js")), "invalid-path");
        assert_eq!(reason(read_path(&root.join("a").join("notes.txt"))), "invalid-path");
        assert_eq!(reason(read_path(&root.join("a").join("missing.js"))), "not-found");
        assert_eq!(reason(read_path(&base.join("outside.js"))), "outside-root");
        // A symlink named like a script cannot lead out of the root.
        let escaped = read_path(&root.join("a").join("link.js")).unwrap_err();
        assert_eq!((escaped.reason.tag(), escaped.script_path.as_str()), ("outside-root", base.join("outside.js").to_str().unwrap()));
        assert_eq!(reason(read_path(&root.join("a").join("dir.js"))), "not-regular-file");
        assert_eq!(reason(read(Some(&base.join("nowhere")), script.to_str().unwrap())), "root-unavailable");
        assert_eq!(reason(read(None, script.to_str().unwrap())), "root-unavailable");
    }

    #[test]
    fn a_long_script_is_cut_at_the_cap_and_marked() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let script = root.join("big.js");
        std::fs::write(&script, "x".repeat(SCRIPT_BYTE_CAP as usize + 10)).unwrap();
        let served = read(Some(&root), script.to_str().unwrap()).unwrap();
        assert_eq!((served["contents"].as_str().unwrap().len() as u64, served["truncated"].clone()), (SCRIPT_BYTE_CAP, json!(true)));
        let exact = root.join("exact.js");
        std::fs::write(&exact, "x".repeat(SCRIPT_BYTE_CAP as usize)).unwrap();
        assert_eq!(read(Some(&root), exact.to_str().unwrap()).unwrap()["truncated"], false);
    }

    #[test]
    fn errors_carry_the_originals_tag_reason_and_message() {
        let error = ScriptError { reason: Reason::OutsideRoot, script_path: "/x.js".into() };
        assert_eq!(error.wire(), json!({"_tag":"OrchestrationGetWorkflowScriptError","reason":"outside-root","scriptPath":"/x.js","message":"Script path is outside the workflow scripts root."}));
    }
}
