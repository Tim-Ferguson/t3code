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
            Js::Num(_) => "number",
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
    tools: HashMap<String, usize>,
    /// WebSocket RPC methods whose payload the original's decoding is run for (`rpc_admission.rs`).
    #[serde(default)]
    rpc: HashMap<String, usize>,
    functions: HashMap<String, Function>,
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
    #[serde(rename = "fn")]
    function: Option<String>,
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
    regex: OnceLock<Option<regex::Regex>>,
}

pub(crate) fn schemas() -> &'static Schemas {
    static SCHEMAS: OnceLock<Schemas> = OnceLock::new();
    SCHEMAS.get_or_init(|| serde_json::from_str(include_str!("mcp_tool_schemas.json")).expect("source-generated tool schemas"))
}

/// The functions the schemas hold as code, each with the kind the port must implement it as.
#[cfg(test)]
pub(crate) fn function_hashes() -> Vec<(&'static str, &'static str, &'static str)> {
    schemas().functions.iter().map(|(hash, function)| (hash.as_str(), function.kind.as_str(), function.source.as_str())).collect()
}
/// Patterns the schemas check that the regex crate cannot compile; none when the port runs them all.
#[cfg(test)]
pub(crate) fn unrunnable_patterns() -> Vec<String> {
    fn walk(checks: &[Check], bad: &mut Vec<String>) {
        for check in checks {
            if let Some(pattern) = &check.pattern {
                if regex::Regex::new(&translate(pattern, check.flags.as_deref().unwrap_or_default())).is_err() {
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
    /// The original's decoding of a WebSocket RPC payload, for a method listed in the schemas.
    pub(crate) fn decode_rpc(&self, method: &str, input: &Js) -> Option<Result<Js, Issue>> {
        let root = *self.rpc.get(method)?;
        Some(Parser { schemas: self, all: true }.parse(root, Some(input)).map(|decoded| decoded.unwrap_or(Js::Undefined)))
    }
    /// Decodes `input` for `tool`, returning the value its handler receives. `all` reports every
    /// issue, as the toolkit's registration does; otherwise the first.
    pub(crate) fn decode(&self, tool: &str, input: &Js, all: bool) -> Result<Js, Issue> {
        let root = self.tools[tool];
        Ok(Parser { schemas: self, all }.parse(root, Some(input))?.unwrap_or(Js::Undefined))
    }
}

struct Parser<'a> {
    schemas: &'a Schemas,
    all: bool,
}

impl Parser<'_> {
    fn mismatch(&self, node: &Node) -> Issue {
        Issue::Leaf(format!("Expected {}", node.expected))
    }
    /// A missing input is neither a failure nor a value; everything else is parsed through the
    /// encoded side, the transformations, then the node itself and its checks.
    fn parse(&self, id: usize, input: Option<&Js>) -> Parsed {
        let node = &self.schemas.nodes[id];
        let Some(input) = input else { return Ok(None) };
        let mut value = input.clone();
        if let Some(last) = node.encoding.last() {
            value = self.parse(last.to, Some(&value))?.unwrap_or(Js::Undefined);
            for position in (0..node.encoding.len()).rev() {
                if let Some(hash) = &node.encoding[position].function {
                    value = functions::transform(hash, value).map_err(|expected| Issue::Leaf(format!("Expected {expected}")))?;
                }
                if position != 0 {
                    value = self.parse(node.encoding[position - 1].to, Some(&value))?.unwrap_or(Js::Undefined);
                }
            }
        }
        let value = self.local(node, value)?;
        let mut issues = Vec::new();
        self.collect(&node.checks, &value, &mut issues);
        if issues.is_empty() { Ok(Some(value)) } else { Err(Issue::All(issues.into_iter().map(|(issue, _)| issue).collect())) }
    }

    fn local(&self, node: &Node, value: Js) -> Result<Js, Issue> {
        let fits = match node.t {
            Tag::String => matches!(value, Js::Str(_)),
            Tag::Number => matches!(value, Js::Num(_)),
            Tag::Boolean => matches!(value, Js::Bool(_)),
            Tag::Null => value == Js::Null,
            Tag::Undefined => value == Js::Undefined,
            Tag::Literal => value.is_literal(node.literal.as_ref().unwrap_or(&Value::Null)),
            Tag::Unknown => true,
            Tag::Never => false,
            Tag::TemplateLiteral => return self.template(node, value),
            Tag::Objects => return self.objects(node, value),
            Tag::Arrays => return self.arrays(node, value),
            Tag::Union => return self.union(node, value),
        };
        if fits { Ok(value) } else { Err(self.mismatch(node)) }
    }

    /// A prefix, one part and a suffix: the part is read from what lies between them.
    fn template(&self, node: &Node, value: Js) -> Result<Js, Issue> {
        let Js::Str(text) = &value else { return Err(self.mismatch(node)) };
        let Some(between) = text.strip_prefix(node.prefix.as_str()).and_then(|rest| rest.strip_suffix(node.suffix.as_str())) else { return Err(self.mismatch(node)) };
        self.parse(node.middle, Some(&Js::Str(between.to_owned())))?;
        Ok(value)
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
        let candidates = candidates(node, &input);
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
            _ => None,
        };
        let holds = if let Some(hash) = &self.function {
            match functions::filter(hash, value) {
                Outcome::Pass => true,
                Outcome::Message(message) => return Some(message),
                Outcome::Fail => false,
            }
        } else if let Some(max) = self.max_length {
            length.is_some_and(|length| length <= max)
        } else if let Some(min) = self.min_length {
            length.is_some_and(|length| length >= min)
        } else if self.int {
            number.is_some_and(|number| number.is_finite() && number.fract() == 0.0 && number.abs() <= 9007199254740991.0)
        } else if self.finite {
            number.is_some_and(f64::is_finite)
        } else if self.trimmed {
            matches!(value, Js::Str(text) if trim(text) == text)
        } else if let Some(pattern) = &self.pattern {
            let regex = self.regex.get_or_init(|| regex::Regex::new(&translate(pattern, self.flags.as_deref().unwrap_or_default())).ok());
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

/// A JavaScript pattern as the regex crate reads it: `\d` is ASCII only there, and `i` is a flag.
fn translate(source: &str, flags: &str) -> String {
    let mut out = String::new();
    if flags.contains('i') {
        out.push_str("(?i)");
    }
    let (mut in_class, mut chars) = (false, source.chars());
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('d') => out.push_str(if in_class { "0-9" } else { "[0-9]" }),
                Some(escaped @ ('w' | 's' | 'b' | 'D' | 'W' | 'S' | 'B')) => panic!("\\{escaped} differs between JavaScript and the regex crate"),
                Some(escaped) => {
                    out.push('\\');
                    out.push(escaped);
                }
                None => out.push('\\'),
            },
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

/// The members of a union worth trying for `input`, in order: the library's `getCandidateIndex`.
fn candidates(node: &Node, input: &Js) -> Vec<usize> {
    let runtime = input.runtime_type();
    let mut literal_of: HashMap<usize, &Value> = HashMap::new();
    let mut literals: Vec<(&Value, Vec<usize>)> = Vec::new();
    let mut only_literals = true;
    // key -> (literal -> members, members that own the key)
    let mut by_sentinel: Vec<(&Value, Vec<(&Value, Vec<usize>)>, Vec<usize>)> = Vec::new();
    let mut sentinel_members = 0;
    let mut otherwise: Option<HashMap<&str, Vec<usize>>> = None;
    for (position, member) in node.members.iter().enumerate() {
        if member.never {
            continue;
        }
        if let Some(literal) = &member.literal {
            literal_of.insert(position, literal);
            match literals.iter_mut().find(|(found, _)| *found == literal) {
                Some((_, members)) => members.push(position),
                None => literals.push((literal, vec![position])),
            }
        } else {
            only_literals = false;
        }
        if member.sentinels.is_empty() {
            for ty in &member.types {
                otherwise.get_or_insert_with(HashMap::new).entry(ty.as_str()).or_default().push(position);
            }
        } else {
            sentinel_members += 1;
            for sentinel in &member.sentinels {
                let entry = match by_sentinel.iter().position(|(key, _, _)| *key == &sentinel.key) {
                    Some(found) => &mut by_sentinel[found],
                    None => {
                        by_sentinel.push((&sentinel.key, Vec::new(), Vec::new()));
                        by_sentinel.last_mut().expect("just pushed")
                    }
                };
                if !entry.2.contains(&position) {
                    entry.2.push(position);
                }
                match entry.1.iter_mut().find(|(found, _)| *found == &sentinel.literal) {
                    Some((_, members)) => {
                        if !members.contains(&position) {
                            members.push(position)
                        }
                    }
                    None => entry.1.push((&sentinel.literal, vec![position])),
                }
            }
        }
    }
    let fallback = |ty: &str| otherwise.as_ref().and_then(|found| found.get(ty)).cloned().unwrap_or_default();
    // `key in input` and `input[key]` on an object or an array.
    let read = |key: &Value| -> Option<&Js> {
        match (input, key) {
            (Js::Obj(_), Value::String(name)) => input.get(name),
            (Js::Arr(items), Value::Number(index)) => index.as_u64().and_then(|index| items.get(index as usize)),
            _ => None,
        }
    };
    let matching = |by_value: &[(&Value, Vec<usize>)], value: &Js| by_value.iter().find(|(literal, _)| value.is_literal(literal)).map(|(_, members)| members.clone());
    let object_like = matches!(input, Js::Obj(_) | Js::Arr(_));
    if only_literals && !literals.is_empty() {
        literals.iter().find(|(literal, _)| input.is_literal(literal)).map(|(_, members)| members.clone()).unwrap_or_default()
    } else if by_sentinel.len() == 1 && otherwise.is_none() {
        if !object_like {
            return Vec::new();
        }
        match read(by_sentinel[0].0) {
            Some(value) if *value != Js::Undefined => matching(&by_sentinel[0].1, value).unwrap_or_default(),
            _ => Vec::new(),
        }
    } else if !by_sentinel.is_empty() {
        if !object_like {
            return fallback(runtime);
        }
        let mut selected: Vec<usize> = fallback(runtime);
        // A key every discriminated member owns seeds the selection; prefer the one with the most values.
        let mut common: Option<usize> = None;
        for (position, entry) in by_sentinel.iter().enumerate() {
            if entry.2.len() == sentinel_members && common.is_none_or(|best| entry.1.len() > by_sentinel[best].1.len()) {
                common = Some(position);
            }
        }
        let mut direct = None;
        if let Some(position) = common {
            if let Some(value) = read(by_sentinel[position].0) {
                // `input[key]` of a present key, undefined included, selects by that value.
                match matching(&by_sentinel[position].1, value) {
                    Some(members) => selected.extend(members),
                    None => return fallback(runtime),
                }
                direct = Some(position);
            }
        }
        if direct.is_none() {
            for entry in &by_sentinel {
                if let Some(value) = read(entry.0) {
                    if let Some(members) = matching(&entry.1, value) {
                        selected.extend(members);
                    }
                }
            }
        }
        for (position, entry) in by_sentinel.iter().enumerate() {
            if Some(position) == direct {
                continue;
            }
            if let Some(value) = read(entry.0) {
                let matched = matching(&entry.1, value).unwrap_or_default();
                selected.retain(|member| !entry.2.contains(member) || matched.contains(member));
            }
        }
        selected.sort_unstable();
        selected.dedup();
        selected
    } else {
        fallback(runtime).into_iter().filter(|member| literal_of.get(member).is_none_or(|literal| input.is_literal(literal))).collect()
    }
}
