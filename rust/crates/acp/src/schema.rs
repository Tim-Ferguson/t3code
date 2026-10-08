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
        });
    };
    check(id, &value, "$", 0).map_err(|(path, expected)| SchemaError {
        schema: schema.into(),
        path,
        expected,
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
