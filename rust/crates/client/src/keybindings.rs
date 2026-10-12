//! Keyboard shortcuts: matching a key event against the environment's resolved rules, `when`
//! expressions and their context, labels, and the table a window-level listener consults before it
//! claims an event. Port of `packages/shared/src/keybindings.ts` (matching, parsing) and
//! `apps/web/src/keybindings.ts` (context, precedence, labels), with the decisions about which
//! events are left alone that the original spreads across its handlers gathered in one place.
use std::collections::{BTreeMap, BTreeSet};
use t3_contracts::{KeybindingWhenNode, ResolvedKeybindingRule, ResolvedKeybindingsConfig, trim_wire_string};

pub const MAX_KEYBINDINGS_COUNT: usize = 256;
const MAX_WHEN_EXPRESSION_DEPTH: usize = 64;

/// `isMacPlatform`: where `mod` means Command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Platform {
    Mac,
    #[default]
    Other,
}
impl Platform {
    /// From `navigator.platform`-style text.
    pub fn parse(platform: &str) -> Self {
        let platform = platform.to_lowercase();
        if ["mac", "iphone", "ipad", "ipod"].iter().any(|name| platform.contains(name)) { Self::Mac } else { Self::Other }
    }
    /// The platform this build runs on, for targets that cannot ask a browser.
    pub fn native() -> Self {
        if cfg!(target_os = "macos") { Self::Mac } else { Self::Other }
    }
    pub fn is_mac(self) -> bool {
        self == Self::Mac
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    pub key: String,
    pub meta: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub modifier: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    Identifier(String),
    Not(Box<When>),
    And(Box<When>, Box<When>),
    Or(Box<When>, Box<When>),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub command: String,
    pub shortcut: Shortcut,
    pub when: Option<When>,
}

impl When {
    fn from_wire(node: &KeybindingWhenNode) -> Self {
        match node {
            KeybindingWhenNode::Identifier { name } => Self::Identifier(name.as_str().to_owned()),
            KeybindingWhenNode::Not { node } => Self::Not(Box::new(Self::from_wire(node))),
            KeybindingWhenNode::And { left, right } => Self::And(Box::new(Self::from_wire(left)), Box::new(Self::from_wire(right))),
            KeybindingWhenNode::Or { left, right } => Self::Or(Box::new(Self::from_wire(left)), Box::new(Self::from_wire(right))),
        }
    }
    /// `whenAstToExpression`.
    pub fn expression(&self) -> String {
        fn wrap(node: &When) -> String {
            match node {
                When::Identifier(_) | When::Not(_) => node.expression(),
                _ => format!("({})", node.expression()),
            }
        }
        match self {
            Self::Identifier(name) => name.clone(),
            Self::Not(node) => format!("!{}", wrap(node)),
            Self::And(left, right) => format!("{} && {}", wrap(left), wrap(right)),
            Self::Or(left, right) => format!("{} || {}", wrap(left), wrap(right)),
        }
    }
    pub fn identifiers(&self, into: &mut BTreeSet<String>) {
        match self {
            Self::Identifier(name) => {
                into.insert(name.clone());
            }
            Self::Not(node) => node.identifiers(into),
            Self::And(left, right) | Self::Or(left, right) => {
                left.identifiers(into);
                right.identifiers(into);
            }
        }
    }
    /// Whether the rule names focus in a text field itself, which is what lets it fire while the
    /// user is typing.
    fn names_field_focus(&self) -> bool {
        let mut names = BTreeSet::new();
        self.identifiers(&mut names);
        names.contains("composerFocus") || names.contains("editableFocus")
    }
}

impl Rule {
    pub fn from_wire(rule: &ResolvedKeybindingRule) -> Self {
        Self {
            command: rule.command.as_str().to_owned(),
            shortcut: Shortcut {
                key: rule.shortcut.key.0.as_str().to_owned(),
                meta: rule.shortcut.meta_key,
                ctrl: rule.shortcut.ctrl_key,
                shift: rule.shortcut.shift_key,
                alt: rule.shortcut.alt_key,
                modifier: rule.shortcut.mod_key,
            },
            when: rule.when_ast.as_ref().and_then(Option::as_ref).map(When::from_wire),
        }
    }
    /// The key text and `when` text that spell this rule in `keybindings.json`.
    pub fn spelling(&self) -> (String, String) {
        (shortcut_to_input(&self.shortcut), self.when.as_ref().map(When::expression).unwrap_or_default())
    }
}

pub fn from_wire(config: &ResolvedKeybindingsConfig) -> Vec<Rule> {
    config.0.iter().map(Rule::from_wire).collect()
}

/// `DEFAULT_RESOLVED_KEYBINDINGS`.
pub fn default_rules() -> Vec<Rule> {
    from_wire(&t3_contracts::default_resolved_keybindings())
}

/// `mergeWithDefaultKeybindings`: the defaults of commands the user has not bound, then the
/// user's rules, at most the last 256.
pub fn merge_with_defaults(custom: Vec<Rule>) -> Vec<Rule> {
    if custom.is_empty() {
        return default_rules();
    }
    let overridden: BTreeSet<&str> = custom.iter().map(|rule| rule.command.as_str()).collect();
    let mut merged: Vec<Rule> = default_rules().into_iter().filter(|rule| !overridden.contains(rule.command.as_str())).collect();
    merged.extend(custom);
    let skip = merged.len().saturating_sub(MAX_KEYBINDINGS_COUNT);
    merged.into_iter().skip(skip).collect()
}

// ---- parsing -------------------------------------------------------------------------------

fn normalize_key_token(token: &str) -> String {
    match token {
        "space" => " ".into(),
        "esc" => "escape".into(),
        other => other.into(),
    }
}

/// `parseKeybindingShortcut`.
pub fn parse_shortcut(value: &str) -> Option<Shortcut> {
    let lowered = value.to_lowercase();
    let mut tokens: Vec<String> = lowered.split('+').map(|token| trim_wire_string(token).to_owned()).collect();
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
    let (mut key, mut meta, mut ctrl, mut shift, mut alt, mut modifier) = (None, false, false, false, false, false);
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
    Some(Shortcut { key: key?, meta, ctrl, shift, alt, modifier })
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
        match (current, chars.get(index + 1).copied()) {
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
                while end < chars.len() && (chars[end].is_ascii_alphanumeric() || matches!(chars[end], '_' | '.' | '-')) {
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
    fn primary(&mut self, depth: usize) -> Option<When> {
        if depth > MAX_WHEN_EXPRESSION_DEPTH {
            return None;
        }
        match self.tokens.get(self.index)? {
            Token::Identifier(name) => {
                let name = name.clone();
                self.index += 1;
                Some(When::Identifier(name))
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
    fn unary(&mut self, depth: usize) -> Option<When> {
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
            node = When::Not(Box::new(node));
        }
        Some(node)
    }
    fn and(&mut self, depth: usize) -> Option<When> {
        let mut left = self.unary(depth)?;
        while matches!(self.tokens.get(self.index), Some(Token::And)) {
            self.index += 1;
            let right = self.unary(depth)?;
            left = When::And(Box::new(left), Box::new(right));
        }
        Some(left)
    }
    fn or(&mut self, depth: usize) -> Option<When> {
        let mut left = self.and(depth)?;
        while matches!(self.tokens.get(self.index), Some(Token::Or)) {
            self.index += 1;
            let right = self.and(depth)?;
            left = When::Or(Box::new(left), Box::new(right));
        }
        Some(left)
    }
}
/// `parseKeybindingWhenExpression`.
pub fn parse_when(expression: &str) -> Option<When> {
    let tokens = tokenize_when(expression)?;
    if tokens.is_empty() {
        return None;
    }
    let mut parser = WhenParser { tokens, index: 0 };
    let ast = parser.or(0)?;
    (parser.index == parser.tokens.len()).then_some(ast)
}

/// `shortcutToKeybindingInput`: the key text that parses back to this shortcut.
pub fn shortcut_to_input(shortcut: &Shortcut) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for (set, name) in [(shortcut.modifier, "mod"), (shortcut.meta, "meta"), (shortcut.ctrl, "ctrl"), (shortcut.alt, "alt"), (shortcut.shift, "shift")] {
        if set {
            parts.push(name);
        }
    }
    parts.push(match shortcut.key.as_str() {
        " " => "space",
        "escape" => "esc",
        other => other,
    });
    parts.join("+")
}

// ---- events and context --------------------------------------------------------------------

/// A keydown, as the page reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: String,
    pub code: String,
    pub meta: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub alt_graph: bool,
    pub repeat: bool,
    pub composing: bool,
    pub key_code: u32,
}
impl KeyEvent {
    /// IME composition owns the key: `isComposing`, or the legacy `229` keyCode.
    pub fn is_composing(&self) -> bool {
        self.composing || self.key_code == 229
    }
    /// A chord with none of Control, Command or Alt is the user typing, whatever the key.
    pub fn is_typing(&self) -> bool {
        !self.ctrl && !self.meta && !self.alt
    }
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let flag = |name: &str| value[name].as_bool().unwrap_or(false);
        Some(Self {
            key: value["key"].as_str()?.to_owned(),
            code: value["code"].as_str().unwrap_or("").to_owned(),
            meta: flag("meta"),
            ctrl: flag("ctrl"),
            shift: flag("shift"),
            alt: flag("alt"),
            alt_graph: flag("altGraph"),
            repeat: flag("repeat"),
            composing: flag("composing"),
            key_code: value["keyCode"].as_u64().unwrap_or(0) as u32,
        })
    }
}

/// What the page's focus is, read from the DOM at the moment of the key. Four booleans, so the
/// listener can look a chord up for every combination without waiting for the app.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dom {
    pub terminal_focus: bool,
    pub editable_focus: bool,
    pub composer_focus: bool,
    pub dialog_open: bool,
}
impl Dom {
    pub const COMBINATIONS: u32 = 16;
    pub fn bits(self) -> u32 {
        u32::from(self.terminal_focus) | u32::from(self.editable_focus) << 1 | u32::from(self.composer_focus) << 2 | u32::from(self.dialog_open) << 3
    }
    pub fn from_bits(bits: u32) -> Self {
        Self { terminal_focus: bits & 1 != 0, editable_focus: bits & 2 != 0, composer_focus: bits & 4 != 0, dialog_open: bits & 8 != 0 }
    }
    pub fn from_json(value: &serde_json::Value) -> Self {
        Self::from_bits(value.as_u64().unwrap_or(0) as u32)
    }
}

/// The names a `when` expression can use, and which are true right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    on: BTreeSet<String>,
}
impl Context {
    /// `isWeb` and `isDesktop` describe the build; the rest starts false.
    pub fn base(desktop: bool) -> Self {
        Self::default().with("isWeb", !desktop).with("isDesktop", desktop)
    }
    pub fn with(mut self, name: &str, on: bool) -> Self {
        if on {
            self.on.insert(name.to_owned());
        } else {
            self.on.remove(name);
        }
        self
    }
    pub fn with_dom(self, dom: Dom) -> Self {
        // The browser has the keyboard while one is shown and the focus is not in the composer, a terminal or a dialog (the original reads
        // the focus of its webview, which this page does not have).
        let preview = self.has("previewOpen") && !dom.composer_focus && !dom.terminal_focus && !dom.dialog_open;
        let context = self.with("previewFocus", preview).with("terminalFocus", dom.terminal_focus).with("editableFocus", dom.editable_focus).with("composerFocus", dom.composer_focus).with("dialogOpen", dom.dialog_open);
        // The Usage page's plain letters never take a key from a field or a dialog (`resolveUsageShortcut`).
        if dom.editable_focus || dom.dialog_open { context.with("usagePageOpen", false) } else { context }
    }
    pub fn has(&self, name: &str) -> bool {
        self.on.contains(name)
    }
}

/// Every name the app's contexts define, for the editor's variable list.
pub const CONTEXT_NAMES: &[&str] = &[
    "terminalFocus", "terminalOpen", "isWeb", "isDesktop", "editableFocus", "composerFocus", "dialogOpen", "diffOpen", "previewOpen", "previewFocus", "turnRunning", "commandPaletteOpen", "modelPickerOpen", "usagePageOpen", "true", "false",
];

pub fn evaluate_when(node: &When, context: &Context) -> bool {
    match node {
        When::Identifier(name) => match name.as_str() {
            "true" => true,
            "false" => false,
            name => context.has(name),
        },
        When::Not(node) => !evaluate_when(node, context),
        When::And(left, right) => evaluate_when(left, context) && evaluate_when(right, context),
        When::Or(left, right) => evaluate_when(left, context) || evaluate_when(right, context),
    }
}
fn matches_when(when: Option<&When>, context: &Context) -> bool {
    when.is_none_or(|when| evaluate_when(when, context))
}

// ---- matching ------------------------------------------------------------------------------

fn code_shortcut_key(code: &str) -> Option<&'static str> {
    Some(match code {
        "Backquote" => "`",
        "Backslash" => "\\",
        "BracketLeft" => "[",
        "BracketRight" => "]",
        "Comma" => ",",
        "Digit0" => "0",
        "Digit1" => "1",
        "Digit2" => "2",
        "Digit3" => "3",
        "Digit4" => "4",
        "Digit5" => "5",
        "Digit6" => "6",
        "Digit7" => "7",
        "Digit8" => "8",
        "Digit9" => "9",
        "Equal" => "=",
        "Minus" => "-",
        "Period" => ".",
        "Quote" => "'",
        "Semicolon" => ";",
        "Slash" => "/",
        _ => return None,
    })
}
pub fn normalize_event_key(key: &str) -> String {
    let lowered = key.to_lowercase();
    if lowered == "esc" { "escape".into() } else { lowered }
}
fn is_ascii_letter(key: &str) -> bool {
    key.len() == 1 && key.as_bytes()[0].is_ascii_lowercase()
}
/// `shortcutKeyFromEvent`: a letter by its layout, anything else by where the key is.
pub fn shortcut_key_from_event(key: &str, code: &str) -> String {
    let layout = normalize_event_key(key);
    if is_ascii_letter(&layout) {
        return layout;
    }
    code_shortcut_key(code).map(str::to_owned).unwrap_or(layout)
}
/// `resolveEventKeys`: the names an event's key can be spelled as in a rule.
pub fn resolve_event_keys(event: &KeyEvent) -> Vec<String> {
    let layout = normalize_event_key(&event.key);
    let mut keys = vec![layout.clone()];
    if let Some(letter) = event.code.strip_prefix("Key").filter(|rest| rest.len() == 1 && rest.as_bytes()[0].is_ascii_uppercase()) {
        if !is_ascii_letter(&layout) {
            keys.push(letter.to_lowercase());
        }
    }
    let physical = shortcut_key_from_event(&event.key, &event.code);
    if !keys.contains(&physical) {
        keys.push(physical);
    }
    keys
}
/// The (meta, ctrl) a shortcut expects once `mod` is resolved for the platform.
fn expected_meta_ctrl(shortcut: &Shortcut, platform: Platform) -> (bool, bool) {
    (shortcut.meta || (shortcut.modifier && platform.is_mac()), shortcut.ctrl || (shortcut.modifier && !platform.is_mac()))
}
pub fn matches_modifiers(event: &KeyEvent, shortcut: &Shortcut, platform: Platform) -> bool {
    let (meta, ctrl) = expected_meta_ctrl(shortcut, platform);
    event.meta == meta && event.ctrl == ctrl && event.shift == shortcut.shift && event.alt == shortcut.alt
}
/// AltGraph composes punctuation on non-Mac layouts; those characters are typing, not chords.
fn alt_graph_typing(event: &KeyEvent, platform: Platform) -> bool {
    let alphanumeric = event.key.chars().count() == 1 && event.key.chars().all(|c| c.is_ascii_alphanumeric());
    !platform.is_mac() && event.alt_graph && !alphanumeric
}
pub fn matches_shortcut(event: &KeyEvent, shortcut: &Shortcut, platform: Platform) -> bool {
    if alt_graph_typing(event, platform) || !matches_modifiers(event, shortcut, platform) {
        return false;
    }
    resolve_event_keys(event).contains(&shortcut.key)
}

/// `resolveShortcutCommand`: the last rule whose chord matches and whose `when` holds wins.
pub fn resolve_rule<'a>(event: &KeyEvent, rules: &'a [Rule], platform: Platform, context: &Context) -> Option<&'a Rule> {
    rules.iter().rev().find(|rule| matches_when(rule.when.as_ref(), context) && matches_shortcut(event, &rule.shortcut, platform))
}

/// What the app does with an event: the rule that fires, or nothing, in which case the event
/// belongs to whoever has the focus. Events during IME composition, chords that are really
/// typing in a text field or the terminal, and commands the app cannot carry out are all left
/// alone, including when a rule matched them.
pub fn decide<'a>(event: &KeyEvent, rules: &'a [Rule], platform: Platform, context: &Context, available: impl Fn(&str) -> bool) -> Option<&'a Rule> {
    if event.is_composing() {
        return None;
    }
    let rule = resolve_rule(event, rules, platform, context)?;
    if !available(&rule.command) {
        return None;
    }
    let typing_place = context.has("editableFocus") || context.has("terminalFocus") || context.has("composerFocus");
    if event.is_typing() && typing_place && !rule.when.as_ref().is_some_and(When::names_field_focus) {
        return None;
    }
    Some(rule)
}

// ---- conflicts, effective shortcuts, labels --------------------------------------------------

/// `shortcutConflictKey`.
pub fn conflict_key(shortcut: &Shortcut, platform: Platform) -> String {
    let (meta, ctrl) = expected_meta_ctrl(shortcut, platform);
    [shortcut.key.as_str(), if meta { "meta" } else { "" }, if ctrl { "ctrl" } else { "" }, if shortcut.shift { "shift" } else { "" }, if shortcut.alt { "alt" } else { "" }].join("|")
}
/// `effectiveShortcutsForCommand`: the command's shortcuts that no later rule takes over.
pub fn effective_shortcuts<'a>(rules: &'a [Rule], command: &str, platform: Platform, context: &Context) -> Vec<&'a Shortcut> {
    let mut claimed = BTreeSet::new();
    let mut effective = Vec::new();
    for rule in rules.iter().rev() {
        if !matches_when(rule.when.as_ref(), context) || !claimed.insert(conflict_key(&rule.shortcut, platform)) {
            continue;
        }
        if rule.command == command {
            effective.push(&rule.shortcut);
        }
    }
    effective
}
pub fn format_key_label(key: &str) -> String {
    match key {
        " " => "Space".into(),
        "escape" => "Esc".into(),
        "arrowup" => "Up".into(),
        "arrowdown" => "Down".into(),
        "arrowleft" => "Left".into(),
        "arrowright" => "Right".into(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => {
            let mut chars = key.chars();
            chars.next().map(|first| first.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
        }
    }
}
/// `formatShortcutLabel`: symbols on Mac, `Ctrl+Shift+K` elsewhere.
pub fn format_shortcut_label(shortcut: &Shortcut, platform: Platform) -> String {
    let key = format_key_label(&shortcut.key);
    let (meta, ctrl) = expected_meta_ctrl(shortcut, platform);
    if platform.is_mac() {
        return format!("{}{}{}{}{key}", if ctrl { "\u{2303}" } else { "" }, if shortcut.alt { "\u{2325}" } else { "" }, if shortcut.shift { "\u{21e7}" } else { "" }, if meta { "\u{2318}" } else { "" });
    }
    let mut parts: Vec<&str> = Vec::new();
    if ctrl {
        parts.push("Ctrl");
    }
    if shortcut.alt {
        parts.push("Alt");
    }
    if shortcut.shift {
        parts.push("Shift");
    }
    if meta {
        parts.push("Meta");
    }
    parts.push(&key);
    parts.join("+")
}
/// `shortcutLabelForCommand`: the hint shown beside a command, from its first effective shortcut.
pub fn shortcut_label(rules: &[Rule], command: &str, platform: Platform, context: &Context) -> Option<String> {
    effective_shortcuts(rules, command, platform, context).first().map(|shortcut| format_shortcut_label(shortcut, platform))
}

// ---- recording ---------------------------------------------------------------------------------

fn recordable_key(key: &str) -> Option<String> {
    let normalized = key.to_lowercase();
    if matches!(normalized.as_str(), "meta" | "control" | "ctrl" | "shift" | "alt" | "option") {
        return None;
    }
    let key = match normalized.as_str() {
        " " => "space".to_owned(),
        "escape" => "esc".to_owned(),
        other => other.to_owned(),
    };
    let named = ["arrowup", "arrowdown", "arrowleft", "arrowright", "enter", "tab", "backspace", "delete", "home", "end", "pageup", "pagedown", "space", "esc"];
    let function_key = key.len() >= 2 && key.len() <= 3 && key.starts_with('f') && key[1..].bytes().all(|b| b.is_ascii_digit());
    (key.chars().count() == 1 || function_key || named.contains(&key.as_str())).then_some(key)
}
/// `keybindingFromKeyboardEvent`: a key press as binding text such as `mod+shift+k`. None for a
/// modifier on its own.
pub fn binding_from_event(event: &KeyEvent, platform: Platform) -> Option<String> {
    let key = recordable_key(&shortcut_key_from_event(&event.key, &event.code))?;
    let mut parts: Vec<&str> = Vec::new();
    if platform.is_mac() {
        if event.meta {
            parts.push("mod");
        }
        if event.ctrl {
            parts.push("ctrl");
        }
    } else {
        if event.ctrl {
            parts.push("mod");
        }
        if event.meta {
            parts.push("meta");
        }
    }
    if event.alt {
        parts.push("alt");
    }
    if event.shift {
        parts.push("shift");
    }
    parts.push(&key);
    Some(parts.join("+"))
}

// ---- the listener's table ----------------------------------------------------------------------

/// Which chords the window listener may claim (preventDefault) right now. For every chord some
/// rule names, a bit per combination of DOM focus says whether [`decide`] would fire a command,
/// computed with the same function that later runs it, so a chord the app would leave alone is
/// never swallowed. The listener reads the DOM at the keypress and looks the bit up; nothing here
/// depends on the DOM, so the table only changes when the app's own state does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Claims {
    masks: BTreeMap<String, u32>,
}
fn chord_key(meta: bool, ctrl: bool, shift: bool, alt: bool, key: &str) -> String {
    format!("{}{}{}{}|{key}", u8::from(meta), u8::from(ctrl), u8::from(shift), u8::from(alt))
}
impl Claims {
    pub fn build(rules: &[Rule], platform: Platform, base: &Context, available: impl Fn(&str) -> bool) -> Self {
        let mut masks = BTreeMap::new();
        let mut chords = BTreeSet::new();
        for rule in rules {
            let (meta, ctrl) = expected_meta_ctrl(&rule.shortcut, platform);
            chords.insert((meta, ctrl, rule.shortcut.shift, rule.shortcut.alt, rule.shortcut.key.clone()));
        }
        for (meta, ctrl, shift, alt, key) in chords {
            // The event a rule's own chord produces. Letters carry their layout key; the rest are
            // matched by name, which is how a rule spells them.
            let event = KeyEvent { key: key.clone(), meta, ctrl, shift, alt, ..Default::default() };
            let mut mask = 0;
            for bits in 0..Dom::COMBINATIONS {
                let context = base.clone().with_dom(Dom::from_bits(bits));
                if decide(&event, rules, platform, &context, &available).is_some() {
                    mask |= 1 << bits;
                }
            }
            if mask != 0 {
                masks.insert(chord_key(meta, ctrl, shift, alt, &key), mask);
            }
        }
        Self { masks }
    }
    /// Also claims these plain letters (Shift allowed, no other modifier), where the focus passes `typing_free`. They are not
    /// bindings: the launcher of an empty panel acts on its letters wherever focus is, unless the person is typing or a dialog is up.
    pub fn with_plain_letters(mut self, letters: &[char], typing_free: impl Fn(Dom) -> bool) -> Self {
        let mask = (0..Dom::COMBINATIONS).filter(|bits| typing_free(Dom::from_bits(*bits))).fold(0, |mask, bits| mask | 1 << bits);
        for letter in letters {
            let key = letter.to_ascii_lowercase().to_string();
            for shift in [false, true] {
                *self.masks.entry(chord_key(false, false, shift, false, &key)).or_insert(0) |= mask;
            }
        }
        self
    }
    pub fn is_empty(&self) -> bool {
        self.masks.is_empty()
    }
    pub fn len(&self) -> usize {
        self.masks.len()
    }
    /// Whether an event with this focus is claimed. The page computes the same lookup from the
    /// JSON below; this is the reference it follows.
    pub fn claims(&self, event: &KeyEvent, dom: Dom, platform: Platform) -> bool {
        if event.is_composing() || alt_graph_typing(event, platform) {
            return false;
        }
        resolve_event_keys(event).iter().any(|key| self.masks.get(&chord_key(event.meta, event.ctrl, event.shift, event.alt, key)).is_some_and(|mask| mask >> dom.bits() & 1 == 1))
    }
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.masks).expect("claims table")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(key: &str) -> KeyEvent {
        KeyEvent { key: key.into(), ..Default::default() }
    }
    fn mod_event(key: &str, platform: Platform) -> KeyEvent {
        let mut event = event(key);
        if platform.is_mac() { event.meta = true } else { event.ctrl = true }
        event
    }

    #[test]
    fn mod_is_command_on_mac_and_control_elsewhere() {
        let shortcut = parse_shortcut("mod+k").unwrap();
        assert!(matches_shortcut(&mod_event("k", Platform::Mac), &shortcut, Platform::Mac));
        assert!(!matches_shortcut(&mod_event("k", Platform::Mac), &shortcut, Platform::Other));
        assert!(matches_shortcut(&mod_event("k", Platform::Other), &shortcut, Platform::Other));
        assert!(!matches_shortcut(&mod_event("k", Platform::Other), &shortcut, Platform::Mac));
        // The modifiers must be exact: an extra Shift is a different chord.
        let mut shifted = mod_event("k", Platform::Other);
        shifted.shift = true;
        assert!(!matches_shortcut(&shifted, &shortcut, Platform::Other));
    }

    #[test]
    fn letters_match_by_layout_and_punctuation_by_position() {
        // A Cyrillic layout still reaches mod+j through the physical key.
        let mut cyrillic = KeyEvent { key: "о".into(), code: "KeyJ".into(), ctrl: true, ..Default::default() };
        assert!(matches_shortcut(&cyrillic, &parse_shortcut("mod+j").unwrap(), Platform::Other));
        cyrillic.code = "KeyK".into();
        assert!(!matches_shortcut(&cyrillic, &parse_shortcut("mod+j").unwrap(), Platform::Other));
        // Shift+[ reports "{" but is the bracket key.
        let bracket = KeyEvent { key: "{".into(), code: "BracketLeft".into(), ctrl: true, shift: true, ..Default::default() };
        assert!(matches_shortcut(&bracket, &parse_shortcut("mod+shift+[").unwrap(), Platform::Other));
        let esc = KeyEvent { key: "Esc".into(), ..Default::default() };
        assert!(matches_shortcut(&esc, &parse_shortcut("esc").unwrap(), Platform::Other));
    }

    #[test]
    fn altgraph_punctuation_is_typing_off_mac_only() {
        let event = KeyEvent { key: "@".into(), alt_graph: true, ctrl: true, alt: true, ..Default::default() };
        let shortcut = parse_shortcut("mod+alt+@").unwrap();
        assert!(!matches_shortcut(&event, &shortcut, Platform::Other));
        assert!(matches_shortcut(&KeyEvent { ctrl: false, meta: true, ..event.clone() }, &shortcut, Platform::Mac));
    }

    #[test]
    fn when_expressions_evaluate_over_the_context() {
        let rule = parse_when("composerFocus && !(terminalFocus || dialogOpen)").unwrap();
        assert!(evaluate_when(&rule, &Context::default().with("composerFocus", true)));
        assert!(!evaluate_when(&rule, &Context::default().with("composerFocus", true).with("dialogOpen", true)));
        assert!(!evaluate_when(&rule, &Context::default()));
        assert!(evaluate_when(&parse_when("true").unwrap(), &Context::default()));
        assert!(!evaluate_when(&parse_when("false || unknownName").unwrap(), &Context::default()));
        assert_eq!(rule.expression(), "composerFocus && !(terminalFocus || dialogOpen)");
        assert!(parse_when("a &&").is_none() && parse_when("").is_none());
    }

    #[test]
    fn the_last_matching_rule_wins_and_a_failing_when_falls_through() {
        let rules = merge_with_defaults(vec![]);
        let mut pressed = mod_event("d", Platform::Other);
        pressed.code = "KeyD".into();
        let outside = Context::base(false);
        assert_eq!(resolve_rule(&pressed, &rules, Platform::Other, &outside).unwrap().command, "diff.toggle");
        let inside = outside.clone().with("terminalFocus", true);
        assert_eq!(resolve_rule(&pressed, &rules, Platform::Other, &inside).unwrap().command, "terminal.split");
        // A later user rule beats the default for the same chord.
        let mut custom = rules.clone();
        custom.push(Rule { command: "terminal.toggle".into(), shortcut: parse_shortcut("mod+d").unwrap(), when: None });
        assert_eq!(resolve_rule(&pressed, &custom, Platform::Other, &outside).unwrap().command, "terminal.toggle");
    }

    #[test]
    fn typing_in_a_field_is_left_alone_unless_the_rule_names_the_field() {
        let rules = vec![
            Rule { command: "thread.stop".into(), shortcut: parse_shortcut("c").unwrap(), when: None },
            Rule { command: "composer.focus".into(), shortcut: parse_shortcut("x").unwrap(), when: parse_when("composerFocus") },
            Rule { command: "diff.toggle".into(), shortcut: parse_shortcut("mod+d").unwrap(), when: None },
        ];
        let any = |_: &str| true;
        let typing = Context::base(false).with("editableFocus", true).with("composerFocus", true);
        assert!(decide(&event("c"), &rules, Platform::Other, &typing, any).is_none(), "plain letters are the user's");
        assert!(decide(&event("c"), &rules, Platform::Other, &Context::base(false), any).is_some(), "but fire where nothing takes text");
        assert_eq!(decide(&event("x"), &rules, Platform::Other, &typing, any).unwrap().command, "composer.focus");
        let chord = KeyEvent { key: "d".into(), ctrl: true, ..Default::default() };
        assert!(decide(&chord, &rules, Platform::Other, &typing, any).is_some(), "a Control chord is a command even in a field");
        let terminal = Context::base(false).with("terminalFocus", true);
        assert!(decide(&event("c"), &rules, Platform::Other, &terminal, any).is_none());
        // IME composition and unavailable commands are never claimed.
        let composing = KeyEvent { composing: true, ..chord.clone() };
        assert!(decide(&composing, &rules, Platform::Other, &typing, any).is_none());
        assert!(decide(&KeyEvent { key_code: 229, ..chord.clone() }, &rules, Platform::Other, &typing, any).is_none());
        assert!(decide(&chord, &rules, Platform::Other, &typing, |command| command != "diff.toggle").is_none());
    }

    #[test]
    fn labels_follow_the_platform_and_the_effective_rule() {
        let rules = merge_with_defaults(vec![]);
        let context = Context::base(false);
        assert_eq!(shortcut_label(&rules, "commandPalette.toggle", Platform::Mac, &context).unwrap(), "\u{2318}K");
        assert_eq!(shortcut_label(&rules, "commandPalette.toggle", Platform::Other, &context).unwrap(), "Ctrl+K");
        assert_eq!(shortcut_label(&rules, "thread.next", Platform::Other, &context).unwrap(), "Ctrl+Shift+]");
        assert_eq!(shortcut_label(&rules, "theme.select", Platform::Mac, &context).unwrap(), "\u{2325}\u{2318}A");
        // A later rule taking the chord removes the earlier command's hint.
        let mut taken = rules.clone();
        taken.push(Rule { command: "terminal.toggle".into(), shortcut: parse_shortcut("mod+k").unwrap(), when: None });
        assert!(shortcut_label(&taken, "commandPalette.toggle", Platform::Other, &context).is_none());
        assert_eq!(shortcut_label(&taken, "terminal.toggle", Platform::Other, &context).unwrap(), "Ctrl+K");
    }

    #[test]
    fn recording_spells_a_chord_the_way_the_file_does() {
        let press = |key: &str, code: &str, meta: bool, ctrl: bool, shift: bool, alt: bool| KeyEvent { key: key.into(), code: code.into(), meta, ctrl, shift, alt, ..Default::default() };
        assert_eq!(binding_from_event(&press("K", "KeyK", true, false, true, false), Platform::Mac).unwrap(), "mod+shift+k");
        assert_eq!(binding_from_event(&press("k", "KeyK", false, true, false, false), Platform::Other).unwrap(), "mod+k");
        assert_eq!(binding_from_event(&press("k", "KeyK", false, true, false, false), Platform::Mac).unwrap(), "ctrl+k");
        assert_eq!(binding_from_event(&press("Escape", "Escape", false, false, false, false), Platform::Other).unwrap(), "esc");
        assert_eq!(binding_from_event(&press("{", "BracketLeft", false, true, true, false), Platform::Other).unwrap(), "mod+shift+[");
        assert_eq!(binding_from_event(&press("Shift", "ShiftLeft", false, false, true, false), Platform::Other), None);
        // What it records parses back to the same chord.
        let recorded = binding_from_event(&press("ArrowUp", "ArrowUp", false, true, false, true), Platform::Other).unwrap();
        assert_eq!(shortcut_to_input(&parse_shortcut(&recorded).unwrap()), recorded);
    }

    #[test]
    fn the_claims_table_agrees_with_the_decision_for_every_default_chord_and_focus() {
        let rules = merge_with_defaults(vec![]);
        let base = Context::base(true).with("diffOpen", true);
        let available = |command: &str| matches!(command, "diff.toggle" | "commandPalette.toggle" | "chat.new" | "thread.next" | "thread.jump.1" | "terminal.toggle");
        for platform in [Platform::Mac, Platform::Other] {
            let claims = Claims::build(&rules, platform, &base, available);
            assert!(!claims.is_empty());
            for rule in &rules {
                let shortcut = &rule.shortcut;
                let (meta, ctrl) = expected_meta_ctrl(shortcut, platform);
                let event = KeyEvent { key: shortcut.key.clone(), meta, ctrl, shift: shortcut.shift, alt: shortcut.alt, ..Default::default() };
                for bits in 0..Dom::COMBINATIONS {
                    let dom = Dom::from_bits(bits);
                    let context = base.clone().with_dom(dom);
                    let decided = decide(&event, &rules, platform, &context, available).is_some();
                    assert_eq!(claims.claims(&event, dom, platform), decided, "{} {:?} {dom:?}", rule.command, platform);
                }
            }
            // Chords no rule names, and the same chord while composing, are never claimed.
            assert!(!claims.claims(&event("a"), Dom::default(), platform));
            let composing = KeyEvent { key: "k".into(), meta: platform.is_mac(), ctrl: !platform.is_mac(), composing: true, ..Default::default() };
            assert!(!claims.claims(&composing, Dom::default(), platform));
        }
    }

    #[test]
    fn plain_letters_are_claimed_only_where_no_one_is_typing_and_only_with_shift_or_nothing() {
        let rules = merge_with_defaults(vec![]);
        let base = Context::base(true);
        let claims = Claims::build(&rules, Platform::Other, &base, |_| false).with_plain_letters(&['F', 'D'], |dom| !(dom.terminal_focus || dom.editable_focus || dom.composer_focus || dom.dialog_open));
        assert!(claims.claims(&event("f"), Dom::default(), Platform::Other));
        assert!(claims.claims(&KeyEvent { shift: true, ..event("F") }, Dom::default(), Platform::Other), "Shift+F: the key's case is not the question");
        assert!(!claims.claims(&event("f"), Dom { editable_focus: true, ..Default::default() }, Platform::Other));
        assert!(!claims.claims(&event("f"), Dom { dialog_open: true, ..Default::default() }, Platform::Other));
        assert!(!claims.claims(&event("x"), Dom::default(), Platform::Other), "a letter no row names");
        assert!(!claims.claims(&KeyEvent { ctrl: true, ..event("f") }, Dom::default(), Platform::Other), "another modifier is a chord of its own");
    }
}
