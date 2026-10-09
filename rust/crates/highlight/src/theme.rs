use serde_json::Value;
use std::{cmp::Ordering, collections::BTreeMap};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Style { pub font: i32, pub foreground: Option<String>, pub background: Option<String> }
impl Style {
    fn inherit(&mut self, next: &Self) {
        if next.font != -1 { self.font = next.font; }
        if next.foreground.is_some() { self.foreground = next.foreground.clone(); }
        if next.background.is_some() { self.background = next.background.clone(); }
    }
}
#[derive(Clone)]
struct Rule { depth: usize, parents: Vec<String>, style: Style }
#[derive(Clone)]
struct Trie { main: Rule, qualified: Vec<Rule>, children: BTreeMap<String, Trie> }
pub struct Theme { pub defaults: Style, root: Trie }
struct Parsed { scope: String, parents: Option<Vec<String>>, index: usize, style: Style }
impl Theme {
    /// Accept Shiki's normalized TextMate settings, retaining the original
    /// partial-style inheritance and selector specificity.
    pub fn new(value: &Value) -> Self {
        let mut parsed = Vec::new();
        if let Some(settings) = value["settings"].as_array() {
            for (index, entry) in settings.iter().enumerate() {
                let Some(style) = entry.get("settings") else { continue; };
                let font = style["fontStyle"].as_str().map_or(-1, |text| text.split(' ').fold(0, |bits, item| {
                    bits | match item { "italic" => 1, "bold" => 2, "underline" => 4, "strikethrough" => 8, _ => 0 }
                }));
                let style = Style { font, foreground: color(&style["foreground"]), background: color(&style["background"]) };
                let scopes: Vec<_> = match &entry["scope"] {
                    Value::String(text) => text.trim_matches(',').split(',').map(str::to_owned).collect(),
                    Value::Array(scopes) => scopes.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
                    _ => vec![String::new()],
                };
                for scope in scopes {
                    let mut segments: Vec<_> = scope.trim_matches(js_space).split(' ').map(str::to_owned).collect();
                    let scope = segments.pop().unwrap_or_default();
                    segments.reverse();
                    parsed.push(Parsed { scope, parents: (!segments.is_empty()).then_some(segments), index, style: style.clone() });
                }
            }
        }
        parsed.sort_by(|a,b| js_cmp(&a.scope,&b.scope).then_with(|| parent_cmp(&a.parents,&b.parents)).then(a.index.cmp(&b.index)));
        let mut defaults = Style { font: 0, foreground: Some("#000000".into()), background: Some("#FFFFFF".into()) };
        let mut root = Trie { main: Rule { depth: 0, parents: Vec::new(), style: Style { font: -1, foreground: None, background: None } },
            qualified: Vec::new(), children: BTreeMap::new() };
        for rule in parsed {
            if rule.scope.is_empty() { defaults.inherit(&rule.style); }
            else { root.insert(0, &rule.scope, rule.parents, rule.style); }
        }
        Self { defaults, root }
    }
    pub fn style(&self, scopes: &[String]) -> Style {
        let mut style = self.defaults.clone();
        for (index, scope) in scopes.iter().enumerate() {
            if let Some(rule) = self.root.match_scope(scope, &scopes[..index]) { style.inherit(&rule.style); }
        }
        style
    }
}
impl Trie {
    fn insert(&mut self, depth: usize, scope: &str, parents: Option<Vec<String>>, style: Style) {
        if !scope.is_empty() {
            let (head, tail) = scope.split_once('.').unwrap_or((scope, ""));
            if !self.children.contains_key(head) {
                self.children.insert(head.into(), Trie { main: self.main.clone(), qualified: self.qualified.clone(), children: BTreeMap::new() });
            }
            self.children.get_mut(head).unwrap().insert(depth+1,tail,parents,style);
        } else if let Some(parents) = parents {
            if let Some(rule) = self.qualified.iter_mut().find(|rule| rule.parents == parents) {
                rule.depth = depth;rule.style.inherit(&style);
            } else {
                let mut inherited = self.main.style.clone();inherited.inherit(&style);
                self.qualified.push(Rule { depth, parents, style: inherited });
            }
        } else { self.main.depth = depth;self.main.style.inherit(&style); }
    }
    fn match_scope(&self, scope: &str, parents: &[String]) -> Option<&Rule> {
        if !scope.is_empty() {
            let (head, tail) = scope.split_once('.').unwrap_or((scope,""));
            if let Some(child) = self.children.get(head) { return child.match_scope(tail,parents); }
        }
        let mut rules: Vec<_> = self.qualified.iter().chain([&self.main]).collect();
        rules.sort_by(|a,b| specificity(a,b));
        rules.into_iter().find(|rule| matches_parents(parents,&rule.parents))
    }
}
fn specificity(a: &Rule, b: &Rule) -> Ordering {
    let depth = b.depth.cmp(&a.depth);
    if depth != Ordering::Equal { return depth; }
    let (mut i,mut j) = (0,0);
    loop {
        if a.parents.get(i).is_some_and(|v| v == ">") { i+=1; }
        if b.parents.get(j).is_some_and(|v| v == ">") { j+=1; }
        let (Some(a),Some(b)) = (a.parents.get(i),b.parents.get(j)) else { break; };
        let comparison = b.encode_utf16().count().cmp(&a.encode_utf16().count());
        if comparison != Ordering::Equal { return comparison; }
        i+=1;j+=1;
    }
    b.parents.len().cmp(&a.parents.len())
}
fn matches_parents(scopes: &[String], patterns: &[String]) -> bool {
    let mut scopes = scopes.iter().rev();
    let mut patterns = patterns.iter();
    while let Some(mut pattern) = patterns.next() {
        let immediate = pattern == ">";
        if immediate { let Some(next) = patterns.next() else { return false; };pattern=next; }
        let mut matched = false;
        for scope in scopes.by_ref() {
            if scope == pattern || scope.strip_prefix(pattern.as_str()).is_some_and(|suffix| suffix.starts_with('.')) {
                matched=true;break;
            }
            if immediate { return false; }
        }
        if !matched { return false; }
    }
    true
}
fn js_cmp(a: &str,b: &str) -> Ordering { a.encode_utf16().cmp(b.encode_utf16()) }
fn parent_cmp(a: &Option<Vec<String>>,b: &Option<Vec<String>>) -> Ordering {
    match (a,b) {
        (None,None) => Ordering::Equal, (None,Some(_)) => Ordering::Less, (Some(_),None) => Ordering::Greater,
        (Some(a),Some(b)) => a.len().cmp(&b.len()).then_with(|| a.iter().zip(b).map(|(a,b)| js_cmp(a,b)).find(|c| *c != Ordering::Equal).unwrap_or(Ordering::Equal)),
    }
}
fn color(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    let digits = text.strip_prefix('#')?;
    if matches!(digits.len(),3|4|6|8) && digits.bytes().all(|b| b.is_ascii_hexdigit()) { Some(text.to_uppercase()) } else { None }
}
fn js_space(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
        | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// TextMate carries standard token type separately from visible theme styles.
/// Equal colors still have a boundary when a string/comment scope ends.
pub fn token_type(scopes: &[String]) -> u8 {
    let mut kind = 0;
    let word_char = |ch: u8| ch.is_ascii_alphanumeric() || ch == b'_';
    for scope in scopes {
        let bytes = scope.as_bytes();
        'search: for (offset, _) in scope.char_indices() {
            for (word, value) in [("comment", 1), ("string", 2), ("regex", 3), ("meta.embedded", 0)] {
                let end = offset + word.len();
                if scope[offset..].starts_with(word)
                    && (offset == 0 || !word_char(bytes[offset - 1]))
                    && (end == bytes.len() || !word_char(bytes[end])) {
                    kind = value;
                    break 'search;
                }
            }
        }
    }
    kind
}
