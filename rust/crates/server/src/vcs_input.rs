//! Decoding and validation of the `vcs.*` payloads (`packages/contracts/src/git.ts`). The
//! schemas trim every string, reject blank ones, cap `query` at 256 characters and `limit` at
//! 200, and read `null` for an optional key as absent. `t3_vcs` leaves all of that to this
//! layer. A failure carries the original's message, first failing key in declaration order.
use serde::Serialize;
use serde_json::{Map, Value};
use t3_vcs::{VcsDriverKind, wire::RefKind};

const LIST_REFS_QUERY_MAX_LENGTH: usize = 256;
const LIST_REFS_MAX_LIMIT: u64 = 200;

/// `String.prototype.trim`: white space and line terminators, with U+FEFF and without U+0085.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

fn at(message: &str, key: &str) -> String {
    format!("{message}\n  at [{}]", Value::String(key.into()))
}

struct Fields<'a>(&'a Map<String, Value>);

impl<'a> Fields<'a> {
    fn of(payload: &'a Value) -> Result<Self, String> {
        payload
            .as_object()
            .map(Self)
            .ok_or_else(|| "Expected object".to_string())
    }

    /// A present, non-null value.
    fn present(&self, key: &str) -> Option<&'a Value> {
        self.0.get(key).filter(|value| !value.is_null())
    }

    fn text(&self, key: &str, max: Option<usize>) -> Result<String, String> {
        match self.0.get(key) {
            None => Err(at("Missing key", key)),
            Some(Value::String(text)) => Self::checked(text, key, max),
            Some(_) => Err(at("Expected string", key)),
        }
    }

    fn optional_text(&self, key: &str, max: Option<usize>) -> Result<Option<String>, String> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::String(text)) => Self::checked(text, key, max).map(Some),
            Some(_) => Err(at("Expected string | null", key)),
        }
    }

    /// `NullOr(TrimmedNonEmptyString)` without `optional`: the key must be there.
    fn nullable_text(&self, key: &str) -> Result<Option<String>, String> {
        match self.0.get(key) {
            None => Err(at("Missing key", key)),
            Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Self::checked(text, key, None).map(Some),
            Some(_) => Err(at("Expected string | null", key)),
        }
    }

    fn checked(text: &str, key: &str, max: Option<usize>) -> Result<String, String> {
        let trimmed = js_trim(text);
        if trimmed.is_empty() {
            return Err(at("Expected a non-blank string", key));
        }
        if let Some(max) = max
            && trimmed.encode_utf16().count() > max
        {
            return Err(at(
                &format!("Expected a value with a length of at most {max}"),
                key,
            ));
        }
        Ok(trimmed.to_string())
    }

    fn optional_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::Bool(flag)) => Ok(Some(*flag)),
            Some(_) => Err(at("Expected boolean | null", key)),
        }
    }

    /// An optional integer in `min..=max` (`Schema.Int` is a safe integer).
    fn optional_int(&self, key: &str, min: u64, max: Option<u64>) -> Result<Option<u64>, String> {
        let Some(value) = self.present(key) else {
            return Ok(None);
        };
        let Some(number) = value.as_f64() else {
            return Err(at("Expected number | null", key));
        };
        if number.fract() != 0.0 || number.abs() > 9_007_199_254_740_991.0 {
            return Err(at("Expected an integer", key));
        }
        if number < min as f64 {
            return Err(at(
                &format!("Expected a value greater than or equal to {min}"),
                key,
            ));
        }
        if let Some(max) = max
            && number > max as f64
        {
            return Err(at(
                &format!("Expected a value less than or equal to {max}"),
                key,
            ));
        }
        Ok(Some(number as u64))
    }

    fn optional_choice<T: Copy>(
        &self,
        key: &str,
        choices: &[(&str, T)],
    ) -> Result<Option<T>, String> {
        let Some(value) = self.present(key) else {
            return Ok(None);
        };
        if let Some(found) = value
            .as_str()
            .and_then(|text| choices.iter().find(|(name, _)| *name == text))
        {
            return Ok(Some(found.1));
        }
        let expected = choices
            .iter()
            .map(|(name, _)| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(" | ");
        // A string outside the choices fails the literal union; any other type also fails
        // the `null` alternative of an optional key.
        let suffix = if value.is_string() { "" } else { " | null" };
        Err(at(&format!("Expected {expected}{suffix}"), key))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusInput {
    pub cwd: String,
}

/// `VcsStatusInput`; `vcs.pull` and `vcs.refreshStatus` take only a working directory.
pub fn status_input(payload: &Value) -> Result<StatusInput, String> {
    let fields = Fields::of(payload)?;
    Ok(StatusInput {
        cwd: fields.text("cwd", None)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusSubscriptionInput {
    pub cwd: String,
    /// Passive observers (`false`) receive cached remote status without retaining its
    /// refresh loop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_remote: Option<bool>,
}

pub fn status_subscription_input(payload: &Value) -> Result<StatusSubscriptionInput, String> {
    let fields = Fields::of(payload)?;
    Ok(StatusSubscriptionInput {
        cwd: fields.text("cwd", None)?,
        include_remote: fields.optional_bool("includeRemote")?,
    })
}

pub fn list_refs_input(payload: &Value) -> Result<t3_vcs::wire::VcsListRefsInput, String> {
    let fields = Fields::of(payload)?;
    Ok(t3_vcs::wire::VcsListRefsInput {
        cwd: fields.text("cwd", None)?,
        query: fields.optional_text("query", Some(LIST_REFS_QUERY_MAX_LENGTH))?,
        cursor: fields.optional_int("cursor", 0, None)?.map(|n| n as usize),
        include_matching_remote_refs: fields.optional_bool("includeMatchingRemoteRefs")?,
        ref_kind: fields.optional_choice(
            "refKind",
            &[
                ("all", RefKind::All),
                ("local", RefKind::Local),
                ("remote", RefKind::Remote),
            ],
        )?,
        refresh: fields.optional_bool("refresh")?,
        limit: fields
            .optional_int("limit", 1, Some(LIST_REFS_MAX_LIMIT))?
            .map(|n| n as usize),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorktreeInput {
    pub cwd: String,
    pub ref_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_ref_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_ref_name: Option<String>,
    pub path: Option<String>,
}

pub fn create_worktree_input(payload: &Value) -> Result<CreateWorktreeInput, String> {
    let fields = Fields::of(payload)?;
    Ok(CreateWorktreeInput {
        cwd: fields.text("cwd", None)?,
        ref_name: fields.text("refName", None)?,
        new_ref_name: fields.optional_text("newRefName", None)?,
        base_ref_name: fields.optional_text("baseRefName", None)?,
        path: fields.nullable_text("path")?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoveWorktreeInput {
    pub cwd: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
}

pub fn remove_worktree_input(payload: &Value) -> Result<RemoveWorktreeInput, String> {
    let fields = Fields::of(payload)?;
    Ok(RemoveWorktreeInput {
        cwd: fields.text("cwd", None)?,
        path: fields.text("path", None)?,
        force: fields.optional_bool("force")?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRefInput {
    pub cwd: String,
    pub ref_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_ref: Option<bool>,
}

pub fn create_ref_input(payload: &Value) -> Result<CreateRefInput, String> {
    let fields = Fields::of(payload)?;
    Ok(CreateRefInput {
        cwd: fields.text("cwd", None)?,
        ref_name: fields.text("refName", None)?,
        switch_ref: fields.optional_bool("switchRef")?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchRefInput {
    pub cwd: String,
    pub ref_name: String,
}

pub fn switch_ref_input(payload: &Value) -> Result<SwitchRefInput, String> {
    let fields = Fields::of(payload)?;
    Ok(SwitchRefInput {
        cwd: fields.text("cwd", None)?,
        ref_name: fields.text("refName", None)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitInput {
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<VcsDriverKind>,
}

pub fn init_input(payload: &Value) -> Result<InitInput, String> {
    let fields = Fields::of(payload)?;
    Ok(InitInput {
        cwd: fields.text("cwd", None)?,
        kind: fields.optional_choice(
            "kind",
            &[
                ("git", VcsDriverKind::Git),
                ("jj", VcsDriverKind::Jj),
                ("unknown", VcsDriverKind::Unknown),
            ],
        )?,
    })
}
