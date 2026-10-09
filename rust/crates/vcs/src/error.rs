//! VCS error shapes. Each variant mirrors one wire error of `packages/contracts/src/vcs.ts`;
//! `VcsError::tag` returns the original `_tag` so a caller can map back to the wire union.
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VcsDriverKind {
    Git,
    Jj,
    Unknown,
}

impl VcsDriverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Jj => "jj",
            Self::Unknown => "unknown",
        }
    }
}

impl VcsDriverKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "git" => Some(Self::Git),
            "jj" => Some(Self::Jj),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

impl std::fmt::Display for VcsDriverKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What every process failure reports about the invocation. Arguments are deliberately
/// reduced to a count so secrets in them never reach messages or logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessContext {
    pub operation: String,
    pub command: String,
    pub cwd: String,
    pub argument_count: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitFailureKind {
    Authentication,
    NotFound,
    RateLimited,
    CommandFailed,
}

impl ExitFailureKind {
    /// The wire value (`failureKind`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::NotFound => "not-found",
            Self::RateLimited => "rate-limited",
            Self::CommandFailed => "command-failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// `exitCode` is a stream only for `VcsProcessOutputReadError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadStream {
    Stdout,
    Stderr,
    ExitCode,
}

impl ReadStream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::ExitCode => "exitCode",
        }
    }
}

impl From<OutputStream> for ReadStream {
    fn from(stream: OutputStream) -> Self {
        match stream {
            OutputStream::Stdout => Self::Stdout,
            OutputStream::Stderr => Self::Stderr,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum VcsError {
    #[error("VCS process failed to spawn in {}: {} ({})", .context.operation, .context.command, .context.cwd)]
    ProcessSpawn {
        context: ProcessContext,
        #[source]
        cause: Arc<std::io::Error>,
    },
    #[error("VCS process failed in {}: {} ({}) exited with {exit_code} - {detail}", .context.operation, .context.command, .context.cwd)]
    ProcessExit {
        context: ProcessContext,
        exit_code: i32,
        detail: String,
        failure_kind: Option<ExitFailureKind>,
        /// Process-boundary hint for a recognized transient failure; absence is not retryable.
        retryable: bool,
        /// UTF-16 length of stderr, as JavaScript would report it. Stderr itself is dropped.
        stderr_length: Option<usize>,
        stderr_truncated: Option<bool>,
    },
    #[error("VCS process timed out in {}: {} ({}) after {timeout_ms}ms", .context.operation, .context.command, .context.cwd)]
    ProcessTimeout {
        context: ProcessContext,
        timeout_ms: u64,
    },
    #[error("VCS process failed to write {stdin_bytes} bytes to stdin in {}: {} ({})", .context.operation, .context.command, .context.cwd)]
    ProcessStdinWrite {
        context: ProcessContext,
        stdin_bytes: usize,
        #[source]
        cause: Arc<std::io::Error>,
    },
    #[error("VCS process failed to read {} in {}: {} ({})", .stream.as_str(), .context.operation, .context.command, .context.cwd)]
    ProcessOutputRead {
        context: ProcessContext,
        stream: ReadStream,
        #[source]
        cause: Arc<std::io::Error>,
    },
    #[error("VCS process {} produced {observed_bytes} bytes in {}: {} ({}), exceeding the {max_bytes} byte limit", .stream.as_str(), .context.operation, .context.command, .context.cwd)]
    ProcessOutputLimit {
        context: ProcessContext,
        stream: OutputStream,
        max_bytes: usize,
        observed_bytes: usize,
    },
    #[error("VCS process completed without an exit code in {}: {} ({})", .context.operation, .context.command, .context.cwd)]
    ProcessMissingExitCode { context: ProcessContext },
    #[error("VCS repository detection failed in {operation}: {cwd} - {detail}")]
    RepositoryDetection {
        operation: String,
        cwd: String,
        detail: String,
    },
    #[error("VCS operation is unsupported for {kind} in {operation}: {detail}")]
    UnsupportedOperation {
        operation: String,
        kind: VcsDriverKind,
        detail: String,
    },
}

impl VcsError {
    /// The `_tag` of the matching wire error.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::ProcessSpawn { .. } => "VcsProcessSpawnError",
            Self::ProcessExit { .. } => "VcsProcessExitError",
            Self::ProcessTimeout { .. } => "VcsProcessTimeoutError",
            Self::ProcessStdinWrite { .. } => "VcsProcessStdinWriteError",
            Self::ProcessOutputRead { .. } => "VcsProcessOutputReadError",
            Self::ProcessOutputLimit { .. } => "VcsProcessOutputLimitError",
            Self::ProcessMissingExitCode { .. } => "VcsProcessMissingExitCodeError",
            Self::RepositoryDetection { .. } => "VcsRepositoryDetectionError",
            Self::UnsupportedOperation { .. } => "VcsUnsupportedOperationError",
        }
    }

    /// A `VcsProcessExitError` raised by the driver itself rather than by a process exit
    /// (empty `write-tree` output, a refused sparse rebuild, a failed `git diff`).
    pub fn exit(
        operation: &str,
        command: &str,
        cwd: &str,
        exit_code: i32,
        detail: impl Into<String>,
    ) -> Self {
        Self::ProcessExit {
            context: ProcessContext {
                operation: operation.into(),
                command: command.into(),
                cwd: cwd.into(),
                argument_count: None,
            },
            exit_code,
            detail: detail.into(),
            failure_kind: None,
            retryable: false,
            stderr_length: None,
            stderr_truncated: None,
        }
    }

    pub fn is_retryable_exit(&self) -> bool {
        matches!(
            self,
            Self::ProcessExit {
                retryable: true,
                ..
            }
        )
    }
}
