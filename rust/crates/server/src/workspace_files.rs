//! Workspace text previews and writes, including explicitly requested host files.
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};
use t3_contracts::{ProjectReadFileInput, ProjectWriteFileInput};

const READ_BUDGET: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct WorkspaceFileError {
    pub message: String,
    context: Value,
    cause: Value,
}
impl WorkspaceFileError {
    pub fn rpc_error(&self, write: bool, cwd: &str, relative: &str) -> Value {
        let mut value = self.context.clone();
        value["_tag"] = json!(if write {
            "ProjectWriteFileError"
        } else {
            "ProjectReadFileError"
        });
        value["cwd"] = json!(cwd);
        value["relativePath"] = json!(relative);
        value["message"] = json!(self.message);
        value["cause"] = self.cause.clone();
        value
    }
}
fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
fn rejection(
    cwd: &str,
    relative: &str,
    failure: &str,
    tag: &str,
    message: String,
    fields: Value,
) -> WorkspaceFileError {
    let mut context = fields.clone();
    context["failure"] = json!(failure);
    let mut cause = fields;
    cause["_tag"] = json!(tag);
    cause["workspaceRoot"] = json!(cwd);
    cause["relativePath"] = json!(relative);
    cause["message"] = json!(message);
    WorkspaceFileError {
        message,
        context,
        cause,
    }
}
fn operation(
    cwd: &str,
    relative: &str,
    target: &Path,
    at: &Path,
    operation: &str,
    error: io::Error,
) -> WorkspaceFileError {
    let code = match error.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST",
        io::ErrorKind::NotADirectory => "ENOTDIR",
        io::ErrorKind::IsADirectory => "EISDIR",
        io::ErrorKind::BrokenPipe => "EPIPE",
        io::ErrorKind::InvalidInput => "EINVAL",
        io::ErrorKind::Interrupted => "EINTR",
        _ => "EIO",
    };
    let target = path_string(target);
    let at = path_string(at);
    rejection(
        cwd,
        relative,
        "operation_failed",
        "WorkspaceFileSystemOperationError",
        format!(
            "Workspace file operation '{operation}' failed at '{at}' for resolved path '{target}' (requested as '{relative}' in '{cwd}')."
        ),
        json!({"resolvedPath":target,"operationPath":at,"operation":operation,"cause":{"name":"Error","message":error.to_string(),"code":code,"errno":error.raw_os_error()}}),
    )
}
/// Lexical normalization follows path.resolve, without following symlinks.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}
pub(crate) fn relative_target(
    cwd: &str,
    relative: &str,
) -> Result<(PathBuf, String), WorkspaceFileError> {
    let relative = t3_contracts::trim_wire_string(relative);
    let root = normalize(Path::new(cwd));
    let target = normalize(&root.join(relative));
    if Path::new(relative).is_absolute() || target == root || !target.starts_with(&root) {
        return Err(rejection(
            cwd,
            relative,
            "workspace_path_outside_root",
            "WorkspacePathOutsideRootError",
            format!("Workspace file path must be relative to the project root: {relative}"),
            json!({}),
        ));
    }
    let result = target
        .strip_prefix(&root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    // WorkspacePaths converts the display path to POSIX separators before its
    // traversal checks, even on Unix where a backslash is a filename character.
    if result.is_empty()
        || result == "."
        || result == ".."
        || result.starts_with("../")
        || Path::new(&result).is_absolute()
    {
        return Err(rejection(
            cwd,
            relative,
            "workspace_path_outside_root",
            "WorkspacePathOutsideRootError",
            format!("Workspace file path must be relative to the project root: {relative}"),
            json!({}),
        ));
    }
    Ok((target, result))
}
pub fn read_file(input: &ProjectReadFileInput) -> Result<Value, WorkspaceFileError> {
    let cwd = input.cwd.as_str();
    let relative = input.relative_path.0.as_str();
    let requested = Path::new(relative);
    let (target, display) = if requested.is_absolute() {
        (requested.to_path_buf(), relative.to_owned())
    } else {
        relative_target(cwd, relative)?
    };
    let root = if requested.is_absolute() {
        None
    } else {
        Some(fs::canonicalize(cwd).map_err(|error| {
            operation(
                cwd,
                relative,
                &target,
                Path::new(cwd),
                "realpath-workspace-root",
                error,
            )
        })?)
    };
    let real = fs::canonicalize(&target)
        .map_err(|error| operation(cwd, relative, &target, &target, "realpath-target", error))?;
    if let Some(root) = root {
        if !real.starts_with(&root) {
            return Err(rejection(
                cwd,
                relative,
                "resolved_path_outside_root",
                "WorkspaceFilePathEscapeError",
                format!(
                    "Workspace file '{relative}' resolves outside workspace root '{cwd}': {}",
                    real.display()
                ),
                json!({"resolvedPath":path_string(&real),"resolvedWorkspaceRoot":path_string(&root)}),
            ));
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = options
        .open(&real)
        .map_err(|error| operation(cwd, relative, &real, &real, "open", error))?;
    let result = (|| {
        let stat = file
            .metadata()
            .map_err(|error| operation(cwd, relative, &real, &real, "stat", error))?;
        if !stat.is_file() {
            return Err(rejection(
                cwd,
                relative,
                "path_not_file",
                "WorkspacePathNotFileError",
                format!(
                    "Workspace path '{relative}' in '{cwd}' is not a file: {}",
                    real.display()
                ),
                json!({"resolvedPath":path_string(&real)}),
            ));
        }
        let mut bytes = Vec::with_capacity(stat.len().min(READ_BUDGET) as usize);
        Read::by_ref(&mut file)
            .take(stat.len().min(READ_BUDGET))
            .read_to_end(&mut bytes)
            .map_err(|error| operation(cwd, relative, &real, &real, "read", error))?;
        if bytes.contains(&0) {
            return Err(rejection(
                cwd,
                relative,
                "binary_file",
                "WorkspaceBinaryFileError",
                format!(
                    "Workspace file '{relative}' in '{cwd}' is binary and cannot be previewed as text."
                ),
                json!({"resolvedPath":path_string(&real)}),
            ));
        }
        let contents = String::from_utf8_lossy(&bytes);
        // WHATWG TextDecoder strips a UTF-8 BOM at the start of the decoded file.
        Ok(
            json!({"relativePath":display,"contents":contents.strip_prefix('\u{feff}').unwrap_or(&contents),"byteLength":stat.len(),"truncated":stat.len()>READ_BUDGET}),
        )
    })();
    #[cfg(unix)]
    {
        use std::os::fd::IntoRawFd;
        // Consume ownership once; unlike Drop, this exposes a close error through
        // the same operation context as the source acquire/use/release service.
        let fd = file.into_raw_fd();
        if unsafe { libc::close(fd) } != 0 {
            return Err(operation(
                cwd,
                relative,
                &real,
                &real,
                "close",
                io::Error::last_os_error(),
            ));
        }
    }
    result
}
pub fn write_file(input: &ProjectWriteFileInput) -> Result<Value, WorkspaceFileError> {
    let cwd = input.cwd.as_str();
    let relative = input.relative_path.0.as_str();
    let (target, display) = relative_target(cwd, relative)?;
    let parent = target.parent().expect("target has workspace parent");
    fs::create_dir_all(parent)
        .map_err(|error| operation(cwd, relative, &target, parent, "make-directory", error))?;
    fs::write(&target, &input.contents)
        .map_err(|error| operation(cwd, relative, &target, &target, "write-file", error))?;
    Ok(json!({"relativePath":display}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn read(cwd: &Path, relative: &str) -> Result<Value, WorkspaceFileError> {
        read_file(&serde_json::from_value(json!({"cwd":cwd,"relativePath":relative})).unwrap())
    }
    fn write(cwd: &Path, relative: &str, contents: &str) -> Result<Value, WorkspaceFileError> {
        write_file(
            &serde_json::from_value(json!({"cwd":cwd,"relativePath":relative,"contents":contents}))
                .unwrap(),
        )
    }
    #[test]
    fn relative_and_explicit_host_file_reads_preserve_wire_paths_and_io_context() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(
            directory.path(),
            "src/../src/index.rs",
            "export const answer = 42;\n",
        )
        .unwrap();
        let result = read(directory.path(), "src/index.rs").unwrap();
        assert_eq!(result["byteLength"], 26);
        assert_eq!(result["contents"], "export const answer = 42;\n");
        assert_eq!(result["truncated"], false);
        serde_json::from_value::<t3_contracts::ProjectReadFileResult>(result).unwrap();
        fs::write(outside.path().join("report.md"), "# Report\n").unwrap();
        let path = path_string(&outside.path().join("report.md"));
        assert_eq!(read(directory.path(), &path).unwrap()["relativePath"], path);
        for path in [
            "../escape.md",
            ".",
            "/absolute.md",
            "..\\escape",
            "\\absolute",
        ] {
            assert_eq!(
                write(directory.path(), path, "do not write")
                    .unwrap_err()
                    .context["failure"],
                "workspace_path_outside_root"
            );
        }
        for path in ["..\\escape", "\\absolute"] {
            assert_eq!(
                read(directory.path(), path).unwrap_err().context["failure"],
                "workspace_path_outside_root"
            );
        }
        assert_eq!(
            read(directory.path(), "../escape.md").unwrap_err().context["failure"],
            "workspace_path_outside_root"
        );
        let error = read(directory.path(), "missing.md").unwrap_err();
        let wire = error.rpc_error(false, &path_string(directory.path()), "missing.md");
        assert_eq!(wire["operation"], "realpath-target");
        assert!(wire["cause"]["cause"]["message"].is_string());
        serde_json::from_value::<t3_contracts::ProjectReadFileError>(wire).unwrap();
    }
    #[test]
    fn previews_reject_binary_directories_and_limit_bytes_with_textdecoder_replacement() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("binary"), b"secret\0content").unwrap();
        let error = read(directory.path(), "binary").unwrap_err();
        assert_eq!(error.context["failure"], "binary_file");
        assert!(
            !error
                .rpc_error(false, "root", "binary")
                .to_string()
                .contains("secret")
        );
        fs::create_dir(directory.path().join("folder")).unwrap();
        assert_eq!(
            read(directory.path(), "folder").unwrap_err().context["failure"],
            "path_not_file"
        );
        let mut bytes = vec![b'a'; READ_BUDGET as usize - 1];
        bytes.extend_from_slice("😀".as_bytes());
        fs::write(directory.path().join("large"), bytes).unwrap();
        let result = read(directory.path(), "large").unwrap();
        assert_eq!(result["byteLength"], READ_BUDGET + 3);
        assert_eq!(result["truncated"], true);
        assert!(result["contents"].as_str().unwrap().ends_with('\u{fffd}'));
        fs::write(directory.path().join("bom"), b"\xef\xbb\xbftext").unwrap();
        assert_eq!(read(directory.path(), "bom").unwrap()["contents"], "text");
    }
    #[cfg(unix)]
    #[test]
    fn relative_symlink_escape_is_rejected_and_fifo_open_cannot_block() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "outside").unwrap();
        symlink(outside.path(), directory.path().join("link")).unwrap();
        assert_eq!(
            read(directory.path(), "link/secret").unwrap_err().context["failure"],
            "resolved_path_outside_root"
        );
        let fifo = directory.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            read(directory.path(), "pipe").unwrap_err().context["failure"],
            "path_not_file"
        );
        let absolute = path_string(&directory.path().join("link/secret"));
        assert_eq!(
            read(directory.path(), &absolute).unwrap()["contents"],
            "outside"
        );
    }
}
