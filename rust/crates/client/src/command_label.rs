//! How a shell command reads in a row: the script inside a plain `sh -c` wrapper, control
//! characters made visible, and the program a command runs (`Ran npm`). Port of
//! `packages/client-runtime/src/work-log/commandLabel.ts`, which is a hand-written scanner rather
//! than a grammar, so it is ported function for function and checked against the original on a
//! corpus (`tests/command_label_parity.rs`). Indices are over `char`s where the original's are
//! over UTF-16 units; nothing in it depends on the difference.
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{LazyLock, OnceLock};

const MAX_COMMAND_SEGMENTS: usize = 64;

fn set(words: &[&'static str]) -> BTreeSet<&'static str> {
    words.iter().copied().collect()
}
static SHELL_PROGRAMS: LazyLock<BTreeSet<&str>> = LazyLock::new(|| set(&["sh", "bash", "zsh", "dash", "ash", "ksh", "fish"]));
static WINDOWS_SHELL_PROGRAMS: LazyLock<BTreeSet<&str>> = LazyLock::new(|| set(&["cmd", "cmd.exe", "powershell", "powershell.exe", "pwsh", "pwsh.exe"]));
static SHELL_OPTIONS_WITH_VALUE: LazyLock<BTreeSet<&str>> = LazyLock::new(|| set(&["-o", "-O", "--rcfile", "--init-file"]));
static SHELL_COMMAND_WRAPPERS: LazyLock<BTreeSet<&str>> = LazyLock::new(|| set(&["builtin", "command", "exec"]));
static SHELL_PRECOMMAND_MODIFIERS: LazyLock<BTreeSet<&str>> = LazyLock::new(|| set(&["nocorrect", "noglob", "time"]));
static POWERSHELL_SETUP_PROGRAMS: LazyLock<BTreeSet<&str>> = LazyLock::new(|| set(&["pop-location", "push-location", "set-location"]));
static POWERSHELL_FLAGS: LazyLock<BTreeSet<&str>> = LazyLock::new(|| set(&["-mta", "-nologo", "-noninteractive", "-noprofile", "-sta"]));
static POWERSHELL_OPTIONS_WITH_VALUE: LazyLock<BTreeSet<&str>> = LazyLock::new(|| set(&["-configurationname", "-executionpolicy", "-inputformat", "-outputformat", "-version", "-windowstyle", "-workingdirectory"]));
static START_PROCESS_FLAGS: LazyLock<BTreeSet<&str>> = LazyLock::new(|| set(&["-confirm", "-debug", "-loaduserprofile", "-nonewwindow", "-passthru", "-usenewenvironment", "-verbose", "-wait", "-whatif"]));
static START_PROCESS_OPTIONS_WITH_VALUE: LazyLock<BTreeSet<&str>> = LazyLock::new(|| {
    set(&["-argumentlist", "-credential", "-environment", "-erroraction", "-errorvariable", "-informationaction", "-informationvariable", "-outbuffer", "-outvariable", "-pipelinevariable", "-progressaction", "-redirectstandarderror", "-redirectstandardinput", "-redirectstandardoutput", "-verb", "-warningaction", "-warningvariable", "-windowstyle", "-workingdirectory"])
});
static SKIPPABLE_SUDO_PROBES: LazyLock<BTreeSet<&str>> = LazyLock::new(|| set(&["[", "[[", "test", "true"]));
const NON_PROGRAM_PREFIX_CHARACTERS: &str = "<>(){}[];|&$`#!%@:";
const NON_PROGRAM_SUFFIX_CHARACTERS: &str = "){]}`";

// Tokens that describe shell syntax or shell-local control flow, not an executable.
static NON_DESCRIPTIVE_SHELL_PROGRAMS: LazyLock<BTreeSet<&str>> = LazyLock::new(|| {
    set(&[
        "!", "#", ".", ":", "[", "[[", "alias", "and", "autoload", "begin", "bg", "bind", "bindkey", "break", "builtin", "caller", "case", "catch", "cd", "command", "compgen", "complete", "compopt", "continue", "coproc", "declare", "dirs", "disown", "do", "done", "elif", "else", "enable", "end", "esac", "eval", "exec", "exit", "export", "false", "fc", "fg", "fi", "finally", "for", "foreach", "function", "getopts", "history", "if", "in", "jobs", "let", "local", "logout", "mapfile", "nocorrect", "noglob", "not", "or", "popd", "pushd", "read", "readarray", "readonly", "repeat", "return", "select", "set", "setopt", "shift", "shopt", "source", "switch", "suspend", "test", "then", "time", "times", "trap", "try", "true", "type", "typeset", "ulimit", "umask", "unalias", "until", "unset", "unsetopt", "wait", "while",
    ])
});
// Unlike setup builtins, these can make later segments part of control flow or unreachable.
static TERMINAL_SHELL_PROGRAMS: LazyLock<BTreeSet<&str>> = LazyLock::new(|| {
    set(&["and", "begin", "break", "case", "catch", "continue", "coproc", "do", "done", "elif", "else", "end", "esac", "eval", "exec", "exit", "false", "fi", "finally", "for", "foreach", "function", "if", "in", "not", "or", "repeat", "return", "select", "switch", "then", "try", "until", "while"])
});

fn re(cell: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(source).expect("command pattern"))
}
/// `\s` of JavaScript, which `trim` also uses.
fn is_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}
fn trim(text: &str) -> &str {
    text.trim_matches(is_space)
}
fn trim_start(text: &str) -> &str {
    text.trim_start_matches(is_space)
}
fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}
fn slice(text: &[char], from: usize, to: usize) -> String {
    text[from.min(text.len())..to.min(text.len()).max(from.min(text.len()))].iter().collect()
}
fn at(text: &[char], index: usize) -> Option<char> {
    text.get(index).copied()
}
fn last_segment(text: &str) -> &str {
    text.rsplit(['\\', '/']).next().unwrap_or(text)
}
fn is_assignment(token: &str) -> bool {
    static CELL: OnceLock<Regex> = OnceLock::new();
    re(&CELL, r"^[A-Za-z_][A-Za-z0-9_]*\+?=").is_match(token)
}

#[derive(Clone, Copy, PartialEq)]
enum Wrapper {
    Env,
    Sudo,
}
fn wrapper_options_with_value(wrapper: Wrapper, option: &str) -> bool {
    match wrapper {
        Wrapper::Env => matches!(option, "-C" | "--chdir" | "-S" | "--split-string" | "-u" | "--unset"),
        Wrapper::Sudo => matches!(option, "-C" | "--close-from" | "-D" | "--chdir" | "-g" | "--group" | "-u" | "--user"),
    }
}
fn wrapper_flag(wrapper: Wrapper, option: &str) -> bool {
    match wrapper {
        Wrapper::Env => matches!(option, "-0" | "--null" | "-i" | "--ignore-environment" | "--debug" | "-v"),
        Wrapper::Sudo => matches!(option, "-A" | "--askpass" | "-b" | "--background" | "-E" | "-H" | "-i" | "-n" | "-S"),
    }
}
#[derive(Clone, Copy, PartialEq)]
enum Context {
    Exec,
    Shell,
}

fn shell_command_argument_index(tokens: &[String], start: usize) -> Option<usize> {
    static CELL: OnceLock<Regex> = OnceLock::new();
    let mut index = start;
    while index < tokens.len() {
        let option = &tokens[index];
        if option == "--" || !option.starts_with('-') {
            return None;
        }
        if SHELL_OPTIONS_WITH_VALUE.contains(option.as_str()) {
            index += 2;
            continue;
        }
        if option == "--command" || re(&CELL, r"^-[a-zA-Z]*c[a-zA-Z]*$").is_match(option) {
            return Some(index + 1);
        }
        index += 1;
    }
    None
}

fn tokenize_shell_command(command: &str) -> Option<Vec<String>> {
    static WINDOWS_PATH: OnceLock<Regex> = OnceLock::new();
    let input = chars(trim(command));
    let mut tokens = vec![];
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let (mut escaping, mut in_backticks, mut token_started) = (false, false, false);
    let (mut substitution_depth, mut parameter_depth) = (0usize, 0usize);
    let mut index = 0;
    while index < input.len() {
        let character = input[index];
        if escaping {
            current.push(character);
            escaping = false;
            token_started = true;
            index += 1;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            let next = at(&input, index + 1);
            let is_windows_path = quote.is_none() && re(&WINDOWS_PATH, r"^(?:[A-Za-z]:|\.{1,2})(?:\\[^\s]*)?$").is_match(&current);
            if (quote == Some('"') || is_windows_path) && next.is_some_and(|next| !matches!(next, '"' | '\\' | '$' | '`' | '\n')) {
                current.push(character);
                token_started = true;
                index += 1;
                continue;
            }
            escaping = true;
            token_started = true;
            index += 1;
            continue;
        }
        if in_backticks {
            current.push(character);
            if character == '`' {
                in_backticks = false;
            }
            token_started = true;
            index += 1;
            continue;
        }
        if let Some(open) = quote {
            if character == open {
                quote = None;
            } else {
                current.push(character);
            }
            token_started = true;
            index += 1;
            continue;
        }
        if character == '`' {
            current.push(character);
            in_backticks = true;
            token_started = true;
            index += 1;
            continue;
        }
        if character == '$' && at(&input, index + 1) == Some('{') {
            current.push_str("${");
            parameter_depth += 1;
            token_started = true;
            index += 2;
            continue;
        }
        if character == '{' && parameter_depth > 0 {
            current.push(character);
            parameter_depth += 1;
            token_started = true;
            index += 1;
            continue;
        }
        if character == '}' && parameter_depth > 0 {
            current.push(character);
            parameter_depth -= 1;
            token_started = true;
            index += 1;
            continue;
        }
        if character == '$' && at(&input, index + 1) == Some('(') {
            current.push_str("$(");
            substitution_depth += 1;
            token_started = true;
            index += 2;
            continue;
        }
        if character == '(' {
            current.push(character);
            substitution_depth += 1;
            token_started = true;
            index += 1;
            continue;
        }
        if character == ')' && substitution_depth > 0 {
            current.push(character);
            substitution_depth -= 1;
            token_started = true;
            index += 1;
            continue;
        }
        if character == '"' || character == '\'' {
            quote = Some(character);
            token_started = true;
            index += 1;
            continue;
        }
        if is_space(character) {
            if substitution_depth > 0 || parameter_depth > 0 {
                current.push(character);
                token_started = true;
                index += 1;
                continue;
            }
            if token_started {
                tokens.push(std::mem::take(&mut current));
                token_started = false;
            }
            index += 1;
            continue;
        }
        current.push(character);
        token_started = true;
        index += 1;
    }
    if quote.is_some() || escaping || in_backticks || substitution_depth > 0 || parameter_depth > 0 {
        return None;
    }
    if token_started {
        tokens.push(current);
    }
    Some(tokens)
}

struct Split {
    first: String,
    remaining: Option<String>,
    separator: Option<String>,
}
struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
}

fn without_comments(command: &[char], end: usize, comments: &[(usize, usize)]) -> String {
    let mut result = String::new();
    let mut cursor = 0;
    for &(start, stop) in comments {
        if start >= end {
            break;
        }
        result.push_str(&slice(command, cursor, start));
        cursor = stop.min(end);
    }
    result + &slice(command, cursor, end)
}

fn read_heredoc_delimiter(command: &[char], start: usize, strip_tabs: bool) -> Option<(Heredoc, usize)> {
    let mut index = start;
    while matches!(at(command, index), Some(' ' | '\t')) {
        index += 1;
    }
    let mut delimiter = String::new();
    let mut quote: Option<char> = None;
    let mut escaping = false;
    while index < command.len() {
        let character = command[index];
        if escaping {
            delimiter.push(character);
            escaping = false;
            index += 1;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaping = true;
            index += 1;
            continue;
        }
        if let Some(open) = quote {
            if character == open {
                quote = None;
            } else {
                delimiter.push(character);
            }
            index += 1;
            continue;
        }
        if character == '"' || character == '\'' {
            quote = Some(character);
            index += 1;
            continue;
        }
        if is_space(character) || ";&|<>()".contains(character) {
            break;
        }
        delimiter.push(character);
        index += 1;
    }
    if delimiter.is_empty() || quote.is_some() || escaping {
        return None;
    }
    Some((Heredoc { delimiter, strip_tabs }, index))
}

fn command_after_heredocs(command: &[char], start: usize, heredocs: &[Heredoc]) -> Option<String> {
    let mut cursor = start;
    for heredoc in heredocs {
        let mut found = false;
        while cursor <= command.len() {
            let newline = command[cursor.min(command.len())..].iter().position(|c| *c == '\n').map(|offset| cursor + offset);
            let line_end = newline.unwrap_or(command.len());
            let line = slice(command, cursor, line_end);
            let line = line.strip_suffix('\r').unwrap_or(&line).to_owned();
            let comparable = if heredoc.strip_tabs { line.trim_start_matches('\t').to_owned() } else { line };
            cursor = newline.map_or(command.len(), |newline| newline + 1);
            if comparable == heredoc.delimiter {
                found = true;
                break;
            }
            if newline.is_none() {
                break;
            }
        }
        if !found {
            return None;
        }
    }
    Some(trim(&slice(command, cursor, command.len())).to_owned()).filter(|rest| !rest.is_empty())
}

fn split_first_shell_command(source: &str) -> Split {
    let command = chars(source);
    let mut quote: Option<char> = None;
    let mut here_string: Option<char> = None;
    let (mut escaping, mut in_backticks, mut in_comment) = (false, false, false);
    let (mut substitution_depth, mut parameter_depth) = (0usize, 0usize);
    let mut heredocs: Vec<Heredoc> = vec![];
    let mut comments: Vec<(usize, usize)> = vec![];
    let mut comment_start = 0;
    let mut separator_before: Option<(usize, usize)> = None;
    let mut index = 0;
    while index < command.len() {
        let character = command[index];
        if let Some(open) = here_string {
            if character == open && at(&command, index + 1) == Some('@') && (index == 0 || command[index - 1] == '\n') {
                here_string = None;
                index += 1;
            }
            index += 1;
            continue;
        }
        if in_comment {
            if character != '\n' {
                index += 1;
                continue;
            }
            in_comment = false;
            comments.push((comment_start, index));
        }
        if escaping {
            escaping = false;
            index += 1;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaping = true;
            index += 1;
            continue;
        }
        if in_backticks {
            if character == '`' {
                in_backticks = false;
            }
            index += 1;
            continue;
        }
        if let Some(open) = quote {
            if character == open {
                quote = None;
            }
            index += 1;
            continue;
        }
        if character == '@' && matches!(at(&command, index + 1), Some('"' | '\'')) && (at(&command, index + 2) == Some('\n') || (at(&command, index + 2) == Some('\r') && at(&command, index + 3) == Some('\n'))) {
            here_string = at(&command, index + 1);
            index += 2;
            continue;
        }
        if character == '"' || character == '\'' {
            quote = Some(character);
            index += 1;
            continue;
        }
        if character == '`' {
            in_backticks = true;
            index += 1;
            continue;
        }
        if character == '#' && (index == 0 || is_space(command[index - 1]) || ";&|(".contains(command[index - 1])) {
            in_comment = true;
            comment_start = index;
            index += 1;
            continue;
        }
        if character == '$' && at(&command, index + 1) == Some('{') {
            parameter_depth += 1;
            index += 2;
            continue;
        }
        if character == '{' && parameter_depth > 0 {
            parameter_depth += 1;
            index += 1;
            continue;
        }
        if character == '}' && parameter_depth > 0 {
            parameter_depth -= 1;
            index += 1;
            continue;
        }
        if character == '(' {
            substitution_depth += 1;
            index += 1;
            continue;
        }
        if character == ')' && substitution_depth > 0 {
            substitution_depth -= 1;
            index += 1;
            continue;
        }
        if substitution_depth > 0 || parameter_depth > 0 {
            index += 1;
            continue;
        }
        if character == '<' && at(&command, index + 1) == Some('<') && at(&command, index + 2) != Some('<') {
            let strip_tabs = at(&command, index + 2) == Some('-');
            match read_heredoc_delimiter(&command, index + if strip_tabs { 3 } else { 2 }, strip_tabs) {
                None => return Split { first: trim(source).to_owned(), remaining: None, separator: None },
                Some((heredoc, end)) => {
                    heredocs.push(heredoc);
                    index = end;
                    continue;
                }
            }
        }
        let next = at(&command, index + 1);
        let is_double = (character == '&' && next == Some('&')) || (character == '|' && matches!(next, Some('|' | '&')));
        let previous = if index > 0 { Some(command[index - 1]) } else { None };
        let is_redirection_ampersand = character == '&' && (matches!(previous, Some('>' | '<')) || next == Some('>'));
        if (!is_double && !";&|\n".contains(character)) || is_redirection_ampersand {
            index += 1;
            continue;
        }
        if character == '\n' && !heredocs.is_empty() {
            let separator = separator_before;
            let first = trim_start(&without_comments(&command, separator.map_or(index, |(at, _)| at), &comments)).to_owned();
            let before = separator.map(|(at, length)| trim(&slice(&command, at + length, index)).to_owned()).unwrap_or_default();
            let after = command_after_heredocs(&command, index + 1, &heredocs);
            let remaining = [Some(before), after].into_iter().flatten().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n");
            return Split { first, remaining: Some(remaining).filter(|rest| !rest.is_empty()), separator: Some(separator.map_or_else(|| "\n".to_owned(), |(at, length)| slice(&command, at, at + length))) };
        }
        if !heredocs.is_empty() {
            if separator_before.is_none() {
                separator_before = Some((index, if is_double { 2 } else { 1 }));
            }
            if is_double {
                index += 1;
            }
            index += 1;
            continue;
        }
        let first = trim_start(&without_comments(&command, index, &comments)).to_owned();
        let mut next_index = index + if is_double { 2 } else { 1 };
        while at(&command, next_index).is_some_and(is_space) {
            next_index += 1;
        }
        let rest = trim(&slice(&command, next_index, command.len())).to_owned();
        return Split { first, remaining: Some(rest).filter(|rest| !rest.is_empty()), separator: Some(if is_double { slice(&command, index, index + 2) } else { character.to_string() }) };
    }
    if in_comment {
        comments.push((comment_start, command.len()));
    }
    Split { first: trim(&without_comments(&command, command.len(), &comments)).to_owned(), remaining: None, separator: None }
}

fn without_leading_comments(command: &str) -> Option<String> {
    let mut rest = trim_start(command).to_owned();
    while rest.starts_with('#') {
        let newline = rest.find('\n')?;
        rest = trim_start(&rest[newline + 1..]).to_owned();
    }
    Some(rest).filter(|rest| !rest.is_empty())
}

fn without_line_continuations(command: &str) -> String {
    let command = chars(command);
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut escaping = false;
    let mut index = 0;
    while index < command.len() {
        let character = command[index];
        if escaping {
            out.push(character);
            escaping = false;
            index += 1;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            if at(&command, index + 1) == Some('\n') {
                index += 2;
                continue;
            }
            if at(&command, index + 1) == Some('\r') && at(&command, index + 2) == Some('\n') {
                index += 3;
                continue;
            }
            out.push(character);
            escaping = true;
            index += 1;
            continue;
        }
        match quote {
            Some(open) => {
                if character == open {
                    quote = None;
                }
            }
            None => {
                if character == '"' || character == '\'' {
                    quote = Some(character);
                }
            }
        }
        out.push(character);
        index += 1;
    }
    out
}

/// The index after a redirection at `index`, past the end of the tokens when its target is missing.
fn index_after_redirection(tokens: &[String], index: usize) -> Option<usize> {
    static OPEN: OnceLock<Regex> = OnceLock::new();
    static REDIRECT: OnceLock<Regex> = OnceLock::new();
    let token = tokens.get(index).filter(|token| !token.is_empty())?;
    if re(&OPEN, r"^[<>]\(").is_match(token) {
        return None;
    }
    let found = re(&REDIRECT, r"^(?:(?:(?:\d+|\*|\{[A-Za-z_][A-Za-z0-9_]*\})?(?:<<<|<<-|<<|<>|>>|>\||<&|>&|<|>))|&>>|&>)([^\n\r\u{2028}\u{2029}]*)$").captures(token)?;
    if !found[1].is_empty() {
        return Some(index + 1);
    }
    Some(if tokens.get(index + 1).is_none() { tokens.len() + 1 } else { index + 2 })
}

fn serialize_tokens(tokens: &[String]) -> String {
    tokens.iter().map(|token| format!("'{}'", token.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ")
}

fn transparent_wrapper_index(wrapper: &str, tokens: &[String], index: usize) -> Option<usize> {
    static SCRIPT: OnceLock<Regex> = OnceLock::new();
    static ARCH: OnceLock<Regex> = OnceLock::new();
    static TIMEOUT: OnceLock<Regex> = OnceLock::new();
    let token = |offset: usize| tokens.get(index + offset);
    match wrapper {
        "bundle" => (token(1).map(String::as_str) == Some("exec") && token(2).is_some()).then_some(index + 2),
        "nohup" => {
            let mut target = index + 1;
            if tokens.get(target).map(String::as_str) == Some("--") {
                target += 1;
            }
            tokens.get(target).filter(|target| !target.is_empty() && !target.starts_with('-')).map(|_| target)
        }
        // BSD `script` takes an output file before the optional command; requiring an option and both operands avoids guessing about a plain `script file`.
        "script" => (re(&SCRIPT, r"^-[adkpqr]+$").is_match(token(1).map_or("", String::as_str)) && token(3).is_some()).then_some(index + 3),
        "arch" => {
            if re(&ARCH, r"^-(?:arm64|arm64e|i386|x86_64)$").is_match(token(1).map_or("", String::as_str)) {
                return token(2).is_some().then_some(index + 2);
            }
            (token(1).map(String::as_str) == Some("-arch") && token(3).is_some()).then_some(index + 3)
        }
        "timeout" | "gtimeout" => (re(&TIMEOUT, r"^(?:\d+(?:\.\d*)?|\.\d+)[smhd]?$").is_match(token(1).map_or("", String::as_str)) && token(2).is_some()).then_some(index + 2),
        _ => None,
    }
}

fn static_program_name(value: &str) -> Option<String> {
    let value = trim(value);
    if value.is_empty() {
        return None;
    }
    // `scheme:` not followed by a slash is a URL or a drive-less token, not a program.
    if let Some(colon) = value.find(':') {
        let scheme = &value[..colon];
        let mut scheme_chars = scheme.chars();
        if scheme_chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && scheme_chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) && !matches!(value[colon + 1..].chars().next(), Some('/' | '\\')) {
            return None;
        }
    }
    let program = last_segment(value);
    let has_separator = value.contains(['\\', '/']);
    if program.is_empty() || (program.chars().any(is_space) && !has_separator) || program.chars().next().is_some_and(|c| NON_PROGRAM_PREFIX_CHARACTERS.contains(c)) || program.chars().last().is_some_and(|c| NON_PROGRAM_SUFFIX_CHARACTERS.contains(c)) {
        return None;
    }
    Some(program.to_owned())
}

fn leading_powershell_literal(command: &str) -> Option<String> {
    let input = chars(trim_start(command));
    let quote = input.first().copied();
    if quote != Some('"') && quote != Some('\'') {
        let word: String = input.iter().take_while(|c| !is_space(**c)).collect();
        return static_program_name(&word);
    }
    let quote = quote?;
    let mut value = String::new();
    let mut index = 1;
    while index < input.len() {
        let character = input[index];
        if character == '`' && at(&input, index + 1).is_some() {
            value.push(input[index + 1]);
            index += 2;
            continue;
        }
        if character == quote {
            if quote == '\'' && at(&input, index + 1) == Some('\'') {
                value.push('\'');
                index += 2;
                continue;
            }
            return static_program_name(&value);
        }
        value.push(character);
        index += 1;
    }
    None
}

/// `undefined` (not a call operator) is `None`; a call operator whose target is unreadable is `Some(None)`.
fn powershell_call_operator_program(command: &str) -> Option<Option<String>> {
    static CELL: OnceLock<Regex> = OnceLock::new();
    let found = re(&CELL, r"^\s*&\s+((?s).*)$").captures(command)?;
    Some(leading_powershell_literal(&found[1]))
}

fn powershell_assignment_program(command: &str, depth: usize, remaining: Option<&str>, segments_remaining: usize) -> Option<Option<String>> {
    static ASSIGNMENT: OnceLock<Regex> = OnceLock::new();
    static COLLECTION: OnceLock<Regex> = OnceLock::new();
    static DIRECT: OnceLock<Regex> = OnceLock::new();
    let found = re(&ASSIGNMENT, r"(?i)^\s*\$(?:(env|global|local|script):)?[A-Za-z_][A-Za-z0-9_]*\s*=\s*((?s).*)$").captures(command)?;
    let next = || remaining.filter(|rest| !rest.is_empty()).and_then(|rest| program_name_internal(rest, depth, Context::Shell, segments_remaining.saturating_sub(1)));
    // Environment assignments are setup: their right-hand side is a value, so prefer the next segment.
    if found.get(1).is_some_and(|scope| scope.as_str().eq_ignore_ascii_case("env")) {
        return Some(next());
    }
    let value = trim(&found[2]).to_owned();
    // The POSIX-oriented splitter does not balance PowerShell arrays or hashtables.
    if re(&COLLECTION, r"(?i)^(?:\[ordered\]\s*)?@\s*[{(]").is_match(&value) {
        return Some(None);
    }
    if let Some(called) = powershell_call_operator_program(&value) {
        return Some(called);
    }
    let direct = re(&DIRECT, r"^(?:@?\(\s*)?([A-Za-z][A-Za-z0-9_.-]*)(?-u:\b)").captures(&value).map(|found| found[1].to_owned());
    if let Some(direct) = direct.filter(|direct| !NON_DESCRIPTIVE_SHELL_PROGRAMS.contains(direct.to_lowercase().as_str())) {
        let parsed = program_name_internal(&value, depth + 1, Context::Shell, segments_remaining);
        return Some(parsed.or(Some(direct)));
    }
    Some(next())
}

fn windows_shell_payload_program(shell: &str, tokens: &[String], start: usize, depth: usize, remaining: Option<&str>, separator: Option<&str>, segments_remaining: usize) -> Option<Option<String>> {
    let parse_payload = |payload: Option<&String>| -> Option<String> {
        let payload = payload.filter(|payload| !payload.is_empty())?;
        let command = match (remaining.filter(|rest| !rest.is_empty()), separator.filter(|separator| !separator.is_empty())) {
            (Some(rest), Some(separator)) => format!("{payload} {separator} {rest}"),
            _ => payload.clone(),
        };
        program_name_internal(&command, depth + 1, Context::Shell, segments_remaining)
    };
    if shell == "cmd" || shell == "cmd.exe" {
        for index in start..tokens.len() {
            let option = tokens[index].to_lowercase();
            if option != "/c" && option != "/k" {
                continue;
            }
            return Some(parse_payload(tokens.get(index + 1)));
        }
        return None;
    }
    let mut index = start;
    while index < tokens.len() {
        let option = tokens[index].to_lowercase();
        if option == "-command" || option == "-c" {
            return Some(parse_payload(tokens.get(index + 1)));
        }
        if option == "-file" || option == "-f" {
            return Some(static_program_name(tokens.get(index + 1).map_or("", String::as_str)));
        }
        if option == "-encodedcommand" || option == "-enc" || option == "-e" {
            return Some(None);
        }
        if POWERSHELL_OPTIONS_WITH_VALUE.contains(option.as_str()) {
            index += 2;
            continue;
        }
        if POWERSHELL_FLAGS.contains(option.as_str()) {
            index += 1;
            continue;
        }
        if !option.starts_with('-') {
            return Some(static_program_name(&tokens[index]));
        }
        index += 1;
    }
    None
}

fn start_process_program(tokens: &[String], start: usize) -> Option<String> {
    let mut index = start;
    while index < tokens.len() {
        let token = &tokens[index];
        let option = token.to_lowercase();
        if option == "-filepath" {
            return static_program_name(tokens.get(index + 1).map_or("", String::as_str));
        }
        if START_PROCESS_FLAGS.contains(option.as_str()) {
            index += 1;
            continue;
        }
        if START_PROCESS_OPTIONS_WITH_VALUE.contains(option.as_str()) {
            tokens.get(index + 1)?;
            index += 2;
            continue;
        }
        if token.starts_with('-') {
            return None;
        }
        return static_program_name(token);
    }
    None
}

/// `NAME=value` where the value is a literal program: the name and the program, or None for a value that names none.
fn literal_assignment_program(token: &str) -> Option<(String, Option<String>)> {
    static CELL: OnceLock<Regex> = OnceLock::new();
    let found = re(&CELL, r"^([A-Za-z_][A-Za-z0-9_]*)=((?s).*)$").captures(token)?;
    let name = found[1].to_owned();
    let mut value = trim(&found[2]).to_owned();
    if value.is_empty() || value.contains(['$', '`']) {
        return Some((name, None));
    }
    if value.starts_with('(') && value.ends_with(')') && value.chars().count() >= 2 {
        let inner: String = value.chars().skip(1).take(value.chars().count() - 2).collect();
        value = tokenize_shell_command(&inner).and_then(|tokens| tokens.into_iter().next()).unwrap_or_default();
    } else if value.chars().any(is_space) && !value.contains(['\\', '/']) {
        return Some((name, None));
    }
    Some((name, static_program_name(&value)))
}

fn referenced_alias(token: &str) -> Option<String> {
    static CELL: OnceLock<Regex> = OnceLock::new();
    let found = re(&CELL, r"^\$(?:([A-Za-z_][A-Za-z0-9_]*)|\{([A-Za-z_][A-Za-z0-9_]*)(?:\[@\])?\})$").captures(token)?;
    found.get(1).or(found.get(2)).map(|name| name.as_str().to_owned())
}

/// Recovers only literal aliases an earlier top-level segment declared (`SSH=(ssh ...)`,
/// `TOOL=/path/to/tool`), without evaluating expansions.
fn literal_alias_program(command: &str) -> Option<String> {
    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    let mut remaining = Some(command.to_owned());
    let mut control_flow_depth = 0usize;
    let mut segment = 0;
    while let Some(rest) = remaining.clone().filter(|rest| !rest.is_empty()) {
        if segment >= 64 {
            break;
        }
        segment += 1;
        let without_comments = without_leading_comments(&rest)?;
        let split = split_first_shell_command(&without_comments);
        let tokens = tokenize_shell_command(&without_line_continuations(&split.first))?;
        let mut command_index = if matches!(tokens.first().map(String::as_str), Some("do" | "then")) { 1 } else { 0 };
        while command_index < tokens.len() {
            if let Some(after) = index_after_redirection(&tokens, command_index).filter(|after| *after <= tokens.len()) {
                command_index = after;
                continue;
            }
            if is_assignment(&tokens[command_index]) {
                command_index += 1;
                continue;
            }
            break;
        }
        if let Some(program) = referenced_alias(tokens.get(command_index).map_or("", String::as_str)).and_then(|name| aliases.get(&name)).filter(|program| !program.is_empty()) {
            return Some(program.clone());
        }
        let leading = tokens.first().map(String::as_str);
        if matches!(leading, Some("fi" | "done" | "esac")) {
            control_flow_depth = control_flow_depth.saturating_sub(1);
        }
        if matches!(leading, Some("if" | "for" | "while" | "until" | "select" | "case")) {
            control_flow_depth += 1;
        }
        if control_flow_depth == 0 && leading == Some("unset") {
            for name in &tokens[1..] {
                aliases.remove(name);
            }
        }
        let assignment_start = if leading == Some("export") { 1 } else { 0 };
        let assignments: Vec<Option<(String, Option<String>)>> = tokens[assignment_start.min(tokens.len())..].iter().map(|token| literal_assignment_program(token)).collect();
        if control_flow_depth == 0 && !assignments.is_empty() && assignments.iter().all(Option::is_some) {
            for (name, program) in assignments.into_iter().flatten() {
                match program.filter(|program| !program.is_empty()) {
                    Some(program) => {
                        aliases.insert(name, program);
                    }
                    None => {
                        aliases.remove(&name);
                    }
                }
            }
        }
        remaining = split.remaining;
    }
    None
}

fn wrapped_shell_command_program(wrapper: &str, tokens: &[String], start: usize, depth: usize, remaining: Option<&str>, segments_remaining: usize) -> Option<String> {
    static EXEC_A: OnceLock<Regex> = OnceLock::new();
    static EXEC_FLAGS: OnceLock<Regex> = OnceLock::new();
    let mut index = start;
    if wrapper == "command" {
        while index < tokens.len() {
            let option = &tokens[index];
            if option == "--" {
                index += 1;
                break;
            }
            if !option.starts_with('-') || option == "-" {
                break;
            }
            if option != "-p" {
                return None;
            }
            index += 1;
        }
    } else if wrapper == "builtin" {
        if tokens.get(index).map(String::as_str) == Some("--") {
            index += 1;
        } else if tokens.get(index).is_some_and(|token| token.starts_with('-')) {
            return None;
        }
    } else if wrapper == "exec" {
        while index < tokens.len() {
            let option = &tokens[index];
            if option == "--" {
                index += 1;
                break;
            }
            if option == "-a" {
                tokens.get(index + 1)?;
                index += 2;
                continue;
            }
            if re(&EXEC_A, r"^-a.+").is_match(option) || re(&EXEC_FLAGS, r"^-[cl]+$").is_match(option) {
                index += 1;
                continue;
            }
            if option.starts_with('-') && option != "-" {
                return None;
            }
            break;
        }
    }
    let wrapped = tokens.get(index..).unwrap_or(&[]);
    if wrapped.is_empty() {
        return None;
    }
    let mut target_index = 0;
    while target_index < wrapped.len() {
        match index_after_redirection(wrapped, target_index) {
            Some(after) if after <= wrapped.len() => target_index = after,
            _ => break,
        }
    }
    let target = wrapped.get(target_index).filter(|target| !target.is_empty());
    if target.is_some_and(|target| is_assignment(target)) {
        return None;
    }
    let program = program_name_internal(&serialize_tokens(wrapped), depth + 1, if wrapper == "exec" { Context::Exec } else { Context::Shell }, segments_remaining);
    if program.is_some() {
        return program;
    }
    if wrapper != "exec" {
        if let Some(target) = target {
            if *target == target.to_lowercase() && NON_DESCRIPTIVE_SHELL_PROGRAMS.contains(target.as_str()) && !TERMINAL_SHELL_PROGRAMS.contains(target.as_str()) {
                if let Some(rest) = remaining.filter(|rest| !rest.is_empty()) {
                    return program_name_internal(rest, depth, Context::Shell, segments_remaining.saturating_sub(1));
                }
            }
        }
    }
    None
}

fn parse_program_name(command: &str, depth: usize, context: Context, segments_remaining: usize) -> Option<String> {
    static BLOCK: OnceLock<Regex> = OnceLock::new();
    static FUNCTION: OnceLock<Regex> = OnceLock::new();
    static HERE_STRING: OnceLock<Regex> = OnceLock::new();
    static WINDOWS_PATH: OnceLock<Regex> = OnceLock::new();
    static SHORT_OPTION: OnceLock<Regex> = OnceLock::new();
    static FUNCTION_TOKEN: OnceLock<Regex> = OnceLock::new();
    static EXE: OnceLock<Regex> = OnceLock::new();
    if depth >= 8 || segments_remaining == 0 {
        return None;
    }
    let without_comments = without_leading_comments(command)?;
    if re(&BLOCK, r"(?i)^(?:catch|finally|for|foreach|function|if|param|switch|try|while)\s*[{(]").is_match(&without_comments) {
        return None;
    }
    if re(&FUNCTION, r"^[A-Za-z_][A-Za-z0-9_]*\s*\(\s*\)\s*\{").is_match(&without_comments) {
        return None;
    }
    // `&&` and `||` inside `[[ ... ]]` are not top-level separators; keep the label conservative.
    if without_comments.starts_with("[[") {
        return None;
    }
    let split = split_first_shell_command(&without_comments);
    let remaining = split.remaining.as_deref().filter(|rest| !rest.is_empty());
    let next = |context: Context| remaining.and_then(|rest| program_name_internal(rest, depth, context, segments_remaining.saturating_sub(1)));
    if re(&HERE_STRING, r#"^@["'](?:\r?\n)"#).is_match(trim_start(&split.first)) {
        return next(Context::Shell);
    }
    if let Some(assignment) = powershell_assignment_program(&split.first, depth, remaining, segments_remaining) {
        return assignment;
    }
    if let Some(found) = re(&WINDOWS_PATH, r"(?i)^\s*((?:\.{1,2}|%[A-Za-z_][A-Za-z0-9_]*%|\$env:[A-Za-z_][A-Za-z0-9_]*)\\\S+)").captures(&split.first) {
        return static_program_name(&found[1]);
    }
    let tokens = tokenize_shell_command(&without_line_continuations(&split.first))?;
    let first_character = trim_start(&split.first).chars().next();
    if tokens.len() == 1 && matches!(first_character, Some('"' | '\'')) && tokens[0].chars().any(|c| is_space(c) || matches!(c, '(' | ')' | '=')) && !tokens[0].contains(['\\', '/']) {
        return next(context);
    }
    let mut index = 0;
    let mut wrapper: Option<Wrapper> = None;
    let mut execution_context = context;
    let (mut saw_assignment, mut saw_redirection) = (false, false);
    while index < tokens.len() {
        let token = &tokens[index];
        if token.is_empty() {
            return None;
        }
        if let Some(after) = index_after_redirection(&tokens, index) {
            if after > tokens.len() {
                return None;
            }
            saw_redirection = true;
            index = after;
            continue;
        }
        if is_assignment(token) {
            saw_assignment = true;
            index += 1;
            continue;
        }
        if execution_context == Context::Shell && token == ":" {
            return next(execution_context);
        }
        let first = token.chars().next().unwrap_or(' ');
        if NON_PROGRAM_PREFIX_CHARACTERS.contains(first) && !(token.starts_with('$') && token.contains('/')) {
            return if execution_context == Context::Shell && token.starts_with('[') { next(execution_context) } else { None };
        }
        let token_program = last_segment(token);
        let is_unqualified = token == token_program;
        if token_program == "env" || token_program == "sudo" {
            wrapper = Some(if token_program == "env" { Wrapper::Env } else { Wrapper::Sudo });
            execution_context = Context::Exec;
            index += 1;
            continue;
        }
        if wrapper.is_some() && token == "--" {
            wrapper = None;
            index += 1;
            continue;
        }
        if let Some(active) = wrapper.filter(|_| token.starts_with('-')) {
            if active == Wrapper::Env && (token == "-S" || token == "--split-string") {
                return tokens.get(index + 1).filter(|command| !command.is_empty()).and_then(|command| program_name_internal(command, depth + 1, execution_context, segments_remaining));
            }
            if active == Wrapper::Env && token.starts_with("--split-string=") {
                return program_name_internal(&token["--split-string=".len()..], depth + 1, execution_context, segments_remaining);
            }
            if wrapper_options_with_value(active, token) {
                tokens.get(index + 1)?;
                index += 2;
                continue;
            }
            if wrapper_flag(active, token) {
                index += 1;
                continue;
            }
            if let Some(equals) = token.find('=') {
                if token.starts_with("--") && equals > 2 {
                    if !wrapper_options_with_value(active, &token[..equals]) {
                        return None;
                    }
                    index += 1;
                    continue;
                }
            }
            if re(&SHORT_OPTION, r"^-[A-Za-z].+").is_match(token) && !token.starts_with("--") {
                let mut consumes_next = false;
                let option_count = token.chars().count();
                for (option_index, option) in token.chars().skip(1).enumerate() {
                    let short = format!("-{option}");
                    if wrapper_options_with_value(active, &short) {
                        consumes_next = option_index == option_count - 2;
                        break;
                    }
                    if !wrapper_flag(active, &short) {
                        return None;
                    }
                }
                if consumes_next && tokens.get(index + 1).is_none() {
                    return None;
                }
                index += if consumes_next { 2 } else { 1 };
                continue;
            }
            return None;
        }
        if !token_program.is_empty() && SHELL_PROGRAMS.contains(re(&EXE, r"(?i)\.exe$").replace(token_program, "").to_lowercase().as_str()) {
            if let Some(script_index) = shell_command_argument_index(&tokens, index + 1) {
                return tokens.get(script_index).filter(|script| !script.is_empty()).and_then(|script| program_name_internal(script, depth + 1, Context::Shell, segments_remaining));
            }
        }
        let lower = token_program.to_lowercase();
        if !lower.is_empty() && WINDOWS_SHELL_PROGRAMS.contains(lower.as_str()) {
            if let Some(payload) = windows_shell_payload_program(&lower, &tokens, index + 1, depth, remaining, split.separator.as_deref(), segments_remaining) {
                return payload;
            }
        }
        if lower == "start-process" {
            if let Some(started) = start_process_program(&tokens, index + 1) {
                return Some(started);
            }
        }
        if execution_context == Context::Shell && is_unqualified && !token_program.is_empty() && SHELL_PRECOMMAND_MODIFIERS.contains(token_program) {
            index += 1;
            if token_program == "time" && tokens.get(index).map(String::as_str) == Some("-p") {
                index += 1;
            }
            if tokens.get(index).is_some_and(|token| token.starts_with('-')) {
                return None;
            }
            continue;
        }
        if is_unqualified && !token_program.is_empty() {
            if let Some(target) = transparent_wrapper_index(token_program, &tokens, index) {
                if let Some(wrapped) = program_name_internal(&serialize_tokens(&tokens[target..]), depth + 1, Context::Exec, segments_remaining) {
                    return Some(wrapped);
                }
            }
        }
        if is_unqualified && !token_program.is_empty() && SHELL_COMMAND_WRAPPERS.contains(token_program) {
            return wrapped_shell_command_program(token_program, &tokens, index + 1, depth, remaining, segments_remaining);
        }
        if (execution_context == Context::Shell || (wrapper == Some(Wrapper::Sudo) && !token_program.is_empty() && SKIPPABLE_SUDO_PROBES.contains(token_program)))
            && is_unqualified
            && !token_program.is_empty()
            && NON_DESCRIPTIVE_SHELL_PROGRAMS.contains(token_program)
            && (!TERMINAL_SHELL_PROGRAMS.contains(token_program) || (token_program == "false" && split.separator.as_deref() != Some("&&")))
            && remaining.is_some()
        {
            return next(Context::Shell);
        }
        if execution_context == Context::Shell && is_unqualified && !lower.is_empty() && POWERSHELL_SETUP_PROGRAMS.contains(lower.as_str()) && remaining.is_some() {
            return next(Context::Shell);
        }
        if token_program.is_empty()
            || (is_unqualified && NON_DESCRIPTIVE_SHELL_PROGRAMS.contains(token_program))
            || token_program.chars().next().is_some_and(|c| NON_PROGRAM_PREFIX_CHARACTERS.contains(c))
            || token_program.chars().last().is_some_and(|c| NON_PROGRAM_SUFFIX_CHARACTERS.contains(c))
            || token_program.ends_with("()")
            || re(&FUNCTION_TOKEN, r"^[A-Za-z_][A-Za-z0-9_]*\(\)\{$").is_match(token_program)
        {
            return None;
        }
        return Some(token_program.to_owned());
    }
    if (saw_assignment || saw_redirection) && wrapper.is_none() {
        return next(execution_context);
    }
    None
}

fn program_name_internal(command: &str, depth: usize, context: Context, segments_remaining: usize) -> Option<String> {
    if segments_remaining == 0 {
        return None;
    }
    if let Some(called) = powershell_call_operator_program(command) {
        return called;
    }
    parse_program_name(command, depth, context, segments_remaining).or_else(|| if context == Context::Shell { literal_alias_program(command) } else { None })
}

/// `commandProgramName`: the program a command runs, for "Ran npm", or none when it names none.
pub fn program_name(command: &str) -> Option<String> {
    program_name_internal(command, 0, Context::Shell, MAX_COMMAND_SEGMENTS)
}

/// `withVisibleControlCharacters`: each control character as its control picture.
pub fn visible_control_characters(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{7f}' => '\u{2421}',
            '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' => char::from_u32(0x2400 + c as u32).unwrap_or(c),
            c => c,
        })
        .collect()
}

// A wrapper whose quoting does not balance is kept: the script's end cannot be known, which happens
// when Codex's secret redaction removes the backslash of an escaped quote inside `-lc "…"`.
fn without_shell_wrapper(command: &str) -> String {
    static EXE: OnceLock<Regex> = OnceLock::new();
    let trimmed = trim(command).to_owned();
    if split_first_shell_command(&trimmed).remaining.is_some() {
        return trimmed;
    }
    let Some(tokens) = tokenize_shell_command(&trimmed) else { return trimmed };
    let program = tokens.first().map(|token| re(&EXE, r"(?i)\.exe$").replace(last_segment(token), "").into_owned());
    if !program.as_deref().is_some_and(|program| !program.is_empty() && SHELL_PROGRAMS.contains(program)) {
        return trimmed;
    }
    // Positional arguments can affect the script; keep those invocations intact.
    let Some(script_index) = shell_command_argument_index(&tokens, 1).filter(|index| *index == tokens.len() - 1) else { return trimmed };
    let script = trim(&tokens[script_index]);
    if script.is_empty() { trimmed } else { script.to_owned() }
}

/// `commandDisplayText`: the command as rows show it, a plain `sh -c` wrapper removed and control
/// characters made visible. Callers keep the original for details.
pub fn display_text(command: &str) -> String {
    visible_control_characters(&without_shell_wrapper(command))
}
