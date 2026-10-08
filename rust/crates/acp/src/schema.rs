//! Generated schema-backed wire values. Every nested field is checked before
//! a value enters the client. Unknown fields survive only in source records or
//! explicitly extensible variants; ordinary struct fields are canonicalized.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::{collections::HashMap, marker::PhantomData, sync::OnceLock};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Invalid ACP {schema} at {path}: expected {expected}")]
pub struct SchemaError {
    pub schema: String,
    pub path: String,
    pub expected: String,
    pub issue: ValidationIssue,
    pub cause: Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<ValidationIssue>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueDiagnostics {
    pub issue_count: usize,
    pub issue_kinds: Vec<String>,
    pub maximum_path_depth: usize,
}
impl ValidationIssue {
    fn leaf(kind: &str) -> Self {
        Self {
            kind: kind.into(),
            path: Vec::new(),
            issues: Vec::new(),
        }
    }
    fn children(kind: &str, issues: Vec<Self>) -> Self {
        Self {
            kind: kind.into(),
            path: Vec::new(),
            issues,
        }
    }
    fn pointer(key: String, issue: Self) -> Self {
        Self {
            kind: "Pointer".into(),
            path: vec![key],
            issues: vec![issue],
        }
    }
    pub fn diagnostics(&self) -> IssueDiagnostics {
        fn visit(issue: &ValidationIssue, depth: usize, out: &mut IssueDiagnostics) {
            out.issue_count += 1;
            if !out.issue_kinds.contains(&issue.kind) {
                out.issue_kinds.push(issue.kind.clone());
            }
            out.maximum_path_depth = out.maximum_path_depth.max(depth);
            let child_depth = depth
                + if issue.kind == "Pointer" {
                    issue.path.len()
                } else {
                    0
                };
            for child in &issue.issues {
                visit(child, child_depth, out);
            }
        }
        let mut out = IssueDiagnostics {
            issue_count: 0,
            issue_kinds: Vec::new(),
            maximum_path_depth: 0,
        };
        visit(self, 0, &mut out);
        out
    }
}
pub trait SchemaName {
    const NAME: &'static str;
}
pub struct Wire<S: SchemaName> {
    value: Value,
    marker: PhantomData<S>,
}
impl<S: SchemaName> Wire<S> {
    pub fn decode(value: Value) -> Result<Self, SchemaError> {
        Ok(Self {
            value: decode(S::NAME, value)?,
            marker: PhantomData,
        })
    }
    pub fn as_value(&self) -> &Value {
        &self.value
    }
    pub fn into_value(self) -> Value {
        self.value
    }
}
impl<S: SchemaName> Clone for Wire<S> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            marker: PhantomData,
        }
    }
}
impl<S: SchemaName> std::fmt::Debug for Wire<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}
impl<S: SchemaName> PartialEq for Wire<S> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
impl<S: SchemaName> Serialize for Wire<S> {
    fn serialize<T: Serializer>(&self, s: T) -> Result<T::Ok, T::Error> {
        self.value.serialize(s)
    }
}
impl<'de, S: SchemaName> Deserialize<'de> for Wire<S> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::decode(Value::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Deserialize)]
struct Table {
    roots: HashMap<String, usize>,
    nodes: Vec<Node>,
}
#[derive(Deserialize)]
struct Node {
    kind: String,
    #[serde(default)]
    checks: Vec<Check>,
    target: Option<usize>,
    #[serde(default)]
    fields: Vec<Field>,
    #[serde(default)]
    index: Vec<Index>,
    item: Option<usize>,
    #[serde(default)]
    members: Vec<usize>,
    value: Option<Value>,
}
#[derive(Deserialize)]
struct Field {
    name: String,
    node: usize,
    optional: bool,
}
#[derive(Deserialize)]
struct Index {
    key: usize,
    value: usize,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Check {
    kind: String,
    minimum: Option<f64>,
    maximum: Option<f64>,
    min_length: Option<usize>,
    #[serde(default)]
    values: Vec<String>,
}
fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(include_str!("schema_table.json")).expect("generated ACP schema table")
    })
}
pub fn schema_names() -> impl Iterator<Item = &'static str> {
    table().roots.keys().map(String::as_str)
}
pub fn decode(schema: &str, value: Value) -> Result<Value, SchemaError> {
    let Some(&id) = table().roots.get(schema) else {
        return Err(SchemaError {
            schema: schema.into(),
            path: "$".into(),
            expected: "a known ACP schema".into(),
            issue: ValidationIssue::leaf("InvalidType"),
            cause: value,
        });
    };
    check(id, &value, "$", 0).map_err(|(path, expected)| SchemaError {
        schema: schema.into(),
        path,
        expected,
        issue: collect_issue(id, &value, 0).unwrap_or_else(|| ValidationIssue::leaf("InvalidType")),
        cause: value.clone(),
    })
}
fn check(id: usize, value: &Value, path: &str, depth: usize) -> Result<Value, (String, String)> {
    let n = &table().nodes[id];
    let fail = || Err((path.into(), n.kind.clone()));
    if depth > 256 {
        return Err((path.into(), "bounded nesting".into()));
    }
    let value = match n.kind.as_str() {
        "Suspend" => check(n.target.unwrap(), value, path, depth + 1)?,
        "Json" => value.clone(),
        "String" if value.is_string() => value.clone(),
        "Number" if value.is_number() => value.clone(),
        "Boolean" if value.is_boolean() => value.clone(),
        "Null" if value.is_null() => value.clone(),
        "Literal" if Some(value) == n.value.as_ref() => value.clone(),
        "Union" => {
            let mut found = None;
            for member in &n.members {
                if let Ok(v) = check(*member, value, path, depth + 1) {
                    found = Some(v);
                    break;
                }
            }
            match found {
                Some(v) => v,
                None => return fail(),
            }
        }
        "Arrays" => {
            let Value::Array(values) = value else {
                return fail();
            };
            Value::Array(
                values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| check(n.item.unwrap(), v, &format!("{path}[{i}]"), depth + 1))
                    .collect::<Result<_, _>>()?,
            )
        }
        "Objects" => {
            let Value::Object(input) = value else {
                return fail();
            };
            for field in &n.fields {
                if let Some(v) = input.get(&field.name) {
                    if literal_gate(field.node, v) == Some(false) {
                        return fail();
                    }
                }
            }
            let mut output = serde_json::Map::new();
            for field in &n.fields {
                match input.get(&field.name) {
                    Some(v) => {
                        output.insert(
                            field.name.clone(),
                            check(field.node, v, &format!("{path}.{}", field.name), depth + 1)?,
                        );
                    }
                    None if field.optional => {}
                    None => {
                        return Err((format!("{path}.{}", field.name), "required field".into()));
                    }
                }
            }
            for (key, value) in input {
                if n.fields.iter().any(|f| &f.name == key) {
                    continue;
                }
                for index in &n.index {
                    if check(index.key, &Value::String(key.clone()), path, depth + 1).is_ok() {
                        output.insert(
                            key.clone(),
                            check(index.value, value, &format!("{path}.{key}"), depth + 1)?,
                        );
                    }
                }
            }
            Value::Object(output)
        }
        _ => return fail(),
    };
    for c in &n.checks {
        let valid = match c.kind.as_str() {
            "isInt" => value.as_f64().is_some_and(|x| {
                x.is_finite() && x.fract() == 0.0 && x.abs() <= 9_007_199_254_740_991.0
            }),
            "isFinite" => value.as_f64().is_some_and(f64::is_finite),
            "isGreaterThanOrEqualTo" => value.as_f64().is_some_and(|x| x >= c.minimum.unwrap()),
            "isLessThanOrEqualTo" => value.as_f64().is_some_and(|x| x <= c.maximum.unwrap()),
            "isMinLength" => {
                value
                    .as_str()
                    .is_some_and(|s| s.encode_utf16().count() >= c.min_length.unwrap())
                    || value
                        .as_array()
                        .is_some_and(|a| a.len() >= c.min_length.unwrap())
            }
            "exclude" => value
                .as_str()
                .is_some_and(|s| !c.values.iter().any(|v| v == s)),
            "currency" => value
                .as_str()
                .is_some_and(|s| s.len() == 3 && s.bytes().all(|b| b.is_ascii_uppercase())),
            _ => false,
        };
        if !valid {
            return Err((path.into(), c.kind.clone()));
        }
    }
    Ok(value)
}

fn literal_gate(id: usize, value: &Value) -> Option<bool> {
    let n = &table().nodes[id];
    match n.kind.as_str() {
        "Suspend" => literal_gate(n.target.unwrap(), value),
        "Literal" => Some(n.value.as_ref() == Some(value)),
        "Union" => {
            let matches = n
                .members
                .iter()
                .map(|m| literal_gate(*m, value))
                .collect::<Option<Vec<_>>>()?;
            Some(matches.into_iter().any(|m| m))
        }
        _ => None,
    }
}

// Effect's default parser reports the first field/array failure, with Composite
// and Pointer wrappers. Union diagnostics include only type/sentinel candidates.
// This runs only on failure, leaving successful hot-path decoding unchanged.
fn eligible(id: usize, value: &Value) -> bool {
    let n = &table().nodes[id];
    match n.kind.as_str() {
        // Effect deliberately treats suspended union members as unknown during
        // candidate selection, then validates the suspended AST normally.
        "Suspend" => true,
        "String" => value.is_string(),
        "Number" => value.is_number(),
        "Boolean" => value.is_boolean(),
        "Null" => value.is_null(),
        "Literal" => n.value.as_ref() == Some(value),
        "Json" => true,
        "Never" => false,
        "Arrays" => value.is_array(),
        "Objects" => value.as_object().is_some_and(|o| {
            n.fields.iter().filter(|f| !f.optional).all(|f| {
                let field = &table().nodes[f.node];
                if field.kind == "Literal" {
                    o.get(&f.name) == field.value.as_ref()
                } else {
                    true
                }
            })
        }),
        "Union" => n.members.iter().any(|m| eligible(*m, value)),
        _ => false,
    }
}
fn collect_issue(id: usize, value: &Value, depth: usize) -> Option<ValidationIssue> {
    if depth > 256 {
        return Some(ValidationIssue::leaf("Forbidden"));
    }
    if check(id, value, "$", depth).is_ok() {
        return None;
    }
    let n = &table().nodes[id];
    let issue = match n.kind.as_str() {
        "Suspend" => return collect_issue(n.target.unwrap(), value, depth + 1),
        "Union" => ValidationIssue::children(
            "AnyOf",
            n.members
                .iter()
                .filter(|m| eligible(**m, value))
                .filter_map(|m| collect_issue(*m, value, depth + 1))
                .collect(),
        ),
        "Objects" => {
            let Some(values) = value.as_object() else {
                return Some(ValidationIssue::leaf("InvalidType"));
            };
            for field in &n.fields {
                let issue = match values.get(&field.name) {
                    Some(v) => collect_issue(field.node, v, depth + 1),
                    None if !field.optional => Some(ValidationIssue::leaf("MissingKey")),
                    None => None,
                };
                if let Some(issue) = issue {
                    return Some(ValidationIssue::children(
                        "Composite",
                        vec![ValidationIssue::pointer(field.name.clone(), issue)],
                    ));
                }
            }
            for (key, v) in values {
                if n.fields.iter().any(|f| f.name == *key) {
                    continue;
                }
                for index in &n.index {
                    if check(index.key, &Value::String(key.clone()), "$", depth + 1).is_ok() {
                        if let Some(issue) = collect_issue(index.value, v, depth + 1) {
                            return Some(ValidationIssue::children(
                                "Composite",
                                vec![ValidationIssue::pointer(key.clone(), issue)],
                            ));
                        }
                    }
                }
            }
            ValidationIssue::children(
                "Composite",
                vec![ValidationIssue::children(
                    "Filter",
                    vec![ValidationIssue::leaf("InvalidValue")],
                )],
            )
        }
        "Arrays" => {
            let Some(values) = value.as_array() else {
                return Some(ValidationIssue::leaf("InvalidType"));
            };
            for (index, v) in values.iter().enumerate() {
                if let Some(issue) = collect_issue(n.item.unwrap(), v, depth + 1) {
                    return Some(ValidationIssue::children(
                        "Composite",
                        vec![ValidationIssue::pointer(index.to_string(), issue)],
                    ));
                }
            }
            ValidationIssue::children(
                "Composite",
                vec![ValidationIssue::children(
                    "Filter",
                    vec![ValidationIssue::leaf("InvalidValue")],
                )],
            )
        }
        _ if eligible(id, value) => ValidationIssue::children(
            "Composite",
            vec![ValidationIssue::children(
                "Filter",
                vec![ValidationIssue::leaf("InvalidValue")],
            )],
        ),
        _ => ValidationIssue::leaf("InvalidType"),
    };
    Some(issue)
}
