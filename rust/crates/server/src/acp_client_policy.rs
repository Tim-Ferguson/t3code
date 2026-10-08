//! Policy for operations mediated by ACP clients; provider-owned tools remain native.
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    Allow,
    Ask,
    Deny,
}
impl Disposition {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}
fn requires_approval(policy: &Value) -> bool {
    policy
        .get("approvalPolicy")
        .map(|value| value != "never")
        .unwrap_or(policy["runtimeMode"] == "approval-required")
}
fn mutation(kind: &str) -> bool {
    matches!(kind, "edit" | "delete" | "move")
}
fn resolve_path(value: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let value = t3_contracts::trim_wire_string(value);
    if value.is_empty() {
        return None;
    }
    let path = Path::new(value);
    if path.is_absolute() {
        Some(path.into())
    } else {
        cwd.map(|cwd| cwd.join(path))
    }
}
fn canonical_containment(path: &Path) -> Option<PathBuf> {
    // Resolve symlinks before lexical normalization. Missing descendants are
    // appended only after the deepest existing ancestor has been canonicalized.
    let mut candidate = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(&candidate) {
            Ok(mut existing) => {
                for part in missing.iter().rev() {
                    existing.push(part);
                }
                let mut normalized = PathBuf::new();
                for component in existing.components() {
                    match component {
                        Component::CurDir => {}
                        Component::ParentDir => {
                            normalized.pop();
                        }
                        component => normalized.push(component.as_os_str()),
                    }
                }
                return Some(normalized);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => return None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        let name = match candidate.components().next_back()? {
            Component::ParentDir => std::ffi::OsString::from(".."),
            Component::CurDir => std::ffi::OsString::from("."),
            Component::Normal(name) => name.to_os_string(),
            _ => return None,
        };
        if !candidate.pop() {
            return None;
        }
        missing.push(name);
    }
}
fn workspace_mutation(policy: &Value, sandbox: &Value, locations: &Value) -> bool {
    let cwd = policy["cwd"]
        .as_str()
        .and_then(|value| resolve_path(value, std::env::current_dir().ok().as_deref()));
    let mut roots = Vec::new();
    if let Some(root) = cwd.as_ref().and_then(|cwd| canonical_containment(cwd)) {
        roots.push(root);
    }
    for root in sandbox["writableRoots"].as_array().into_iter().flatten() {
        if let Some(root) = root
            .as_str()
            .and_then(|root| resolve_path(root, cwd.as_deref()))
            .and_then(|root| canonical_containment(&root))
        {
            roots.push(root);
        }
    }
    let Some(locations) = locations
        .as_array()
        .filter(|locations| !locations.is_empty())
    else {
        return false;
    };
    if roots.is_empty() {
        return false;
    }
    locations.iter().all(|location| {
        location["path"]
            .as_str()
            .and_then(|path| resolve_path(path, cwd.as_deref()))
            .and_then(|path| canonical_containment(&path))
            .is_some_and(|path| roots.iter().any(|root| path.starts_with(root)))
    })
}
pub(crate) fn operation(policy: &Value, kind: Option<&str>, locations: &Value) -> Disposition {
    let kind = kind.unwrap_or("other");
    let sandbox = &policy["sandboxPolicy"];
    let sandbox_type = match sandbox.as_object().and_then(|sandbox| sandbox.get("type")) {
        None => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => Some("\0unknown sandbox"),
    };
    if matches!(kind, "read" | "search" | "think") {
        return if matches!(
            sandbox_type,
            None | Some("readOnly" | "workspaceWrite" | "dangerFullAccess" | "externalSandbox")
        ) {
            Disposition::Allow
        } else {
            Disposition::Deny
        };
    }
    if requires_approval(policy) {
        return Disposition::Ask;
    }
    match sandbox_type {
        Some("readOnly") => Disposition::Deny,
        Some("workspaceWrite") => {
            if mutation(kind) && workspace_mutation(policy, sandbox, locations) {
                Disposition::Allow
            } else {
                Disposition::Deny
            }
        }
        Some("dangerFullAccess" | "externalSandbox") => Disposition::Allow,
        None => {
            if policy["runtimeMode"] == "approval-required" {
                Disposition::Deny
            } else if policy["runtimeMode"] == "auto-accept-edits"
                && policy.get("approvalPolicy").is_none()
                && !mutation(kind)
            {
                Disposition::Ask
            } else {
                Disposition::Allow
            }
        }
        _ => Disposition::Deny,
    }
}
pub(crate) fn permission(policy: &Value, request: &Value) -> Disposition {
    operation(
        policy,
        request["toolCall"]["kind"].as_str(),
        &request["toolCall"]["locations"],
    )
}
pub(crate) fn execute(policy: &Value) -> Disposition {
    operation(policy, Some("execute"), &Value::Null)
}
pub(crate) fn mcp_approval(
    policy: &Value,
    request: &Value,
    native_id: Option<&str>,
) -> Option<Disposition> {
    if request["mode"] != "form"
        || (request["_meta"]["codex_approval_kind"] != "mcp_tool_call"
            && !native_id.is_some_and(|id| id.starts_with("mcp_tool_call_approval_")))
    {
        return None;
    }
    Some(if requires_approval(policy) {
        Disposition::Ask
    } else {
        Disposition::Allow
    })
}
#[derive(Default)]
pub(crate) struct Grants {
    session_execute: bool,
    turn_execute: Option<String>,
}
impl Grants {
    pub(crate) fn record(&mut self, kind: &str, scope: &str, turn_key: &str) {
        if kind != "command" {
            return;
        }
        if scope == "session" {
            self.session_execute = true;
        } else {
            self.turn_execute = Some(turn_key.to_owned());
        }
    }
    pub(crate) fn allows_execute(&self, turn_key: Option<&str>) -> bool {
        self.session_execute
            || turn_key.is_some_and(|key| self.turn_execute.as_deref() == Some(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn paths(value: &mut Value, root: &str) {
        match value {
            Value::String(value) => *value = value.replace("$ROOT", root),
            Value::Array(values) => {
                for value in values {
                    paths(value, root);
                }
            }
            Value::Object(values) => {
                for value in values.values_mut() {
                    paths(value, root);
                }
            }
            _ => {}
        }
    }
    #[cfg(unix)]
    #[test]
    fn source_permission_execute_mcp_and_grants_cover_symlink_containment_and_optional_values() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        for name in ["workspace", "extra", "outside"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        std::fs::write(root.join("workspace/file"), "inside").unwrap();
        std::fs::write(root.join("outside/file"), "outside").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("workspace/escape")).unwrap();
        std::os::unix::fs::symlink(root.join("outside/missing"), root.join("workspace/broken"))
            .unwrap();
        for (index, line) in include_str!("../tests/fixtures/acp-client-policy.jsonl")
            .lines()
            .enumerate()
        {
            let mut fixture: Value = serde_json::from_str(line).unwrap();
            paths(&mut fixture["input"], root.to_str().unwrap());
            let input = &fixture["input"];
            let actual = match fixture["operation"].as_str().unwrap() {
                "permission" => json!(permission(&input["policy"], &input["request"]).as_str()),
                "execute" => json!(execute(input).as_str()),
                "mcp" => json!(
                    mcp_approval(
                        &input["policy"],
                        &input["request"],
                        input["nativeId"].as_str()
                    )
                    .map(Disposition::as_str)
                ),
                "grants" => {
                    let mut grants = Grants::default();
                    json!(
                        input
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|row| {
                                if row.get("query").is_some() {
                                    json!(grants.allows_execute(row["query"].as_str()))
                                } else {
                                    grants.record(
                                        row["kind"].as_str().unwrap(),
                                        row["scope"].as_str().unwrap(),
                                        row["turnKey"].as_str().unwrap(),
                                    );
                                    Value::Null
                                }
                            })
                            .collect::<Vec<_>>()
                    )
                }
                operation => panic!("Unknown ACP client policy fixture operation {operation}"),
            };
            assert_eq!(
                actual, fixture["output"],
                "source ACP client policy fixture {index}: {input}"
            );
        }
    }
}
