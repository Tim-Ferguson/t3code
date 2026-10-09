//! Claude Code skills: filesystem discovery for the provider snapshot (`ClaudeSkills.ts`) and the
//! rewrite of a `$skill` mention into the slash command the CLI runs (`ClaudeSkillDispatch.ts`).
//!
//! Claude Code loads skills from `<config dir>/skills` (user scope) and `<cwd>/.claude/skills`
//! (project scope), one directory per skill with a `SKILL.md` carrying YAML frontmatter. The user
//! root wins on name collisions. `.agents/skills` is a Codex location the CLI does not read.
//!
//! The frontmatter is read by a small parser of the subset skills use (top-level `key: value`
//! scalars, quoted strings, and block scalars) rather than a YAML library; a document that is not
//! a mapping, or has an unterminated quote or a tab indent, is malformed and its skill skipped, as
//! the original skips a skill whose YAML does not parse.
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// The CLI's `~` expansion for a user-supplied path.
pub fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    match value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\")) {
        Some(rest) => home.join(rest),
        None => PathBuf::from(value),
    }
}

#[derive(Debug, PartialEq)]
enum Frontmatter {
    Missing,
    Malformed,
    Parsed { description: Option<String>, user_invocation_only: bool, not_user_invocable: bool },
}

/// `parseFrontmatterBoolean`: Claude Code also accepts the YAML 1.1 spellings.
fn frontmatter_boolean(value: &Scalar) -> Option<bool> {
    match value {
        Scalar::Number(number) => (*number == 1.0).then_some(true).or_else(|| (*number == 0.0).then_some(false)),
        Scalar::Bool(value) => Some(*value),
        Scalar::Text(text) => match text.trim().to_lowercase().as_str() {
            "true" | "yes" | "on" | "y" => Some(true),
            "false" | "no" | "off" | "n" => Some(false),
            _ => None,
        },
        Scalar::Other => None,
    }
}

enum Scalar {
    Text(String),
    Bool(bool),
    Number(f64),
    Other,
}

/// The frontmatter block: `---`, the body, `---` (or end), as `FRONTMATTER_PATTERN` reads it.
fn frontmatter_block(contents: &str) -> Option<&str> {
    let rest = contents.strip_prefix("---\r\n").or_else(|| contents.strip_prefix("---\n"))?;
    // Lazy match up to the first closing line.
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            return Some(rest[..offset].trim_end_matches(['\n', '\r']));
        }
        offset += line.len();
    }
    None
}

fn parse_scalar(raw: &str) -> Result<Scalar, ()> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "~" || raw == "null" {
        return Ok(Scalar::Other);
    }
    let first = raw.chars().next().unwrap();
    if first == '"' || first == '\'' {
        let closing = raw[1..].rfind(first).ok_or(())?;
        let inner = &raw[1..1 + closing];
        let tail = raw[1 + closing + 1..].trim();
        if !tail.is_empty() && !tail.starts_with('#') {
            return Err(());
        }
        return Ok(Scalar::Text(if first == '"' { unescape_double(inner) } else { inner.replace("''", "'") }));
    }
    // A plain scalar ends at a ` #` comment.
    let plain = raw.split(" #").next().unwrap_or(raw).trim_end();
    match plain {
        "true" | "True" | "TRUE" => return Ok(Scalar::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Scalar::Bool(false)),
        _ => {}
    }
    if let Ok(number) = plain.parse::<f64>() {
        if plain.chars().all(|character| character.is_ascii_digit() || matches!(character, '.' | '-' | '+' | 'e' | 'E')) {
            return Ok(Scalar::Number(number));
        }
    }
    Ok(Scalar::Text(plain.to_owned()))
}
fn unescape_double(inner: &str) -> String {
    let mut out = String::new();
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// The top-level mapping of a frontmatter block. `Err` when it is not a mapping or cannot be read.
fn parse_mapping(block: &str) -> Result<Vec<(String, Scalar)>, ()> {
    let lines: Vec<&str> = block.lines().collect();
    let mut entries = vec![];
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        // A top-level list or flow collection is an object to the original: it parses, with no fields.
        if entries.is_empty() && (line.starts_with("- ") || line == "-" || line.starts_with('[') || line.starts_with('{')) {
            return Ok(vec![]);
        }
        if line.starts_with(char::is_whitespace) || line.starts_with("- ") {
            return Err(());
        }
        let Some((key, value)) = line.split_once(':') else { return Err(()) };
        if key.is_empty() || key.contains(['"', '\'']) || !(value.is_empty() || value.starts_with([' ', '\t'])) {
            return Err(());
        }
        let key = key.trim().to_owned();
        let value = value.trim();
        // Continuation lines belong to this key: more-indented lines of a block or plain scalar.
        let mut continuation = vec![];
        while index < lines.len() && (lines[index].starts_with(char::is_whitespace) || lines[index].trim().is_empty()) {
            if lines[index].starts_with('\t') {
                return Err(());
            }
            continuation.push(lines[index]);
            index += 1;
        }
        while continuation.last().is_some_and(|line| line.trim().is_empty()) {
            continuation.pop();
        }
        let scalar = if let Some(style) = value.strip_prefix(['|', '>']) {
            let folded = value.starts_with('>');
            let style = style.trim();
            if !style.is_empty() && !style.chars().all(|character| matches!(character, '-' | '+' | '0'..='9')) && !style.starts_with('#') {
                return Err(());
            }
            let indent = continuation.iter().filter(|line| !line.trim().is_empty()).map(|line| line.len() - line.trim_start().len()).min().unwrap_or(0);
            let body: Vec<&str> = continuation.iter().map(|line| if line.len() >= indent { &line[indent..] } else { "" }).collect();
            let text = if folded { fold(&body) } else { body.join("\n") };
            let text = if style.contains('-') { text } else { text + "\n" };
            Scalar::Text(text)
        } else if value.starts_with(['[', '{']) {
            Scalar::Other
        } else if value.is_empty() && !continuation.is_empty() {
            // A nested mapping or list: not a scalar this reader uses.
            Scalar::Other
        } else if !continuation.is_empty() {
            // A plain multi-line scalar folds its lines with spaces.
            let mut parts = vec![value.to_owned()];
            parts.extend(continuation.iter().map(|line| line.trim().to_owned()));
            if value.starts_with(['"', '\'']) {
                parse_scalar(&parts.join(" "))?
            } else {
                Scalar::Text(parts.join(" "))
            }
        } else {
            parse_scalar(value)?
        };
        entries.push((key, scalar));
    }
    Ok(entries)
}
fn fold(lines: &[&str]) -> String {
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push(if line.is_empty() || lines[index - 1].is_empty() { '\n' } else { ' ' });
        }
        out.push_str(line);
    }
    out
}

fn parse_frontmatter(contents: &str) -> Frontmatter {
    let Some(block) = frontmatter_block(contents) else { return Frontmatter::Missing };
    let Ok(entries) = parse_mapping(block) else { return Frontmatter::Malformed };
    let get = |key: &str| entries.iter().rev().find(|(name, _)| name == key).map(|(_, value)| value);
    let description = match get("description") {
        Some(Scalar::Text(text)) => Some(text.trim().to_owned()).filter(|text| !text.is_empty()),
        _ => None,
    };
    Frontmatter::Parsed {
        description,
        user_invocation_only: get("disable-model-invocation").and_then(frontmatter_boolean) == Some(true),
        not_user_invocable: get("user-invocable").and_then(frontmatter_boolean) == Some(false),
    }
}

/// Where an administrator installs the policy file whose settings outrank every user and project
/// one. Absent on almost every machine.
pub fn managed_settings_path(platform: &str, environment: &HashMap<String, String>) -> Option<PathBuf> {
    match platform {
        "darwin" => Some(PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")),
        "win32" => environment.get("PROGRAMDATA").map(|dir| dir.trim()).filter(|dir| !dir.is_empty()).map(|dir| Path::new(dir).join("ClaudeCode").join("managed-settings.json")),
        _ => Some(PathBuf::from("/etc/claude-code/managed-settings.json")),
    }
}
/// `skillOverrideSettingsPaths`: settings files in increasing precedence.
pub fn skill_override_settings_paths(config_dir: &Path, cwd: Option<&Path>, managed: Option<&Path>, repository_root: Option<&Path>) -> Vec<PathBuf> {
    let root = repository_root.filter(|root| Some(*root) != cwd);
    let mut paths = vec![config_dir.join("settings.json")];
    if let Some(cwd) = cwd {
        paths.push(cwd.join(".claude").join("settings.json"));
        paths.push(cwd.join(".claude").join("settings.local.json"));
    }
    if let Some(root) = root {
        paths.push(root.join(".claude").join("settings.local.json"));
    }
    paths.extend(managed.map(Path::to_path_buf));
    paths
}
/// The nearest ancestor of `cwd` (inclusive) holding a `.git` entry.
fn repository_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = std::path::absolute(cwd).ok()?;
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

struct Override {
    enabled: bool,
    user_invocation_only: bool,
}
/// Comments and trailing commas are tolerated in these hand-edited files.
fn lenient_json(text: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str(text) {
        return Some(value);
    }
    let mut out = String::new();
    let characters: Vec<char> = text.chars().collect();
    let (mut index, mut in_string) = (0, false);
    while index < characters.len() {
        let character = characters[index];
        if in_string {
            out.push(character);
            if character == '\\' && index + 1 < characters.len() {
                index += 1;
                out.push(characters[index]);
            } else if character == '"' {
                in_string = false;
            }
        } else if character == '"' {
            in_string = true;
            out.push(character);
        } else if character == '/' && characters.get(index + 1) == Some(&'/') {
            while index < characters.len() && characters[index] != '\n' {
                index += 1;
            }
            continue;
        } else if character == '/' && characters.get(index + 1) == Some(&'*') {
            index += 2;
            while index + 1 < characters.len() && !(characters[index] == '*' && characters[index + 1] == '/') {
                index += 1;
            }
            index += 2;
            continue;
        } else if character == ',' {
            let next = characters[index + 1..].iter().find(|next| !next.is_whitespace());
            if !matches!(next, Some('}') | Some(']')) {
                out.push(character);
            }
        } else {
            out.push(character);
        }
        index += 1;
    }
    serde_json::from_str(&out).ok()
}
/// The four states Claude Code accepts. The CLI validates the whole map: one entry with an unknown
/// value makes it drop every override in that file.
fn read_overrides(paths: &[PathBuf]) -> HashMap<String, Override> {
    let mut overrides = HashMap::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let Some(document) = lenient_json(&text) else { continue };
        let Some(map) = document["skillOverrides"].as_object() else { continue };
        let parsed: Option<Vec<(String, Override)>> = map
            .iter()
            .map(|(name, value)| {
                let state = match value.as_str()? {
                    "off" => Override { enabled: false, user_invocation_only: false },
                    "user-invocable-only" => Override { enabled: true, user_invocation_only: true },
                    "on" | "name-only" => Override { enabled: true, user_invocation_only: false },
                    _ => return None,
                };
                Some((name.clone(), state))
            })
            .collect();
        overrides.extend(parsed.into_iter().flatten());
    }
    overrides
}

/// `resolveClaudeConfigDirPath`: the instance `homePath`, then `CLAUDE_CONFIG_DIR` from the
/// environment (relative to the workspace, as the spawned CLI sees it), then `~/.claude`.
pub fn config_dir_path(home_path: &str, environment: &HashMap<String, String>, cwd: Option<&Path>, home: &Path) -> PathBuf {
    let absolute = |path: PathBuf| std::path::absolute(path.clone()).unwrap_or(path);
    let home_path = home_path.trim();
    if !home_path.is_empty() {
        return absolute(expand_home(home_path, home));
    }
    let from_environment = environment.get("CLAUDE_CONFIG_DIR").map(|value| value.trim()).unwrap_or("");
    if !from_environment.is_empty() {
        return absolute(match cwd {
            Some(cwd) if !Path::new(from_environment).is_absolute() => cwd.join(from_environment),
            _ => PathBuf::from(from_environment),
        });
    }
    home.join(".claude")
}

/// Where discovery looks. `managed` is the administrator policy file, `None` to skip it.
pub struct Roots<'a> {
    pub home_path: &'a str,
    pub environment: &'a HashMap<String, String>,
    pub home: &'a Path,
    pub managed_settings: Option<PathBuf>,
}

/// `discoverClaudeSkills`: best-effort, so a broken skill never degrades the provider snapshot.
/// Roots are listed highest precedence first and the first hit for a name wins.
pub fn discover(roots: &Roots<'_>, cwd: Option<&Path>) -> Vec<Value> {
    let config_dir = config_dir_path(roots.home_path, roots.environment, cwd, roots.home);
    let repository = cwd.and_then(repository_root);
    let overrides = read_overrides(&skill_override_settings_paths(&config_dir, cwd, roots.managed_settings.as_deref(), repository.as_deref()));
    let mut scan = vec![(config_dir.join("skills"), "user")];
    if let Some(cwd) = cwd {
        scan.push((cwd.join(".claude").join("skills"), "project"));
    }
    let mut skills: Vec<(String, Value)> = vec![];
    for (directory, scope) in scan {
        let mut entries: Vec<String> = std::fs::read_dir(&directory).map(|entries| entries.filter_map(|entry| entry.ok()?.file_name().into_string().ok()).collect()).unwrap_or_default();
        entries.sort();
        for entry in entries {
            let path = directory.join(&entry).join("SKILL.md");
            let Ok(contents) = std::fs::read_to_string(&path) else { continue };
            let frontmatter = parse_frontmatter(&contents);
            // Malformed frontmatter means the skill will not load in Claude Code either.
            if frontmatter == Frontmatter::Malformed {
                continue;
            }
            // Claude Code identifies a skill by its directory, not by the frontmatter `name`.
            let name = entry.trim().to_owned();
            if name.is_empty() || skills.iter().any(|(existing, _)| *existing == name) {
                continue;
            }
            let state = overrides.get(&name);
            let (description, invocation_only, not_invocable) = match &frontmatter {
                Frontmatter::Parsed { description, user_invocation_only, not_user_invocable } => (description.clone(), *user_invocation_only, *not_user_invocable),
                _ => (None, false, false),
            };
            let mut skill = Map::new();
            skill.insert("name".into(), json!(name));
            skill.insert("path".into(), json!(path));
            skill.insert("enabled".into(), json!(state.is_none_or(|state| state.enabled)));
            skill.insert("scope".into(), json!(scope));
            if let Some(description) = description {
                skill.insert("description".into(), json!(description));
            }
            if invocation_only || state.is_some_and(|state| state.user_invocation_only) {
                skill.insert("userInvocationOnly".into(), json!(true));
            }
            if not_invocable {
                skill.insert("userInvocable".into(), json!(false));
            }
            skills.push((name, Value::Object(skill)));
        }
    }
    let collator = collator();
    skills.sort_by(|left, right| collator.compare(&left.0, &right.0));
    skills.into_iter().map(|(_, skill)| skill).collect()
}
fn collator() -> icu_collator::CollatorBorrowed<'static> {
    let preferences: icu_collator::CollatorPreferences = crate::workspace_entries::locale_from_environment().into();
    icu_collator::Collator::try_new(preferences, Default::default()).expect("compiled ICU collation data")
}

/// The names a `$skill` mention may dispatch: enabled skills the user can invoke.
pub fn user_invocable_names(skills: &[Value]) -> std::collections::HashSet<String> {
    skills.iter().filter(|skill| skill["enabled"] == true && skill["userInvocable"] != false).filter_map(|skill| skill["name"].as_str().map(str::to_owned)).collect()
}

/// A prompt split around its dispatched skill mention.
#[derive(Debug, PartialEq)]
pub struct Dispatch {
    /// Text before the dispatched mention, `None` when it opens the prompt.
    pub leading_text: Option<String>,
    /// `/name` plus the trailing text, ready to be the message's last text block.
    pub command_text: String,
}

fn name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, ':' | '_' | '-')
}
/// A symbol run that reads as a price or count (`$5`, `$1_000`, `$2k`, `$3e4`) is not a skill.
fn is_numeric_mention(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_digit) {
        return false;
    }
    let mut index = 1;
    while bytes.get(index).is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_') {
        index += 1;
    }
    let ends = |index: usize| rest[index..].chars().next().is_none_or(char::is_whitespace);
    if bytes.get(index).is_some_and(|byte| b"kKmMbBtT".contains(byte)) && ends(index + 1) {
        return true;
    }
    if bytes.get(index).is_some_and(|byte| b"eE".contains(byte)) {
        let mut end = index + 1;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end > index + 1 && ends(end) {
            return true;
        }
    }
    ends(index)
}
struct Mention {
    name: String,
    start: usize,
    end: usize,
}
/// `SKILL_MENTION_PATTERN`: `(^|\s)<currency symbol>name` followed by whitespace or the end.
fn mentions(prompt: &str) -> Vec<Mention> {
    let mut found = vec![];
    let mut position = 0;
    let boundaries: Vec<usize> = prompt.char_indices().map(|(index, _)| index).collect();
    for &index in &boundaries {
        if index < position {
            continue;
        }
        let character = prompt[index..].chars().next().unwrap();
        // `^` matches only at the start of the prompt; `\s` consumes one whitespace character.
        let mut starts = vec![];
        if index == 0 {
            starts.push((0, 0));
        }
        if character.is_whitespace() {
            starts.push((index + character.len_utf8(), character.len_utf8()));
        }
        for (symbol_at, prefix) in starts {
            let Some(symbol) = prompt[symbol_at..].chars().next() else { continue };
            if !is_currency_symbol(symbol) {
                continue;
            }
            let rest = &prompt[symbol_at + symbol.len_utf8()..];
            let run: &str = &rest[..rest.find(|character: char| !name_character(character)).unwrap_or(rest.len())];
            if is_numeric_mention(rest) || !run.chars().any(|character| character.is_ascii_alphabetic()) || !run.chars().next().is_some_and(|character| character.is_ascii_alphanumeric()) {
                continue;
            }
            if rest[run.len()..].chars().next().is_some_and(|next| !next.is_whitespace()) {
                continue;
            }
            let start = symbol_at;
            found.push(Mention { name: run.to_owned(), start, end: symbol_at + symbol.len_utf8() + run.len() });
            position = symbol_at + symbol.len_utf8() + run.len();
            let _ = prefix;
            break;
        }
    }
    found
}
/// `\p{Sc}`
fn is_currency_symbol(character: char) -> bool {
    matches!(character, '$' | '¢' | '£' | '¤' | '¥' | '֏' | '؋' | '৲' | '৳' | '৻' | '૱' | '௹' | '฿' | '៛' | '₠'..='₿' | '꠸' | '﷼' | '﹩' | '＄' | '￠' | '￡' | '￥' | '￦')
}

/// `planClaudeSkillDispatch`: split `prompt` around the last `$skill` mention that names a known
/// skill. `None` when there is nothing to dispatch; mentions of unknown skills stay literal.
pub fn plan_dispatch(prompt: &str, skill_names: &std::collections::HashSet<String>) -> Option<Dispatch> {
    let known: Vec<Mention> = mentions(prompt).into_iter().filter(|mention| skill_names.contains(&mention.name)).collect();
    let last = known.last()?;
    let mut leading = prompt[..last.start].to_owned();
    // Earlier mentions are rewritten to `/name` inline, from the end so offsets hold.
    for mention in known[..known.len() - 1].iter().rev() {
        leading = format!("{}/{}{}", &leading[..mention.start], mention.name, &leading[mention.end..]);
    }
    let leading = leading.trim_end().to_owned();
    Some(Dispatch { leading_text: (!leading.is_empty()).then_some(leading), command_text: format!("/{}{}", last.name, &prompt[last.end..]).trim_end().to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn discovery_and_dispatch_match_the_unchanged_source() {
        let fixtures: Value = serde_json::from_str(include_str!("../tests/fixtures/claude-skills.json")).unwrap();
        for case in fixtures["discovery"].as_array().unwrap() {
            let root = tempfile::tempdir().unwrap();
            let base = std::fs::canonicalize(root.path()).unwrap();
            for (relative, contents) in case["files"].as_object().unwrap() {
                write(&base, relative, contents.as_str().unwrap());
            }
            let environment: HashMap<String, String> = case["environment"].as_object().unwrap().iter().map(|(key, value)| (key.clone(), value.as_str().unwrap().replace("$ROOT", base.to_str().unwrap()))).collect();
            let cwd = base.join(case["cwd"].as_str().unwrap());
            let roots = Roots { home_path: &case["homePath"].as_str().unwrap().replace("$ROOT", base.to_str().unwrap()), environment: &environment, home: &base.join("home"), managed_settings: None };
            let got = discover(&roots, Some(&cwd));
            let want: Vec<Value> = serde_json::from_str(&case["output"].to_string().replace("$ROOT", base.to_str().unwrap())).unwrap();
            assert_eq!(got, want, "{}", case["name"]);
        }
        for case in fixtures["dispatch"].as_array().unwrap() {
            let names: std::collections::HashSet<String> = case["skills"].as_array().unwrap().iter().map(|name| name.as_str().unwrap().to_owned()).collect();
            let got = plan_dispatch(case["prompt"].as_str().unwrap(), &names).map(|dispatch| json!({"leadingText":dispatch.leading_text,"commandText":dispatch.command_text})).unwrap_or(Value::Null);
            let want = case["output"].clone();
            let want = if want.is_null() { want } else { json!({"leadingText":want["leadingText"],"commandText":want["commandText"]}) };
            assert_eq!(got, want, "{}", case["prompt"]);
        }
        for case in fixtures["settingsPaths"].as_array().unwrap() {
            let got = skill_override_settings_paths(Path::new(case["configDir"].as_str().unwrap()), case["cwd"].as_str().map(Path::new), managed_settings_path(case["platform"].as_str().unwrap(), &HashMap::new()).as_deref().filter(|_| true), case["root"].as_str().map(Path::new));
            let got: Vec<&str> = got.iter().map(|path| path.to_str().unwrap()).collect();
            let want: Vec<&str> = case["output"].as_array().unwrap().iter().map(|path| path.as_str().unwrap()).collect();
            assert_eq!(got, want);
        }
    }
}
