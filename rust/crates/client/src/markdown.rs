//! Code-fence policy from the original ChatMarkdown renderer. The renderer owns
//! local controls independently of a streaming fence's changing text.
use regex::Regex;
use std::sync::LazyLock;

const SPACE: &str =
    r"[\t-\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";
const NOT_SPACE: &str =
    r"[^\t-\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";

pub fn fence_language(class_name: Option<&str>) -> String {
    static LANGUAGE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(&format!(r"(?:^|{SPACE})language-({NOT_SPACE}+)")).unwrap());
    let raw = class_name
        .and_then(|value| LANGUAGE.captures(value))
        .and_then(|capture| capture.get(1))
        .map(|value| value.as_str())
        .unwrap_or("text");
    if raw == "gitignore" { "ini" } else { raw }.to_owned()
}

pub fn fence_title(meta: Option<&str>) -> Option<String> {
    static ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r#"(?i)(?:^|{SPACE})(?:title|file(?:name)?)=(?:"([^"]+)"|'([^']+)'|({NOT_SPACE}+))"#
        ))
        .unwrap()
    });
    static FILE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_@][A-Za-z0-9_@./-]*\.[A-Za-z0-9]+$").unwrap());
    static SEPARATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("{SPACE}+")).unwrap());
    let meta = meta?;
    if let Some(capture) = ATTRIBUTE.captures(meta) {
        if let Some(value) = (1..=3).find_map(|index| capture.get(index)) {
            return Some(value.as_str().to_owned());
        }
    }
    SEPARATOR
        .split(meta)
        .find(|token| FILE.is_match(token))
        .map(str::to_owned)
}

pub fn closed_fence(source: &str) -> bool {
    static OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:`{3,}|~{3,})").unwrap());
    static CLOSE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|\n)[ \t>]*(`{3,}|~{3,})[ \t\r]*$").unwrap());
    let Some(opening) = OPEN.find(source) else {
        return false;
    };
    let Some(closing) = CLOSE.captures(source).and_then(|capture| capture.get(1)) else {
        return false;
    };
    opening.as_str().as_bytes()[0] == closing.as_str().as_bytes()[0]
        && closing.as_str().len() >= opening.as_str().len()
}

pub fn can_run_shell(code: &str, language: &str, streaming: bool, available: bool) -> bool {
    static CONTROL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{Cc}\p{Cf}]").unwrap());
    let command = crate::provider_auth::trim(code);
    available
        && !streaming
        && matches!(
            language,
            "sh" | "bash" | "zsh" | "fish" | "shell" | "powershell" | "pwsh"
        )
        && code.ends_with('\n')
        && !command.is_empty()
        && !command.ends_with('\\')
        && !CONTROL.is_match(code.strip_suffix('\n').unwrap_or(code))
}
