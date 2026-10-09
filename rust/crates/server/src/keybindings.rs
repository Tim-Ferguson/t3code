//! Keybinding rules: parsing, `when` expressions, resolution against the defaults, validation of
//! persisted entries, and the issue texts the original reports for them (port of
//! `packages/shared/src/keybindings.ts` and the pure half of `apps/server/src/keybindings.ts`).
use serde::Serialize;
use serde_json::Value;
use std::{collections::HashSet, sync::OnceLock};
use t3_contracts::{KeybindingCommand, STATIC_KEYBINDING_COMMANDS, trim_wire_string};

pub const MAX_KEYBINDINGS_COUNT: usize = 256;
const MAX_WHEN_EXPRESSION_DEPTH: usize = 64;
const MAX_KEY_LENGTH: usize = 64;
const MAX_WHEN_LENGTH: usize = 256;

/// A rule as persisted: the text the user wrote, trimmed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rule {
    pub key: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Shortcut {
    pub key: String,
    pub meta_key: bool,
    pub ctrl_key: bool,
    pub shift_key: bool,
    pub alt_key: bool,
    pub mod_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum WhenNode {
    Identifier { name: String },
    Not { node: Box<WhenNode> },
    And { left: Box<WhenNode>, right: Box<WhenNode> },
    Or { left: Box<WhenNode>, right: Box<WhenNode> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolved {
    pub command: String,
    pub shortcut: Shortcut,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when_ast: Option<WhenNode>,
}

impl Resolved {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("resolved keybinding")
    }
}

/// The original's default rules, as written (the startup sync persists these exact strings).
/// Generated from `DEFAULT_KEYBINDINGS` by `tools/generate_keybinding_fixtures.mjs`.
pub fn default_rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let rows: Vec<Value> = serde_json::from_str(include_str!("keybinding-defaults.json"))
            .expect("keybinding defaults");
        rows.iter()
            .map(|row| Rule {
                key: row["key"].as_str().expect("default key").into(),
                command: row["command"].as_str().expect("default command").into(),
                when: row["when"].as_str().map(str::to_owned),
            })
            .collect()
    })
}

pub fn default_resolved() -> &'static [Resolved] {
    static RESOLVED: OnceLock<Vec<Resolved>> = OnceLock::new();
    RESOLVED.get_or_init(|| compile_config(default_rules()))
}

fn normalize_key_token(token: &str) -> String {
    match token {
        "space" => " ".into(),
        "esc" => "escape".into(),
        other => other.into(),
    }
}

pub fn parse_shortcut(value: &str) -> Option<Shortcut> {
    let lowered = value.to_lowercase();
    let mut tokens: Vec<String> = lowered
        .split('+')
        .map(|token| trim_wire_string(token).to_owned())
        .collect();
    let mut trailing_empty = 0;
    while tokens.last().is_some_and(String::is_empty) {
        trailing_empty += 1;
        tokens.pop();
    }
    if trailing_empty > 0 {
        tokens.push("+".into());
    }
    if tokens.iter().any(String::is_empty) || tokens.is_empty() {
        return None;
    }
    let (mut key, mut meta, mut ctrl, mut shift, mut alt, mut modifier) =
        (None, false, false, false, false, false);
    for token in &tokens {
        match token.as_str() {
            "cmd" | "meta" => meta = true,
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            "alt" | "option" => alt = true,
            "mod" => modifier = true,
            other => {
                if key.is_some() {
                    return None;
                }
                key = Some(normalize_key_token(other));
            }
        }
    }
    Some(Shortcut {
        key: key?,
        meta_key: meta,
        ctrl_key: ctrl,
        shift_key: shift,
        alt_key: alt,
        mod_key: modifier,
    })
}

enum Token {
    Identifier(String),
    Not,
    And,
    Or,
    LParen,
    RParen,
}

fn tokenize_when(expression: &str) -> Option<Vec<Token>> {
    let chars: Vec<char> = expression.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let current = chars[index];
        // JavaScript `\s`: the same set `String.prototype.trim` strips.
        if trim_wire_string(&current.to_string()).is_empty() {
            index += 1;
            continue;
        }
        let next = chars.get(index + 1).copied();
        match (current, next) {
            ('&', Some('&')) => {
                tokens.push(Token::And);
                index += 2;
                continue;
            }
            ('|', Some('|')) => {
                tokens.push(Token::Or);
                index += 2;
                continue;
            }
            _ => {}
        }
        match current {
            '!' => tokens.push(Token::Not),
            '(' => tokens.push(Token::LParen),
            ')' => tokens.push(Token::RParen),
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut end = index + 1;
                while end < chars.len()
                    && (chars[end].is_ascii_alphanumeric() || matches!(chars[end], '_' | '.' | '-'))
                {
                    end += 1;
                }
                tokens.push(Token::Identifier(chars[index..end].iter().collect()));
                index = end;
                continue;
            }
            _ => return None,
        }
        index += 1;
    }
    Some(tokens)
}

struct WhenParser {
    tokens: Vec<Token>,
    index: usize,
}

impl WhenParser {
    fn primary(&mut self, depth: usize) -> Option<WhenNode> {
        if depth > MAX_WHEN_EXPRESSION_DEPTH {
            return None;
        }
        match self.tokens.get(self.index)? {
            Token::Identifier(name) => {
                let name = name.clone();
                self.index += 1;
                Some(WhenNode::Identifier { name })
            }
            Token::LParen => {
                self.index += 1;
                let inner = self.or(depth + 1)?;
                if !matches!(self.tokens.get(self.index), Some(Token::RParen)) {
                    return None;
                }
                self.index += 1;
                Some(inner)
            }
            _ => None,
        }
    }
    fn unary(&mut self, depth: usize) -> Option<WhenNode> {
        let mut nots = 0;
        while matches!(self.tokens.get(self.index), Some(Token::Not)) {
            self.index += 1;
            nots += 1;
            if nots > MAX_WHEN_EXPRESSION_DEPTH {
                return None;
            }
        }
        let mut node = self.primary(depth)?;
        for _ in 0..nots {
            node = WhenNode::Not { node: Box::new(node) };
        }
        Some(node)
    }
    fn and(&mut self, depth: usize) -> Option<WhenNode> {
        let mut left = self.unary(depth)?;
        while matches!(self.tokens.get(self.index), Some(Token::And)) {
            self.index += 1;
            let right = self.unary(depth)?;
            left = WhenNode::And { left: Box::new(left), right: Box::new(right) };
        }
        Some(left)
    }
    fn or(&mut self, depth: usize) -> Option<WhenNode> {
        let mut left = self.and(depth)?;
        while matches!(self.tokens.get(self.index), Some(Token::Or)) {
            self.index += 1;
            let right = self.and(depth)?;
            left = WhenNode::Or { left: Box::new(left), right: Box::new(right) };
        }
        Some(left)
    }
}

pub fn parse_when(expression: &str) -> Option<WhenNode> {
    let tokens = tokenize_when(expression)?;
    if tokens.is_empty() {
        return None;
    }
    let mut parser = WhenParser { tokens, index: 0 };
    let ast = parser.or(0)?;
    (parser.index == parser.tokens.len()).then_some(ast)
}

pub fn compile_rule(rule: &Rule) -> Option<Resolved> {
    let shortcut = parse_shortcut(&rule.key)?;
    let when_ast = match &rule.when {
        Some(when) => Some(parse_when(when)?),
        None => None,
    };
    Some(Resolved { command: rule.command.clone(), shortcut, when_ast })
}

/// Invalid rules are dropped; at most the last 256 remain.
pub fn compile_config(rules: &[Rule]) -> Vec<Resolved> {
    let compiled: Vec<Resolved> = rules.iter().filter_map(compile_rule).collect();
    let skip = compiled.len().saturating_sub(MAX_KEYBINDINGS_COUNT);
    compiled.into_iter().skip(skip).collect()
}

/// Defaults whose command the user has not bound, then the user's rules; at most the last 256.
pub fn merge_with_defaults(custom: Vec<Resolved>) -> Vec<Resolved> {
    if custom.is_empty() {
        return default_resolved().to_vec();
    }
    let overridden: HashSet<&str> = custom.iter().map(|rule| rule.command.as_str()).collect();
    let mut merged: Vec<Resolved> = default_resolved()
        .iter()
        .filter(|rule| !overridden.contains(rule.command.as_str()))
        .cloned()
        .collect();
    merged.extend(custom);
    let skip = merged.len().saturating_sub(MAX_KEYBINDINGS_COUNT);
    merged.into_iter().skip(skip).collect()
}

/// A shortcut back as a key string; None when it has no unambiguous spelling.
pub fn encode_shortcut(shortcut: &Shortcut) -> Option<String> {
    if shortcut.key.is_empty() || (shortcut.key != "+" && shortcut.key.contains('+')) {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for (set, name) in [
        (shortcut.mod_key, "mod"),
        (shortcut.meta_key, "meta"),
        (shortcut.ctrl_key, "ctrl"),
        (shortcut.alt_key, "alt"),
        (shortcut.shift_key, "shift"),
    ] {
        if set {
            parts.push(name);
        }
    }
    parts.push(if shortcut.key == " " { "space" } else { &shortcut.key });
    Some(parts.join("+"))
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn encode_when(node: &WhenNode) -> String {
    match node {
        WhenNode::Identifier { name } => name.clone(),
        WhenNode::Not { node } => format!("!({})", encode_when(node)),
        WhenNode::And { left, right } => format!("({} && {})", encode_when(left), encode_when(right)),
        WhenNode::Or { left, right } => format!("({} || {})", encode_when(left), encode_when(right)),
    }
}

/// The shortcut and context a rule occupies, for conflict detection; None when it does not parse.
fn shortcut_context(rule: &Rule) -> Option<String> {
    let encoded = encode_shortcut(&parse_shortcut(&rule.key)?)?;
    Some(format!("{encoded}\0{}", rule.when.as_deref().unwrap_or("")))
}

pub fn has_same_shortcut_context(left: &Rule, right: &Rule) -> bool {
    matches!((shortcut_context(left), shortcut_context(right)), (Some(a), Some(b)) if a == b)
}

pub fn is_same_rule(left: &Rule, right: &Rule) -> bool {
    left.command == right.command && left.key == right.key && left.when == right.when
}

// ---- persisted entries ---------------------------------------------------------------------

/// What a malformed entry or file is reported as, in the exact words of the original's schema
/// errors (`SchemaError: ...`).
fn schema_error(detail: &str) -> String {
    format!("SchemaError: {detail}")
}

fn command_list() -> String {
    STATIC_KEYBINDING_COMMANDS
        .iter()
        .map(|command| format!("\"{command}\""))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// A persisted string field: trimmed, 1 to `max` UTF-16 code units. Err is the schema error text.
fn bounded_text(entry: &serde_json::Map<String, Value>, field: &str, max: usize) -> Result<String, String> {
    let at = format!("\n  at [\"{field}\"]");
    let value = entry
        .get(field)
        .ok_or_else(|| schema_error(&format!("Missing key{at}")))?
        .as_str()
        .ok_or_else(|| schema_error(&format!("Expected string{at}")))?;
    let trimmed = trim_wire_string(value);
    match trimmed.encode_utf16().count() {
        0 => Err(schema_error(&format!("Expected a value with a length of at least 1{at}"))),
        n if n > max => Err(schema_error(&format!("Expected a value with a length of at most {max}{at}"))),
        _ => Ok(trimmed.to_owned()),
    }
}

/// Decodes one persisted entry into a rule that also resolves. Err is the issue message the
/// original reports for it.
pub fn decode_entry(entry: &Value) -> Result<Rule, String> {
    let object = entry
        .as_object()
        .ok_or_else(|| schema_error("Expected object"))?;
    let key = bounded_text(object, "key", MAX_KEY_LENGTH)?;
    let command = match object.get("command") {
        None => return Err(schema_error("Missing key\n  at [\"command\"]")),
        Some(Value::String(command)) => {
            if KeybindingCommand::new(command).is_err() {
                return Err(schema_error(&format!(
                    "Expected {}\n  at [\"command\"]\nExpected a string matching template literal parts\n  at [\"command\"]",
                    command_list()
                )));
            }
            command.clone()
        }
        Some(_) => {
            return Err(schema_error(&format!(
                "Expected {} | string\n  at [\"command\"]",
                command_list()
            )));
        }
    };
    let when = match object.get("when") {
        None => None,
        Some(Value::String(_)) => Some(bounded_text(object, "when", MAX_WHEN_LENGTH)?),
        Some(_) => return Err(schema_error("Expected string | undefined\n  at [\"when\"]")),
    };
    let rule = Rule { key, command, when };
    compile_rule(&rule).ok_or_else(|| schema_error("Invalid keybinding rule"))?;
    Ok(rule)
}

/// A rule from an RPC payload that already passed the contract decode: trimmed, so it compares
/// equal to the persisted text.
pub fn rule_from_parts(key: &str, command: &str, when: Option<&str>) -> Rule {
    Rule {
        key: key.to_owned(),
        command: command.to_owned(),
        when: when.map(str::to_owned),
    }
}

#[cfg(test)]
#[path = "keybindings_tests.rs"]
pub(crate) mod tests;
