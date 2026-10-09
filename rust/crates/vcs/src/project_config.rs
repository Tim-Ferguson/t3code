//! Per-project VCS selection from `.t3code/vcs.json` (port of `vcs/VcsProjectConfig.ts`).
use std::path::{Path, PathBuf};

use regex::{Captures, Regex};
use serde_json::Value;

use crate::error::VcsDriverKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOperation {
    Inspect,
    Read,
    Decode,
}

impl ConfigOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Read => "read",
            Self::Decode => "decode",
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("Failed to {} VCS project config at {}.", .operation.as_str(), .config_path.display())]
pub struct VcsProjectConfigError {
    pub operation: ConfigOperation,
    pub cwd: PathBuf,
    pub config_path: PathBuf,
    pub cause: String,
}

/// The kind to use for `cwd`: an explicit request wins, then the nearest `.t3code/vcs.json`
/// (`{"vcs":{"kind":..}}` or `{"vcsKind":..}`). `None` means "auto": detect from the
/// directory. Unreadable or invalid config is logged and treated as absent.
pub fn resolve_kind(cwd: &Path, requested_kind: Option<VcsDriverKind>) -> Option<VcsDriverKind> {
    if requested_kind.is_some() {
        return requested_kind;
    }
    let (config_path, inspect_errors) = find_config_path(cwd);
    for error in &inspect_errors {
        log_config_error(error);
    }
    let config_path = config_path?;
    match read_configured_kind(cwd, &config_path) {
        Ok(kind) => kind,
        Err(error) => {
            log_config_error(&error);
            None
        }
    }
}

fn log_config_error(error: &VcsProjectConfigError) {
    eprintln!(
        "WARN {error} operation={} cwd={} configPath={}",
        error.operation.as_str(),
        error.cwd.display(),
        error.config_path.display()
    );
}

/// Nearest `.t3code/vcs.json` at or above `cwd`, plus the candidates that could not be
/// inspected (which are skipped, not fatal).
pub fn find_config_path(cwd: &Path) -> (Option<PathBuf>, Vec<VcsProjectConfigError>) {
    let mut errors = Vec::new();
    let mut current = cwd;
    loop {
        let candidate = current.join(".t3code").join("vcs.json");
        match candidate.try_exists() {
            Ok(true) => return (Some(candidate), errors),
            Ok(false) => {}
            Err(cause) => errors.push(VcsProjectConfigError {
                operation: ConfigOperation::Inspect,
                cwd: cwd.to_path_buf(),
                config_path: candidate,
                cause: cause.to_string(),
            }),
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return (None, errors),
        }
    }
}

pub fn read_configured_kind(
    cwd: &Path,
    config_path: &Path,
) -> Result<Option<VcsDriverKind>, VcsProjectConfigError> {
    let fail = |operation, cause: String| VcsProjectConfigError {
        operation,
        cwd: cwd.to_path_buf(),
        config_path: config_path.to_path_buf(),
        cause,
    };
    let bytes = std::fs::read(config_path)
        .map_err(|cause| fail(ConfigOperation::Read, cause.to_string()))?;
    // Decoded like a text decoder would: lossy UTF-8, byte order mark dropped.
    let raw = String::from_utf8_lossy(&bytes);
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let value: Value = serde_json::from_str(&strip_lenient_json(raw))
        .map_err(|cause| fail(ConfigOperation::Decode, cause.to_string()))?;
    decode_configured_kind(&value).map_err(|cause| fail(ConfigOperation::Decode, cause))
}

/// Validates the config's shape the way its schema does: unknown keys are ignored, a present
/// but invalid key rejects the whole file.
fn decode_configured_kind(value: &Value) -> Result<Option<VcsDriverKind>, String> {
    let object = value.as_object().ok_or("Expected an object")?;
    let kind_of = |value: &Value, key: &str| -> Result<VcsDriverKind, String> {
        value
            .as_str()
            .and_then(VcsDriverKind::parse)
            .ok_or_else(|| format!("Expected \"git\" | \"jj\" | \"unknown\" at {key}"))
    };
    let mut nested_kind = None;
    if let Some(vcs) = object.get("vcs") {
        let vcs = vcs.as_object().ok_or("Expected an object at vcs")?;
        if let Some(kind) = vcs.get("kind") {
            nested_kind = Some(kind_of(kind, "vcs.kind")?);
        }
    }
    let flat_kind = object
        .get("vcsKind")
        .map(|kind| kind_of(kind, "vcsKind"))
        .transpose()?;
    Ok(nested_kind.or(flat_kind))
}

/// Strips `//` and `/* */` comments and trailing commas, leaving string literals alone: the
/// same three passes as the original's lenient JSON getter.
pub fn strip_lenient_json(input: &str) -> String {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<[Regex; 3]> = OnceLock::new();
    let [line_comments, block_comments, trailing_commas] = PATTERNS.get_or_init(|| {
        [
            Regex::new(r#"("(?:[^"\\]|\\.)*")|//[^\n]*"#).unwrap(),
            Regex::new(r#"(?s)("(?:[^"\\]|\\.)*")|/\*.*?\*/"#).unwrap(),
            Regex::new(r#"("(?:[^"\\]|\\.)*")|,(\s*[}\]])"#).unwrap(),
        ]
    });
    let keep_string = |captures: &Captures| captures.get(1).map_or("", |m| m.as_str()).to_string();
    let stripped = line_comments.replace_all(input, keep_string);
    let stripped = block_comments.replace_all(&stripped, keep_string);
    trailing_commas
        .replace_all(&stripped, |captures: &Captures| {
            captures
                .get(1)
                .or_else(|| captures.get(2))
                .map_or("", |m| m.as_str())
                .to_string()
        })
        .into_owned()
}
