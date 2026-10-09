//! Effect Schema decoding of a tool's parameters, run over the schema `mcp_tool_schemas.json`
//! writes from the original. The interpreter follows the library's own parser (`SchemaAST` struct,
//! array, union and check handling, with `errors: "all"` and excess properties ignored) and its
//! default formatter, so a call is admitted or refused with the same text the model reads there.
//! Functions the schema holds as code live in `mcp_tool_functions.rs`, found by source hash.
use crate::mcp_tool_functions::{self as functions, Outcome};
use serde::Deserialize;
use serde_json::{Number, Value};
use std::{collections::HashMap, sync::OnceLock};

/// A JavaScript value as the decoder sees it: JSON plus `undefined`, which a transformation can
/// produce and which a missing key is not.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Js {
    Undefined,
    Null,
    Bool(bool),
    Num(Number),
    /// NaN and the infinities, which a JSON number cannot be but the JSON codec reads from "NaN", "Infinity" and "-Infinity".
    NonFinite(f64),
    Str(String),
    Arr(Vec<Js>),
    Obj(Vec<(String, Js)>),
}

impl Js {
    pub(crate) fn from_json(value: &Value) -> Js {
        match value {
            Value::Null => Js::Null,
            Value::Bool(flag) => Js::Bool(*flag),
            Value::Number(number) => Js::Num(number.clone()),
            Value::String(text) => Js::Str(text.clone()),
            Value::Array(items) => Js::Arr(items.iter().map(Js::from_json).collect()),
            Value::Object(object) => Js::Obj(object.iter().map(|(key, found)| (key.clone(), Js::from_json(found))).collect()),
        }
    }
    /// What `JSON.stringify` writes: undefined properties vanish.
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Js::Undefined | Js::Null => Value::Null,
            Js::Bool(flag) => Value::Bool(*flag),
            Js::Num(number) => Value::Number(number.clone()),
            Js::NonFinite(number) => Value::String(if number.is_nan() { "NaN" } else if *number > 0.0 { "Infinity" } else { "-Infinity" }.into()),
            Js::Str(text) => Value::String(text.clone()),
            Js::Arr(items) => Value::Array(items.iter().map(Js::to_json).collect()),
            Js::Obj(entries) => Value::Object(entries.iter().filter(|(_, found)| *found != Js::Undefined).map(|(key, found)| (key.clone(), found.to_json())).collect()),
        }
    }
    pub(crate) fn get(&self, key: &str) -> Option<&Js> {
        match self {
            Js::Obj(entries) => entries.iter().find(|(name, _)| name == key).map(|(_, found)| found),
            _ => None,
        }
    }
    /// `input[key] !== undefined`
    pub(crate) fn defined(&self, key: &str) -> bool {
        self.get(key).is_some_and(|found| *found != Js::Undefined)
    }
    fn runtime_type(&self) -> &'static str {
        match self {
            Js::Undefined => "undefined",
            Js::Null => "null",
            Js::Bool(_) => "boolean",
            Js::Num(_) | Js::NonFinite(_) => "number",
            Js::Str(_) => "string",
            Js::Arr(_) => "array",
            Js::Obj(_) => "object",
        }
    }
    /// SameValueZero against a schema literal.
    fn is_literal(&self, literal: &Value) -> bool {
        match (self, literal) {
            (Js::Str(a), Value::String(b)) => a == b,
            (Js::Bool(a), Value::Bool(b)) => a == b,
            (Js::Num(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
            _ => false,
        }
    }
}

/// Own enumerable keys in the order `Object.keys` gives: integer-like keys ascending, then the rest as written.
fn js_keys(entries: &[(String, Js)]) -> Vec<&(String, Js)> {
    let index = |key: &str| key.parse::<u32>().ok().filter(|number| *number != u32::MAX && number.to_string() == key);
    let mut integers: Vec<(u32, &(String, Js))> = entries.iter().filter_map(|entry| index(&entry.0).map(|number| (number, entry))).collect();
    integers.sort_by_key(|(number, _)| *number);
    integers.into_iter().map(|(_, entry)| entry).chain(entries.iter().filter(|entry| index(&entry.0).is_none())).collect()
}

#[derive(Deserialize, Default)]
pub(crate) struct Schemas {
    nodes: Vec<Node>,
    #[serde(default)]
    tools: HashMap<String, usize>,
    /// WebSocket RPC methods whose payload the original's decoding is run for (`rpc_admission.rs`).
    #[serde(default)]
    rpc: HashMap<String, usize>,
    functions: HashMap<String, Function>,
    /// What a client decodes of each method's replies (`rpc_response.rs`): the exit, and for a stream the chunk.
    #[serde(default)]
    responses: HashMap<String, Response>,
    /// The most nodes one decode visits; a reply is far larger than a payload.
    #[serde(skip, default = "default_visits")]
    max_visits: usize,
}
fn default_visits() -> usize {
    MAX_VISITS
}
#[derive(Deserialize)]
struct Response {
    exit: usize,
    #[serde(default)]
    chunk: Option<usize>,
}
/// The functions are read by the test that checks the port implements each one.
#[derive(Deserialize)]
#[cfg_attr(not(test), allow(dead_code))]
struct Function {
    kind: String,
    source: String,
}
#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Objects,
    Arrays,
    Union,
    TemplateLiteral,
    Literal,
    String,
    Number,
    Boolean,
    Null,
    Undefined,
    Never,
    Unknown,
    /// A Duration, a DateTime or JSON itself: whatever the encoding link made always passes it.
    Declaration,
    /// A recursive schema's way back to a node.
    Suspend,
    /// `undefined`, what a method with no result returns.
    Void,
}
#[derive(Deserialize)]
struct Node {
    t: Tag,
    expected: String,
    #[serde(default)]
    literal: Option<Value>,
    #[serde(default)]
    props: Vec<Prop>,
    #[serde(default)]
    indexes: Vec<Index>,
    #[serde(default)]
    elements: Vec<Element>,
    #[serde(default)]
    rest: Vec<usize>,
    #[serde(default)]
    types: Vec<usize>,
    #[serde(default)]
    members: Vec<Member>,
    #[serde(default)]
    checks: Vec<Check>,
    #[serde(default)]
    encoding: Vec<Link>,
    /// A template literal: a literal prefix and suffix around one part.
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
    #[serde(default)]
    middle: usize,
    #[serde(default)]
    target: usize,
    /// The schemas an Exit, a Cause, a cause reason or an Option was made from (its `expected` says which), which it decodes the
    /// value it holds with once its encoding has been read.
    #[serde(default)]
    params: Vec<usize>,
    /// Built on the first union parse of this node.
    #[serde(skip)]
    plan: OnceLock<Plan>,
}
#[derive(Deserialize)]
struct Prop {
    name: String,
    optional: bool,
    #[serde(rename = "type")]
    ty: usize,
}
#[derive(Deserialize)]
struct Index {
    key: usize,
    value: usize,
}
#[derive(Deserialize)]
struct Element {
    optional: bool,
    #[serde(rename = "type")]
    ty: usize,
}
/// What a union reads of a member to choose which members to try for an input.
#[derive(Deserialize)]
struct Member {
    never: bool,
    #[serde(default)]
    literal: Option<Value>,
    sentinels: Vec<Sentinel>,
    types: Vec<String>,
}
#[derive(Deserialize)]
struct Sentinel {
    key: Value,
    literal: Value,
}
#[derive(Deserialize)]
struct Link {
    to: usize,
    #[serde(rename = "fn", default)]
    function: Option<String>,
    /// `withDecodingDefault`: a missing or undefined value becomes this.
    #[serde(default)]
    default: Option<DefaultValue>,
    /// `catchDecoding` turns a failure into `undefined` ("catch"); "identity" leaves it as it is.
    #[serde(default)]
    middleware: Option<String>,
    /// A transformation that ignores its input: its one result, `undefined` when it has no `value`.
    #[serde(default)]
    constant: Option<DefaultValue>,
    /// A field kept when the schema at this node accepts it, `undefined` otherwise (`ForwardCompatibleOptional`).
    #[serde(default)]
    tolerant: Option<usize>,
    /// What a tolerant field becomes where it is not kept: "null" (`ForwardCompatibleNullable`), else absent.
    #[serde(default)]
    fallback: Option<String>,
    /// A tolerant field that reads a missing key as null as well (`OmittedWhenNull`).
    #[serde(default)]
    omitted: bool,
    /// What a transformation closes over (the string an empty one becomes).
    #[serde(default)]
    closure: Option<Value>,
}
#[derive(Deserialize)]
struct DefaultValue {
    #[serde(default)]
    value: Option<Value>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Check {
    expected: Option<String>,
    aborted: bool,
    group: Option<Vec<Check>>,
    #[serde(rename = "fn")]
    function: Option<String>,
    #[serde(rename = "maxLength")]
    max_length: Option<usize>,
    #[serde(rename = "minLength")]
    min_length: Option<usize>,
    /// Strings a function over a set closes over, which it refuses.
    rejects: Option<Vec<String>>,
    /// The turn item types a thread stream's unknown-event filter knows, and the text a known-tag filter fails with.
    items: Option<Vec<String>>,
    message: Option<String>,
    #[serde(rename = "maxProperties")]
    max_properties: Option<usize>,
    #[serde(rename = "minProperties")]
    min_properties: Option<usize>,
    int: bool,
    finite: bool,
    trimmed: bool,
    minimum: Option<f64>,
    maximum: Option<f64>,
    #[serde(rename = "exclusiveMinimum")]
    exclusive_minimum: Option<f64>,
    #[serde(rename = "exclusiveMaximum")]
    exclusive_maximum: Option<f64>,
    pattern: Option<String>,
    flags: Option<String>,
    #[serde(skip)]
    regex: OnceLock<Option<Pattern>>,
}

pub(crate) fn schemas() -> &'static Schemas {
    static SCHEMAS: OnceLock<Schemas> = OnceLock::new();
    SCHEMAS.get_or_init(|| Schemas { max_visits: MAX_VISITS, ..serde_json::from_str(include_str!("mcp_tool_schemas.json")).expect("source-generated tool schemas") })
}
/// What a client decodes of the server's replies, written by the same generator from the same schemas.
#[cfg(any(test, feature = "outbound-check"))]
pub(crate) fn response_schemas() -> &'static Schemas {
    static SCHEMAS: OnceLock<Schemas> = OnceLock::new();
    SCHEMAS.get_or_init(|| Schemas { max_visits: 40_000_000, ..serde_json::from_str(include_str!("rpc_response_schemas.jsonl")).expect("source-generated response schemas") })
}

/// The functions the schemas hold as code, each with the kind the port must implement it as.
#[cfg(test)]
pub(crate) fn function_hashes() -> Vec<(&'static str, &'static str, &'static str)> {
    schemas().functions.iter().map(|(hash, function)| (hash.as_str(), function.kind.as_str(), function.source.as_str())).collect()
}
/// The functions the replies' schemas hold.
#[cfg(test)]
pub(crate) fn response_function_hashes() -> Vec<(&'static str, &'static str, &'static str)> {
    response_schemas().functions.iter().map(|(hash, function)| (hash.as_str(), function.kind.as_str(), function.source.as_str())).collect()
}
/// Patterns the schemas check that the regex crate cannot compile; none when the port runs them all.
#[cfg(test)]
pub(crate) fn unrunnable_patterns() -> Vec<String> {
    fn walk(checks: &[Check], bad: &mut Vec<String>) {
        for check in checks {
            if let Some(pattern) = &check.pattern {
                if Pattern::compile(pattern, check.flags.as_deref().unwrap_or_default()).is_none() {
                    bad.push(pattern.clone());
                }
            }
            walk(check.group.as_deref().unwrap_or_default(), bad);
        }
    }
    let mut bad = Vec::new();
    schemas().nodes.iter().for_each(|node| walk(&node.checks, &mut bad));
    bad
}
#[cfg(test)]
pub(crate) fn rpc_methods() -> impl Iterator<Item = &'static str> {
    schemas().rpc.keys().map(String::as_str)
}
#[cfg(test)]
pub(crate) fn tool_names() -> impl Iterator<Item = &'static str> {
    schemas().tools.keys().map(String::as_str)
}

/// A decoding failure, reduced to what the default formatter prints from it.
#[derive(Debug)]
pub(crate) enum Issue {
    Leaf(String),
    Pointer(Key, Box<Issue>),
    All(Vec<Issue>),
}
#[derive(Debug, Clone)]
pub(crate) enum Key {
    Name(String),
    Index(usize),
}

impl Issue {
    /// The library's default formatter: each leaf on its own line with the path it sits at.
    pub(crate) fn format(&self) -> String {
        self.format_at(String::new())
    }
    fn format_at(&self, path: String) -> String {
        match self {
            Issue::Leaf(message) if path.is_empty() => message.clone(),
            Issue::Leaf(message) => format!("{message}\n  at {path}"),
            Issue::Pointer(key, issue) => issue.format_at(format!("{path}[{}]", match key {
                Key::Name(name) => serde_json::to_string(name).unwrap_or_default(),
                Key::Index(index) => index.to_string(),
            })),
            Issue::All(issues) => issues.iter().map(|issue| issue.format_at(path.clone())).collect::<Vec<_>>().join("\n"),
        }
    }
}

type Parsed = Result<Option<Js>, Issue>;

impl Schemas {
    pub(crate) fn has(&self, tool: &str) -> bool {
        self.tools.contains_key(tool)
    }
    pub(crate) fn covers_rpc(&self, method: &str) -> bool {
        self.rpc.contains_key(method)
    }
    /// Decodes a reply as the client does: the `exit` of the response message, or for a stream the `values` of a chunk. None for
    /// a method with no such schema.
    #[cfg(any(test, feature = "outbound-check"))]
    pub(crate) fn decode_response(&self, method: &str, chunk: bool, input: &Js) -> Option<Result<Js, Issue>> {
        let response = self.responses.get(method)?;
        let root = if chunk { response.chunk? } else { response.exit };
        Some(Parser { strict: true, ..Parser::new(self, false) }.parse(root, Some(input)).map(|decoded| decoded.unwrap_or(Js::Undefined)))
    }
    #[cfg(any(test, feature = "outbound-check"))]
    pub(crate) fn response_methods(&self) -> impl Iterator<Item = (&str, bool)> {
        self.responses.iter().map(|(method, response)| (method.as_str(), response.chunk.is_some()))
    }
    /// The original's decoding of a WebSocket RPC payload, for a method listed in the schemas. The RPC server decodes with the
    /// library's defaults, which stop at the first issue of a struct or an array.
    pub(crate) fn decode_rpc(&self, method: &str, input: &Js) -> Option<Result<Js, Issue>> {
        let root = *self.rpc.get(method)?;
        Some(Parser::new(self, false).parse(root, Some(input)).map(|decoded| decoded.unwrap_or(Js::Undefined)))
    }
    /// Decodes `input` for `tool`, returning the value its handler receives. `all` reports every
    /// issue, as the toolkit's registration does; otherwise the first.
    pub(crate) fn decode(&self, tool: &str, input: &Js, all: bool) -> Result<Js, Issue> {
        let root = self.tools[tool];
        Ok(Parser::new(self, all).parse(root, Some(input))?.unwrap_or(Js::Undefined))
    }
}

/// The deepest nesting of schema nodes the interpreter follows and the most nodes it visits for one payload. The original's
/// parser is bounded by the engine's stack; here an adversarial payload is refused instead of exhausting this one. A payload
/// the transport parsed is at most 128 JSON levels deep (the parser's own limit), which a recursive schema follows in about 4
/// nodes per level pair.
const MAX_DEPTH: usize = 512;
const MAX_VISITS: usize = 1_000_000;

struct Parser<'a> {
    schemas: &'a Schemas,
    all: bool,
    /// `onExcessProperty: "error"`: a key no property or index signature covers is refused instead of ignored. A reply is
    /// checked this way, as what an encoder makes of a value never carries a key its schema does not name.
    strict: bool,
    depth: std::cell::Cell<usize>,
    visits: std::cell::Cell<usize>,
}

impl<'a> Parser<'a> {
    fn new(schemas: &'a Schemas, all: bool) -> Self {
        Parser { schemas, all, strict: false, depth: std::cell::Cell::new(0), visits: std::cell::Cell::new(0) }
    }
    fn mismatch(&self, node: &Node) -> Issue {
        Issue::Leaf(format!("Expected {}", node.expected))
    }
    /// A missing input is neither a failure nor a value; everything else is parsed through the
    /// encoded side, the transformations, then the node itself and its checks.
    fn parse(&self, id: usize, input: Option<&Js>) -> Parsed {
        let (depth, visits) = (self.depth.get() + 1, self.visits.get() + 1);
        if depth > MAX_DEPTH {
            return Err(Issue::Leaf("Maximum call stack size exceeded".into()));
        }
        if visits > self.schemas.max_visits {
            return Err(Issue::Leaf("The payload is too large to decode".into()));
        }
        self.depth.set(depth);
        self.visits.set(visits);
        let parsed = self.parse_node(id, input);
        self.depth.set(depth - 1);
        parsed
    }
    fn parse_node(&self, id: usize, input: Option<&Js>) -> Parsed {
        let node = &self.schemas.nodes[id];
        let Some(last) = node.encoding.last() else {
            let Some(input) = input else { return Ok(None) };
            return self.finish(node, input.clone());
        };
        // The library parses the encoded side, then each link's transformation, parsing the next link's side after it.
        let mut result = self.parse(last.to, input);
        for position in (0..node.encoding.len()).rev() {
            result = self.transform(&node.encoding[position], result);
            if position != 0 {
                result = match result {
                    Ok(found) => self.parse(node.encoding[position - 1].to, found.as_ref()),
                    failed => failed,
                };
            }
        }
        match result? {
            Some(value) => self.finish(node, value),
            None => Ok(None),
        }
    }
    /// One link's transformation of what the side before it parsed to. A failure passes through unless the link catches it.
    fn transform(&self, link: &Link, result: Parsed) -> Parsed {
        if link.middleware.as_deref() == Some("catch") {
            return Ok(result.unwrap_or(Some(Js::Undefined)));
        }
        let value = result?;
        if let Some(default) = &link.default {
            return Ok(Some(match value {
                Some(found) if found != Js::Undefined => found,
                _ => Js::from_json(default.value.as_ref().unwrap_or(&Value::Null)),
            }));
        }
        if let Some(constant) = &link.constant {
            return Ok(value.map(|_| constant.value.as_ref().map_or(Js::Undefined, Js::from_json)));
        }
        if let Some(member) = link.tolerant {
            let dropped = if link.fallback.as_deref() == Some("null") { Js::Null } else { Js::Undefined };
            return Ok(match value {
                Some(raw) => Some(if self.parse(member, Some(&raw)).is_ok() { raw } else { dropped }),
                None if link.omitted => Some(Js::Null),
                None => None,
            });
        }
        match (&link.function, value) {
            (Some(hash), Some(value)) => functions::transform(hash, value, link.closure.as_ref()).map(Some).map_err(|expected| Issue::Leaf(format!("Expected {expected}"))),
            (_, value) => Ok(value),
        }
    }
    /// The node's own shape, then its checks.
    fn finish(&self, node: &Node, value: Js) -> Parsed {
        let value = self.local(node, value)?;
        let mut issues = Vec::new();
        self.collect(&node.checks, &value, &mut issues);
        if issues.is_empty() { Ok(Some(value)) } else { Err(Issue::All(issues.into_iter().map(|(issue, _)| issue).collect())) }
    }

    fn local(&self, node: &Node, value: Js) -> Result<Js, Issue> {
        let fits = match node.t {
            Tag::String => matches!(value, Js::Str(_)),
            Tag::Number => matches!(value, Js::Num(_) | Js::NonFinite(_)),
            Tag::Boolean => matches!(value, Js::Bool(_)),
            Tag::Null => value == Js::Null,
            Tag::Undefined => value == Js::Undefined,
            Tag::Literal => value.is_literal(node.literal.as_ref().unwrap_or(&Value::Null)),
            Tag::Declaration if !node.params.is_empty() => return self.declaration(node, value),
            Tag::Unknown | Tag::Declaration => true,
            Tag::Void => value == Js::Undefined,
            Tag::Never => false,
            Tag::Suspend => return Ok(self.parse(node.target, Some(&value))?.unwrap_or(Js::Undefined)),
            Tag::TemplateLiteral => return self.template(node, value),
            Tag::Objects => return self.objects(node, value),
            Tag::Arrays => return self.arrays(node, value),
            Tag::Union => return self.union(node, value),
        };
        if fits { Ok(value) } else { Err(self.mismatch(node)) }
    }

    /// The `run` of the library's Exit, Cause, CauseReason and Option declarations (`Schema.js`): the value, as the codec's encoding
    /// made it, is decoded with the schemas the declaration was made from, and an issue is put at the key the library puts it at.
    fn declaration(&self, node: &Node, value: Js) -> Result<Js, Issue> {
        let at = |key: &str, issue: Issue| Issue::Pointer(Key::Name(key.into()), Box::new(issue));
        let field = |name: &str| value.get(name).cloned();
        let decode = |param: usize, input: Option<Js>| self.parse(param, input.as_ref()).map(|decoded| decoded.unwrap_or(Js::Undefined));
        let tag = match value.get("_tag") {
            Some(Js::Str(tag)) => tag.as_str(),
            _ => "",
        };
        // One reason of a cause: a typed failure is decoded with the error schema, a defect with the defect schema.
        let reason = |error: usize, defect: usize, reason: &Js| -> Result<(), Issue> {
            match reason.get("_tag") {
                Some(Js::Str(kind)) if kind == "Fail" => decode(error, reason.get("error").cloned()).map(|_| ()).map_err(|issue| at("error", issue)),
                Some(Js::Str(kind)) if kind == "Die" => decode(defect, reason.get("defect").cloned()).map(|_| ()).map_err(|issue| at("defect", issue)),
                _ => Ok(()),
            }
        };
        let (error, defect) = match node.expected.as_str() {
            "Exit" => (node.params[1], node.params[2]),
            _ => (node.params[0], *node.params.get(1).unwrap_or(&node.params[0])),
        };
        match node.expected.as_str() {
            "Option" if tag == "Some" => decode(node.params[0], field("value")).map(|_| ()).map_err(|issue| at("value", issue))?,
            "Exit" if tag == "Success" => decode(node.params[0], field("value")).map(|_| ()).map_err(|issue| at("value", issue))?,
            "Exit" | "Cause" => {
                let Some(Js::Arr(reasons)) = (if node.expected == "Exit" { field("cause") } else { Some(value.clone()) }).as_ref().map(|found| found.clone()) else { return Ok(value) };
                for (position, found) in reasons.iter().enumerate() {
                    let issue = reason(error, defect, found).err().map(|issue| at("failures", Issue::Pointer(Key::Index(position), Box::new(issue))));
                    if let Some(issue) = issue {
                        return Err(if node.expected == "Exit" { at("cause", issue) } else { issue });
                    }
                }
            }
            "Cause.Failure" => reason(error, defect, &value)?,
            _ => {}
        }
        Ok(value)
    }

    /// A prefix, one part and a suffix: the part is read from what lies between them, as the element of the tuple the
    /// library parses a template literal as. Anything but a string is not one, and a string that is not cut by the
    /// prefix and suffix does not match the parts.
    fn template(&self, node: &Node, value: Js) -> Result<Js, Issue> {
        let Js::Str(text) = &value else { return Err(Issue::Leaf("Expected string".into())) };
        let Some(between) = text.strip_prefix(node.prefix.as_str()).and_then(|rest| rest.strip_suffix(node.suffix.as_str())) else { return Err(Issue::Leaf("Expected a string matching template literal parts".into())) };
        self.parse(node.middle, Some(&Js::Str(between.to_owned()))).map_err(|issue| Issue::Pointer(Key::Index(1), Box::new(issue)))?;
        Ok(value)
    }
    /// Whether the key is one the index signature reads at all: the library takes the keys that pass the checks of the
    /// parameter's encoded side (or fit its template, or one of its union members) and leaves the others alone.
    /// Whether an index signature reads the key, so that it is no excess: any string for a plain string parameter.
    fn key_covered(&self, id: usize, key: &str) -> bool {
        let plain = {
            let parameter = &self.schemas.nodes[id];
            parameter.t == Tag::String && parameter.checks.is_empty() && parameter.encoding.is_empty()
        };
        plain || self.key_selected(id, key)
    }
    fn key_selected(&self, id: usize, key: &str) -> bool {
        let mut node = &self.schemas.nodes[id];
        while let Some(last) = node.encoding.last() {
            node = &self.schemas.nodes[last.to];
        }
        match node.t {
            Tag::String => {
                let mut issues = Vec::new();
                self.collect(&node.checks, &Js::Str(key.to_owned()), &mut issues);
                issues.is_empty()
            }
            Tag::TemplateLiteral => key.strip_prefix(node.prefix.as_str()).is_some_and(|rest| rest.strip_suffix(node.suffix.as_str()).is_some()),
            Tag::Union => node.types.iter().any(|member| self.key_selected(*member, key)),
            _ => false,
        }
    }

    fn objects(&self, node: &Node, input: Js) -> Result<Js, Issue> {
        if node.props.is_empty() && node.indexes.is_empty() {
            return if matches!(input, Js::Null | Js::Undefined) { Err(self.mismatch(node)) } else { Ok(input) };
        }
        let Js::Obj(record) = &input else { return Err(self.mismatch(node)) };
        let mut out: Vec<(String, Js)> = Vec::new();
        let mut issues = Vec::new();
        macro_rules! fail {
            ($key:expr, $issue:expr) => {{
                issues.push(Issue::Pointer($key, Box::new($issue)));
                if !self.all {
                    return Err(Issue::All(issues));
                }
            }};
        }
        if self.strict {
            for (key, _) in js_keys(record) {
                if !node.props.iter().any(|prop| prop.name == *key) && !node.indexes.iter().any(|index| self.key_covered(index.key, key)) {
                    fail!(Key::Name(key.clone()), Issue::Leaf("Expected no excess property".into()));
                }
            }
        }
        for prop in &node.props {
            match self.parse(prop.ty, record.iter().find(|(name, _)| *name == prop.name).map(|(_, found)| found)) {
                Err(issue) => fail!(Key::Name(prop.name.clone()), issue),
                Ok(Some(found)) => out.push((prop.name.clone(), found)),
                Ok(None) if !prop.optional => fail!(Key::Name(prop.name.clone()), Issue::Leaf("Missing key".into())),
                Ok(None) => {}
            }
        }
        for index in &node.indexes {
            let plain_key = {
                let key = &self.schemas.nodes[index.key];
                key.t == Tag::String && key.checks.is_empty() && key.encoding.is_empty()
            };
            for (key, found) in js_keys(record) {
                if !plain_key && !self.key_selected(index.key, key) {
                    continue;
                }
                let named = if plain_key {
                    Js::Str(key.clone())
                } else {
                    match self.parse(index.key, Some(&Js::Str(key.clone()))) {
                        Ok(named) => named.unwrap_or(Js::Undefined),
                        Err(issue) => {
                            fail!(Key::Name(key.clone()), issue);
                            continue;
                        }
                    }
                };
                match self.parse(index.value, Some(found)) {
                    Err(issue) => fail!(Key::Name(key.clone()), issue),
                    Ok(Some(value)) => {
                        if let Js::Str(named) = named {
                            if !node.props.iter().any(|prop| prop.name == *key || prop.name == named) {
                                out.retain(|(name, _)| *name != named);
                                out.push((named, value));
                            }
                        }
                    }
                    Ok(None) => {}
                }
            }
        }
        if issues.is_empty() { Ok(Js::Obj(out)) } else { Err(Issue::All(issues)) }
    }

    fn arrays(&self, node: &Node, input: Js) -> Result<Js, Issue> {
        let Js::Arr(items) = &input else { return Err(self.mismatch(node)) };
        let (len, fixed) = (items.len(), node.elements.len());
        let tail = node.rest.len().saturating_sub(1);
        let threshold = fixed.max(len.saturating_sub(tail));
        let end = if node.rest.is_empty() { fixed } else { len.max(fixed + tail) };
        let mut out = vec![Js::Undefined; len];
        let mut issues = Vec::new();
        macro_rules! fail {
            ($index:expr, $issue:expr) => {{
                issues.push(Issue::Pointer(Key::Index($index), Box::new($issue)));
                if !self.all {
                    return Err(Issue::All(issues));
                }
            }};
        }
        for index in 0..end {
            let (ty, optional) = if index < fixed {
                (node.elements[index].ty, node.elements[index].optional)
            } else if index >= threshold {
                (node.rest[index - threshold + 1], false)
            } else {
                (node.rest[0], false)
            };
            match self.parse(ty, items.get(index)) {
                Err(issue) => fail!(index, issue),
                Ok(Some(found)) => out[index] = found,
                Ok(None) if !optional => fail!(index, Issue::Leaf("Missing key".into())),
                Ok(None) => {}
            }
        }
        if node.rest.is_empty() {
            for index in fixed..len {
                fail!(index, Issue::Leaf("Expected no excess property".into()));
            }
        }
        if issues.is_empty() { Ok(Js::Arr(out)) } else { Err(Issue::All(issues)) }
    }

    fn union(&self, node: &Node, input: Js) -> Result<Js, Issue> {
        let candidates = node.plan.get_or_init(|| Plan::build(node)).select(&input);
        if candidates.is_empty() {
            return Err(self.mismatch(node));
        }
        let mut issues = Vec::new();
        for member in candidates {
            match self.parse(node.types[member], Some(&input)) {
                Ok(found) => return Ok(found.unwrap_or(Js::Undefined)),
                Err(issue) => issues.push(issue),
            }
        }
        Err(Issue::All(issues))
    }

    /// The library's `collectIssues`: every failing check unless one aborts, or only the first when not collecting all.
    fn collect(&self, checks: &[Check], value: &Js, issues: &mut Vec<(Issue, bool)>) {
        for check in checks {
            if let Some(group) = &check.group {
                self.collect(group, value, issues);
                if issues.last().is_some_and(|(_, aborted)| !self.all || *aborted) {
                    return;
                }
            } else if let Some(message) = check.fails(value) {
                issues.push((Issue::Leaf(message), check.aborted));
                if !self.all || check.aborted {
                    return;
                }
            }
        }
    }
}

impl Check {
    /// The message of the issue this check raises for `value`, if it does.
    fn fails(&self, value: &Js) -> Option<String> {
        let length = match value {
            Js::Str(text) => Some(text.encode_utf16().count()),
            Js::Arr(items) => Some(items.len()),
            _ => None,
        };
        let number = match value {
            Js::Num(number) => number.as_f64(),
            Js::NonFinite(number) => Some(*number),
            _ => None,
        };
        // `Reflect.ownKeys`: an array counts its length too.
        let properties = match value {
            Js::Obj(entries) => Some(entries.len()),
            Js::Arr(items) => Some(items.len() + 1),
            _ => None,
        };
        let holds = if let Some(hash) = &self.function {
            match functions::filter(hash, value, functions::Closed { rejects: self.rejects.as_deref(), items: self.items.as_deref(), message: self.message.as_deref() }) {
                Outcome::Pass => true,
                Outcome::Message(message) => return Some(message),
                Outcome::Fail => false,
            }
        } else if let Some(max) = self.max_length {
            length.is_some_and(|length| length <= max)
        } else if let Some(min) = self.min_length {
            length.is_some_and(|length| length >= min)
        } else if let Some(max) = self.max_properties {
            properties.is_some_and(|count| count <= max)
        } else if let Some(min) = self.min_properties {
            properties.is_some_and(|count| count >= min)
        } else if self.int {
            number.is_some_and(|number| number.is_finite() && number.fract() == 0.0 && number.abs() <= 9007199254740991.0)
        } else if self.finite {
            number.is_some_and(f64::is_finite)
        } else if self.trimmed {
            matches!(value, Js::Str(text) if trim(text) == text)
        } else if let Some(pattern) = &self.pattern {
            let regex = self.regex.get_or_init(|| Pattern::compile(pattern, self.flags.as_deref().unwrap_or_default()));
            matches!(value, Js::Str(text) if regex.as_ref().expect("a pattern the port can run").is_match(text))
        } else if let (Some(min), Some(max)) = (self.minimum, self.maximum) {
            number.is_some_and(|number| number >= min && number <= max)
        } else if let Some(min) = self.minimum {
            number.is_some_and(|number| number >= min)
        } else if let Some(max) = self.maximum {
            number.is_some_and(|number| number <= max)
        } else if let Some(min) = self.exclusive_minimum {
            number.is_some_and(|number| number > min)
        } else if let Some(max) = self.exclusive_maximum {
            number.is_some_and(|number| number < max)
        } else {
            unreachable!("a check the generator wrote but the port does not know")
        };
        (!holds).then(|| format!("Expected {}", self.expected.as_deref().unwrap_or("<filter>")))
    }
}

pub(crate) fn trim(text: &str) -> &str {
    t3_contracts::trim_wire_string(text)
}

/// A pattern the schemas check: a regular expression, or one led by a negative lookahead (`^(?!(?:a|b)$)`), which the regex
/// crate has no syntax for. Both are matched as the engine would, unanchored unless the pattern anchors itself.
enum Pattern {
    Plain(regex::Regex),
    /// `^(?!not)rest`: neither alternative reads past the start, so the two halves are tested apart.
    Lead { not: regex::Regex, rest: Option<regex::Regex> },
}
impl Pattern {
    fn compile(source: &str, flags: &str) -> Option<Pattern> {
        let Some(after) = source.strip_prefix("^(?!") else { return regex::Regex::new(&translate(source, flags)).ok().map(Pattern::Plain) };
        let mut depth = 1;
        let mut end = None;
        let mut escaped = false;
        for (at, c) in after.char_indices() {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(at);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end?;
        let not = regex::Regex::new(&format!("^{}", translate(&after[..end], flags))).ok()?;
        let rest = &after[end + 1..];
        let rest = if rest.is_empty() { None } else { Some(regex::Regex::new(&format!("^{}", translate(rest, flags))).ok()?) };
        Some(Pattern::Lead { not, rest })
    }
    fn is_match(&self, text: &str) -> bool {
        match self {
            Pattern::Plain(regex) => regex.is_match(text),
            Pattern::Lead { not, rest } => !not.is_match(text) && rest.as_ref().is_none_or(|rest| rest.is_match(text)),
        }
    }
}

/// A JavaScript pattern as the regex crate reads it: `\d`, `\w`, `\s` and `\b` mean the ASCII (or JavaScript's own) sets
/// there, `.` stops at the four line terminators, and `i` is a flag.
fn translate(source: &str, flags: &str) -> String {
    const SPACE: &str = "\\t\\n\\x0B\\x0C\\r \\u{a0}\\u{1680}\\u{2000}-\\u{200a}\\u{2028}\\u{2029}\\u{202f}\\u{205f}\\u{3000}\\u{feff}";
    let mut out = String::new();
    if flags.contains('i') {
        out.push_str("(?i)");
    }
    let (mut in_class, mut chars) = (false, source.chars());
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('d') => out.push_str(if in_class { "0-9" } else { "[0-9]" }),
                Some('D') => out.push_str(if in_class { "[^0-9]" } else { "[^0-9]" }),
                Some('w') => out.push_str(if in_class { "A-Za-z0-9_" } else { "[A-Za-z0-9_]" }),
                Some('W') => out.push_str("[^A-Za-z0-9_]"),
                Some('s') => if in_class { out.push_str(SPACE) } else { out.push_str(&format!("[{SPACE}]")) },
                Some('S') => out.push_str(&format!("[^{SPACE}]")),
                Some('b') if !in_class => out.push_str("(?-u:\\b)"),
                Some('B') => out.push_str("(?-u:\\B)"),
                Some(escaped) => {
                    out.push('\\');
                    out.push(escaped);
                }
                None => out.push('\\'),
            },
            '.' if !in_class => out.push_str("[^\\n\\r\\u{2028}\\u{2029}]"),
            '[' => {
                in_class = true;
                out.push(c);
            }
            ']' => {
                in_class = false;
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// What a union reads of its members before looking at an input, built once: the library's `getCandidateIndex`.
#[derive(Default)]
struct Plan {
    literal_of: HashMap<usize, Value>,
    literals: Vec<(Value, Vec<usize>)>,
    only_literals: bool,
    /// key -> (literal -> members, members that own the key)
    by_sentinel: Vec<(Value, Vec<(Value, Vec<usize>)>, Vec<usize>)>,
    /// A key every discriminated member owns, the one with the most values: it seeds the selection.
    common: Option<usize>,
    /// Members without a sentinel, by the runtime type they accept.
    otherwise: Option<HashMap<String, Vec<usize>>>,
}

impl Plan {
    fn build(node: &Node) -> Plan {
        let mut plan = Plan { only_literals: true, ..Plan::default() };
        let mut sentinel_members = 0;
        for (position, member) in node.members.iter().enumerate() {
            if member.never {
                continue;
            }
            if let Some(literal) = &member.literal {
                plan.literal_of.insert(position, literal.clone());
                match plan.literals.iter_mut().find(|(found, _)| found == literal) {
                    Some((_, members)) => members.push(position),
                    None => plan.literals.push((literal.clone(), vec![position])),
                }
            } else {
                plan.only_literals = false;
            }
            if member.sentinels.is_empty() {
                for ty in &member.types {
                    plan.otherwise.get_or_insert_with(HashMap::new).entry(ty.clone()).or_default().push(position);
                }
            } else {
                sentinel_members += 1;
                for sentinel in &member.sentinels {
                    let entry = match plan.by_sentinel.iter().position(|(key, _, _)| *key == sentinel.key) {
                        Some(found) => &mut plan.by_sentinel[found],
                        None => {
                            plan.by_sentinel.push((sentinel.key.clone(), Vec::new(), Vec::new()));
                            plan.by_sentinel.last_mut().expect("just pushed")
                        }
                    };
                    if !entry.2.contains(&position) {
                        entry.2.push(position);
                    }
                    match entry.1.iter_mut().find(|(found, _)| *found == sentinel.literal) {
                        Some((_, members)) => {
                            if !members.contains(&position) {
                                members.push(position)
                            }
                        }
                        None => entry.1.push((sentinel.literal.clone(), vec![position])),
                    }
                }
            }
        }
        for (position, entry) in plan.by_sentinel.iter().enumerate() {
            if entry.2.len() == sentinel_members && plan.common.is_none_or(|best| entry.1.len() > plan.by_sentinel[best].1.len()) {
                plan.common = Some(position);
            }
        }
        plan
    }

    /// The members of the union worth trying for `input`, in order.
    fn select(&self, input: &Js) -> Vec<usize> {
        let runtime = input.runtime_type();
        let fallback = |ty: &str| self.otherwise.as_ref().and_then(|found| found.get(ty)).cloned().unwrap_or_default();
        // `key in input` and `input[key]` on an object or an array.
        let read = |key: &Value| -> Option<&Js> {
            match (input, key) {
                (Js::Obj(_), Value::String(name)) => input.get(name),
                (Js::Arr(items), Value::Number(index)) => index.as_u64().and_then(|index| items.get(index as usize)),
                _ => None,
            }
        };
        let matching = |by_value: &[(Value, Vec<usize>)], value: &Js| by_value.iter().find(|(literal, _)| value.is_literal(literal)).map(|(_, members)| members.clone());
        let object_like = matches!(input, Js::Obj(_) | Js::Arr(_));
        if self.only_literals && !self.literals.is_empty() {
            self.literals.iter().find(|(literal, _)| input.is_literal(literal)).map(|(_, members)| members.clone()).unwrap_or_default()
        } else if self.by_sentinel.len() == 1 && self.otherwise.is_none() {
            if !object_like {
                return Vec::new();
            }
            match read(&self.by_sentinel[0].0) {
                Some(value) if *value != Js::Undefined => matching(&self.by_sentinel[0].1, value).unwrap_or_default(),
                _ => Vec::new(),
            }
        } else if !self.by_sentinel.is_empty() {
            if !object_like {
                return fallback(runtime);
            }
            let mut selected: Vec<usize> = fallback(runtime);
            let mut direct = None;
            if let Some(position) = self.common {
                if let Some(value) = read(&self.by_sentinel[position].0) {
                    // `input[key]` of a present key, undefined included, selects by that value.
                    match matching(&self.by_sentinel[position].1, value) {
                        Some(members) => selected.extend(members),
                        None => return fallback(runtime),
                    }
                    direct = Some(position);
                }
            }
            if direct.is_none() {
                for entry in &self.by_sentinel {
                    if let Some(value) = read(&entry.0) {
                        if let Some(members) = matching(&entry.1, value) {
                            selected.extend(members);
                        }
                    }
                }
            }
            for (position, entry) in self.by_sentinel.iter().enumerate() {
                if Some(position) == direct {
                    continue;
                }
                if let Some(value) = read(&entry.0) {
                    let matched = matching(&entry.1, value).unwrap_or_default();
                    selected.retain(|member| !entry.2.contains(member) || matched.contains(member));
                }
            }
            selected.sort_unstable();
            selected.dedup();
            selected
        } else {
            fallback(runtime).into_iter().filter(|member| self.literal_of.get(member).is_none_or(|literal| input.is_literal(literal))).collect()
        }
    }
}
