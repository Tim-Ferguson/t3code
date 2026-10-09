//! Compile grammar data once, retaining recursive rule identity. Includes are
//! resolved against their grammar repository; `$base` remains the outer grammar.
use crate::{regex::Source, scanner::{Error, Input}};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use crate::selector::Selector;

pub type RuleId = usize;
#[derive(Clone, Debug)]
pub struct Capture {
    pub name: Option<String>,
    pub content_name: Option<String>,
    pub retokenize: Option<RuleId>,
}
#[derive(Clone, Debug)]
pub struct Rule {
    pub name: Option<String>,
    pub content_name: Option<String>,
    pub kind: Kind,
}
#[derive(Clone, Debug)]
pub enum Kind {
    Container { patterns: Vec<RuleId>, missing: bool },
    Match { expression: Source, captures: Vec<Option<Capture>> },
    BeginEnd { begin: Source, end: Source, begin_captures: Vec<Option<Capture>>,
        end_captures: Vec<Option<Capture>>, patterns: Vec<RuleId>, missing: bool, end_last: bool },
    BeginWhile { begin: Source, condition: Source, begin_captures: Vec<Option<Capture>>,
        while_captures: Vec<Option<Capture>>, patterns: Vec<RuleId>, missing: bool },
}
impl Kind {
    pub fn patterns(&self) -> &[RuleId] {
        match self {
            Self::Container { patterns, .. } | Self::BeginEnd { patterns, .. }
                | Self::BeginWhile { patterns, .. } => patterns,
            Self::Match { .. } => &[],
        }
    }
    fn missing_empty(&self) -> bool {
        match self {
            Self::Container { patterns, missing } | Self::BeginEnd { patterns, missing, .. }
                | Self::BeginWhile { patterns, missing, .. } => *missing && patterns.is_empty(),
            _ => false,
        }
    }
}
#[derive(Clone)]
struct Raw {
    assignable: bool,
    readable: bool,
    fields: Map<String, Value>,
    patterns: Option<Vec<usize>>,
    repository: BTreeMap<String, usize>,
    captures: BTreeMap<String, Vec<(String, usize)>>,
    injections: Vec<(String,usize)>,
}
#[derive(Default)]
pub struct Registry {
    raw: Vec<Raw>,
    grammars: BTreeMap<String, usize>,
    injection_targets: BTreeMap<String, Vec<String>>,
}
#[derive(Clone)]
pub struct Grammar {
    pub scope: String,
    pub root: RuleId,
    pub rules: Vec<Rule>,
    pub injections: Vec<(Selector,RuleId)>,
}
impl Registry {
    pub fn add(&mut self, mut grammar: Value) -> Result<(), Error> {
        normalize_clone(&mut grammar);
        let mut fields = grammar.as_object().cloned().ok_or_else(|| Error("grammar must be an object".into()))?;
        let scope = fields.get("scopeName").and_then(Value::as_str)
            .ok_or_else(|| Error("grammar scopeName is missing".into()))?.to_owned();
        // Upstream synthesizes a named include-only `$self` from the grammar.
        fields.insert("name".into(), Value::String(scope.clone()));
        let id = self.add_raw(Value::Object(fields))?;
        if let Some(targets) = self.raw[id].fields.get("injectTo").and_then(Value::as_array) {
            for target in targets.iter().filter_map(Value::as_str) {
                self.injection_targets.entry(target.into()).or_default().push(scope.clone());
            }
        }
        self.grammars.insert(scope, id);
        Ok(())
    }
    fn add_raw(&mut self, value: Value) -> Result<usize, Error> {
        // The source walks repository entries lazily. Non-object entries may
        // be unused metadata; reject them only if compiled as an actual rule.
        let assignable=value.is_object()||value.is_array();
        let readable=!value.is_null();
        let fields=match value {Value::Object(fields)=>fields,_=>Map::new()};
        let patterns = fields.get("patterns").and_then(Value::as_array)
            .map(|items| items.iter().cloned().map(|item| self.add_raw(item)).collect::<Result<Vec<_>, _>>()).transpose()?;
        let mut repository = BTreeMap::new();
        if let Some(rules) = fields.get("repository").and_then(Value::as_object) {
            for (name, rule) in rules { repository.insert(name.clone(), self.add_raw(rule.clone())?); }
        }
        let mut captures = BTreeMap::new();
        for key in ["captures", "beginCaptures", "endCaptures", "whileCaptures"] {
            let items=match fields.get(key) {
                Some(Value::Object(items))=>Some(items.iter().map(|(key,value)|(key.clone(),value.clone())).collect::<Vec<_>>()),
                Some(Value::Array(items))=>Some(items.iter().enumerate().map(|(index,value)|(index.to_string(),value.clone())).collect()),
                _=>None,
            };
            if let Some(items) = items {
                let mut capture_ids = Vec::new();
                for (index, capture) in items {
                    // NaN-keyed capture properties never participate in the source
                    // numeric capture array (some pinned grammars contain them).
                    if index != "$vscodeTextmateLocation" && capture_index(&index).is_some() {
                        // Primitive captures expose no name/content/patterns in JS.
                        // Null remains an error because source property access throws.
                        let capture=if !capture.is_object() && !capture.is_array() && !capture.is_null() {
                            Value::Object(Map::new())
                        }else{capture.clone()};
                        capture_ids.push((index.clone(), self.add_raw(capture)?));
                    }
                }
                captures.insert(key.into(), capture_ids);
            }
        }
        let mut injections=Vec::new();
        if let Some(items) = fields.get("injections").and_then(Value::as_object) {
            for (selector, rule) in items { injections.push((selector.clone(),self.add_raw(rule.clone())?)); }
        }
        let id = self.raw.len();
        self.raw.push(Raw { assignable, readable, fields, patterns, repository, captures, injections });
        Ok(id)
    }
    pub fn compile(&self, scope: &str) -> Result<Grammar, Error> {
        let base = *self.grammars.get(scope).ok_or_else(|| Error(format!("Unknown grammar {scope}")))?;
        let mut compiler = Compiler { registry: self, rules: Vec::new(), compiled: BTreeMap::new(), base };
        let context = compiler.context(base);
        let root = compiler.rule(base, &context)?;
        let mut injections=Vec::new();
        for (selector,raw) in &self.raw[base].injections {
            let rule=compiler.rule(*raw,&context)?;
            injections.extend(Selector::parse(selector).into_iter().map(|selector| (selector,rule)));
        }
        if let Some(targets) = self.injection_targets.get(scope) {
            for target in targets {
                let raw=*self.grammars.get(target).ok_or_else(||Error(format!("Missing injection grammar {target}")))?;
                if let Some(selector)=self.raw[raw].fields.get("injectionSelector").and_then(Value::as_str) {
                    let injection_context=compiler.context(raw);
                    let rule=compiler.rule(raw,&injection_context)?;
                    injections.extend(Selector::parse(selector).into_iter().map(|selector|(selector,rule)));
                }
            }
        }
        injections.sort_by_key(|(selector,_)| selector.priority);
        Ok(Grammar { scope: scope.into(), root, rules: compiler.rules, injections })
    }
}
#[derive(Clone)]
struct Context { repository: BTreeMap<String, usize>, own: usize }
struct Compiler<'a> {
    registry: &'a Registry,
    base: usize,
    rules: Vec<Rule>,
    compiled: BTreeMap<usize, RuleId>,
}
impl Compiler<'_> {
    fn context(&self, own: usize) -> Context {
        Context { repository: self.registry.raw[own].repository.clone(), own }
    }
    fn rule(&mut self, raw_id: usize, context: &Context) -> Result<RuleId, Error> {
        if let Some(id) = self.compiled.get(&raw_id) { return Ok(*id); }
        let raw = self.registry.raw[raw_id].clone();
        if !raw.assignable {return Err(Error("grammar rule cannot receive an identity".into()))}
        let name = string(&raw.fields, "name");
        let content_name = string(&raw.fields, "contentName");
        let id = self.rules.len();
        // Assign identity before descendants compile, so `$self` and repository
        // recursion can refer to this rule without cloning or unbounded recursion.
        self.compiled.insert(raw_id, id);
        self.rules.push(Rule { name, content_name, kind: Kind::Container { patterns: Vec::new(), missing: false } });
        let kind = if let Some(expression) = nonempty(&raw.fields, "match") {
            Kind::Match { expression: Source::new(&Input::from_str(&expression)),
                captures: self.captures(&raw, "captures", context)? }
        } else if !raw.fields.contains_key("begin") {
            let mut nested = context.clone();
            nested.repository.extend(raw.repository.clone());
            let (patterns, missing) = if raw.patterns.is_none() {
                if let Some(include) = nonempty(&raw.fields, "include") {
                    let target = self.include(&include, &nested)?;
                    (target.into_iter().collect(), target.is_none())
                } else { (Vec::new(), false) }
            } else { self.patterns(raw.patterns.as_deref().unwrap(), &nested)? };
            Kind::Container { patterns, missing }
        } else {
            let begin = Source::new(&Input::from_str(&string(&raw.fields, "begin").unwrap_or_default()));
            let begin_captures = self.captures(&raw, "beginCaptures", context)?;
            let (patterns, missing) = self.patterns(raw.patterns.as_deref().unwrap_or(&[]), context)?;
            if let Some(condition) = nonempty(&raw.fields, "while") {
                Kind::BeginWhile { begin, condition: Source::new(&Input::from_str(&condition)),
                    begin_captures, while_captures: self.captures(&raw, "whileCaptures", context)?, patterns, missing }
            } else {
                Kind::BeginEnd { begin, end: Source::new(&Input::from_str(&nonempty(&raw.fields, "end").unwrap_or_else(|| "\u{ffff}".into()))),
                    begin_captures, end_captures: self.captures(&raw, "endCaptures", context)?,
                    patterns, missing, end_last: raw.fields.get("applyEndPatternLast").is_some_and(truthy) }
            }
        };
        self.rules[id].kind = kind;
        Ok(id)
    }
    fn include(&mut self, include: &str, context: &Context) -> Result<Option<RuleId>, Error> {
        let (raw_id, context) = if include == "$self" { (Some(context.own), context.clone()) }
        else if include == "$base" { (Some(self.base), context.clone()) }
        else if let Some(name) = include.strip_prefix('#') { (context.repository.get(name).copied(), context.clone()) }
        else {
            let (scope, name) = include.split_once('#').map_or((include, None), |(scope, name)| (scope, Some(name)));
            let Some(&own) = self.registry.grammars.get(scope) else { return Ok(None); };
            let external = self.context(own);
            let target = name.map_or(Some(own), |name| external.repository.get(name).copied());
            (target, external)
        };
        raw_id.map(|raw_id| self.rule(raw_id, &context)).transpose()
    }
    fn patterns(&mut self, raw_ids: &[usize], context: &Context) -> Result<(Vec<RuleId>, bool), Error> {
        let mut patterns = Vec::new();
        for &raw_id in raw_ids {
            let include = nonempty(&self.registry.raw[raw_id].fields, "include");
            let rule = if let Some(include) = include { self.include(&include, context)? }
                else { Some(self.rule(raw_id, context)?) };
            if let Some(rule) = rule {
                if !self.rules[rule].kind.missing_empty() { patterns.push(rule); }
            }
        }
        let missing = patterns.len() != raw_ids.len();
        Ok((patterns, missing))
    }
    fn captures(&mut self, raw: &Raw, key: &str, context: &Context) -> Result<Vec<Option<Capture>>, Error> {
        let Some(entries) = raw.captures.get(key).or_else(|| raw.captures.get("captures")) else { return Ok(Vec::new()); };
        let mut captures = Vec::new();
        for (index, raw_id) in entries {
            // Source parseInt accepts a decimal prefix in capture keys.
            let Some(index) = capture_index(index) else { continue; };
            if index > 1_000_000 { return Err(Error("grammar capture index is too large".into())); }
            captures.resize_with(captures.len().max(index + 1), || None);
            let entry = &self.registry.raw[*raw_id];
            if !entry.readable {return Err(Error("null grammar capture".into()))}
            let (name, content_name, has_patterns) = (string(&entry.fields, "name"), string(&entry.fields, "contentName"), entry.patterns.is_some());
            let retokenize = if has_patterns { Some(self.rule(*raw_id, context)?) } else { None };
            captures[index] = Some(Capture { name, content_name, retokenize });
        }
        Ok(captures)
    }
}
fn string(fields: &Map<String, Value>, key: &str) -> Option<String> {
    fields.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn nonempty(fields: &Map<String, Value>, key: &str) -> Option<String> { string(fields, key).filter(|value| !value.is_empty()) }
fn truthy(value: &Value) -> bool {
    match value { Value::Null => false, Value::Bool(v) => *v, Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(), _ => true }
}

fn capture_index(text:&str)->Option<usize> {
    let text=text.trim_start_matches(|ch|matches!(ch,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'));
    let text=text.strip_prefix('+').unwrap_or(text);
    let digits:String=text.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

// The pinned TextMate clone treats typeof-null as an object and clones it to
// {}. Keep this at the raw-grammar boundary, not at arbitrary rule evaluation.
fn normalize_clone(value:&mut Value) {
    match value {
        Value::Null=>*value=Value::Object(Map::new()),
        Value::Array(items)=>for item in items {normalize_clone(item)},
        Value::Object(fields)=>for value in fields.values_mut() {normalize_clone(value)},
        _=>{},
    }
}
