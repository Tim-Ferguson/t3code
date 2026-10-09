//! The failures of the `vcs.*` methods and the original `_tag` envelope of each: `GitCommandError`
//! and `VcsError` as `t3_vcs` raises them, plus the `GitManagerError` the status workflow wraps
//! around a failed repository detection. Handlers map every failure through `wire`.
use serde_json::{Map, Value, json};
use t3_vcs::{
    GitCommandError, VcsError,
    error::{ProcessContext, ReadStream},
};

/// `Schema.Defect` encodes an `Error` as its name and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cause {
    pub name: String,
    pub message: String,
}

impl Cause {
    fn of(error: &VcsError) -> Self {
        Self {
            name: error.tag().into(),
            message: error.to_string(),
        }
    }
    fn wire(&self) -> Value {
        json!({"name":self.name,"message":self.message})
    }
}

#[derive(Debug, Clone)]
pub enum VcsRpcError {
    /// `GitCommandError`; `cause` is set when the workflow wrapped a registry failure (the
    /// `t3_vcs` serializer leaves out the OS-level cause of its own errors).
    Command {
        error: GitCommandError,
        cause: Option<Cause>,
    },
    /// `GitManagerError`.
    Manager {
        operation: String,
        cwd: String,
        detail: String,
        cause: Option<Cause>,
    },
    Vcs(VcsError),
}

impl From<GitCommandError> for VcsRpcError {
    fn from(error: GitCommandError) -> Self {
        Self::Command { error, cause: None }
    }
}
impl From<VcsError> for VcsRpcError {
    fn from(error: VcsError) -> Self {
        Self::Vcs(error)
    }
}

impl std::fmt::Display for VcsRpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Command { error, .. } => write!(formatter, "{error}"),
            Self::Manager {
                operation, detail, ..
            } => write!(formatter, "Git manager failed in {operation}: {detail}"),
            Self::Vcs(error) => write!(formatter, "{error}"),
        }
    }
}

impl VcsRpcError {
    /// The `GitCommandError` a command routed through the registry reports when the registry
    /// failed or found another kind of repository (`command: "vcs-route"`).
    pub fn route(
        operation: &str,
        cwd: &str,
        detail: impl Into<String>,
        cause: Option<&VcsError>,
    ) -> Self {
        Self::Command {
            error: GitCommandError {
                operation: operation.into(),
                command: "vcs-route".into(),
                cwd: cwd.into(),
                argument_count: None,
                exit_code: None,
                stdout_length: None,
                stderr_length: None,
                output_length: None,
                reason: None,
                detail: detail.into(),
                cause: None,
            },
            cause: cause.map(Cause::of),
        }
    }

    pub fn manager(
        operation: &str,
        cwd: &str,
        detail: impl Into<String>,
        cause: Option<&VcsError>,
    ) -> Self {
        Self::Manager {
            operation: operation.into(),
            cwd: cwd.into(),
            detail: detail.into(),
            cause: cause.map(Cause::of),
        }
    }

    /// The `_tag` of the wire error.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Command { .. } => "GitCommandError",
            Self::Manager { .. } => "GitManagerError",
            Self::Vcs(error) => error.tag(),
        }
    }

    /// The failing operation, where the error names one.
    pub fn operation(&self) -> Option<&str> {
        match self {
            Self::Command { error, .. } => Some(&error.operation),
            Self::Manager { operation, .. } => Some(operation),
            Self::Vcs(VcsError::ProcessSpawn { context, .. })
            | Self::Vcs(VcsError::ProcessExit { context, .. })
            | Self::Vcs(VcsError::ProcessTimeout { context, .. })
            | Self::Vcs(VcsError::ProcessStdinWrite { context, .. })
            | Self::Vcs(VcsError::ProcessOutputRead { context, .. })
            | Self::Vcs(VcsError::ProcessOutputLimit { context, .. })
            | Self::Vcs(VcsError::ProcessMissingExitCode { context }) => Some(&context.operation),
            Self::Vcs(
                VcsError::RepositoryDetection { operation, .. }
                | VcsError::UnsupportedOperation { operation, .. },
            ) => Some(operation),
        }
    }

    pub fn wire(&self) -> Value {
        match self {
            Self::Command { error, cause } => {
                let mut value = serde_json::to_value(error).expect("serializable command error");
                if let Some(cause) = cause {
                    value["cause"] = cause.wire();
                }
                value
            }
            Self::Manager {
                operation,
                cwd,
                detail,
                cause,
            } => {
                let mut value = json!({"_tag":"GitManagerError","operation":operation,"cwd":cwd,"detail":detail});
                if let Some(cause) = cause {
                    value["cause"] = cause.wire();
                }
                value
            }
            Self::Vcs(error) => vcs_error_wire(error),
        }
    }
}

fn context_fields(map: &mut Map<String, Value>, context: &ProcessContext) {
    map.insert("operation".into(), context.operation.clone().into());
    map.insert("command".into(), context.command.clone().into());
    map.insert("cwd".into(), context.cwd.clone().into());
    if let Some(count) = context.argument_count {
        map.insert("argumentCount".into(), count.into());
    }
}

/// An `Error` whose cause is an OS-level failure: `{ name, message }`.
fn io_cause(error: &std::io::Error) -> Value {
    json!({"name":"Error","message":error.to_string()})
}

/// `VcsError` as the contracts' `VcsError` union encodes it.
pub fn vcs_error_wire(error: &VcsError) -> Value {
    let mut map = Map::new();
    map.insert("_tag".into(), error.tag().into());
    match error {
        VcsError::ProcessSpawn { context, cause } => {
            context_fields(&mut map, context);
            map.insert("cause".into(), io_cause(cause));
        }
        VcsError::ProcessExit {
            context,
            exit_code,
            detail,
            failure_kind,
            retryable,
            stderr_length,
            stderr_truncated,
        } => {
            context_fields(&mut map, context);
            map.insert("exitCode".into(), (*exit_code).into());
            map.insert("detail".into(), detail.clone().into());
            if let Some(kind) = failure_kind {
                map.insert("failureKind".into(), kind.as_str().into());
            }
            if *retryable {
                map.insert("retryable".into(), true.into());
            }
            if let Some(length) = stderr_length {
                map.insert("stderrLength".into(), (*length).into());
            }
            if let Some(truncated) = stderr_truncated {
                map.insert("stderrTruncated".into(), (*truncated).into());
            }
        }
        VcsError::ProcessTimeout {
            context,
            timeout_ms,
        } => {
            context_fields(&mut map, context);
            map.insert("timeoutMs".into(), (*timeout_ms).into());
        }
        VcsError::ProcessStdinWrite {
            context,
            stdin_bytes,
            cause,
        } => {
            context_fields(&mut map, context);
            map.insert("stdinBytes".into(), (*stdin_bytes).into());
            map.insert("cause".into(), io_cause(cause));
        }
        VcsError::ProcessOutputRead {
            context,
            stream,
            cause,
        } => {
            context_fields(&mut map, context);
            map.insert("stream".into(), ReadStream::as_str(*stream).into());
            map.insert("cause".into(), io_cause(cause));
        }
        VcsError::ProcessOutputLimit {
            context,
            stream,
            max_bytes,
            observed_bytes,
        } => {
            context_fields(&mut map, context);
            map.insert("stream".into(), stream.as_str().into());
            map.insert("maxBytes".into(), (*max_bytes).into());
            map.insert("observedBytes".into(), (*observed_bytes).into());
        }
        VcsError::ProcessMissingExitCode { context } => context_fields(&mut map, context),
        VcsError::RepositoryDetection {
            operation,
            cwd,
            detail,
        } => {
            map.insert("operation".into(), operation.clone().into());
            map.insert("cwd".into(), cwd.clone().into());
            map.insert("detail".into(), detail.clone().into());
        }
        VcsError::UnsupportedOperation {
            operation,
            kind,
            detail,
        } => {
            map.insert("operation".into(), operation.clone().into());
            map.insert("kind".into(), kind.as_str().into());
            map.insert("detail".into(), detail.clone().into());
        }
    }
    Value::Object(map)
}
