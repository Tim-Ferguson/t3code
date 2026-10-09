//! Runs a project's setup script in a terminal of the thread and reports how it ended (port of
//! `project/ProjectSetupScriptRunner.ts`).
//!
//! The command is typed into an interactive shell, so its exit code is read back from the
//! terminal stream: the command runs inside a block that ends by printing a per-run sentinel
//! with `$?`. A run that does not need its result (a launch at the project root) skips the
//! wrapper and only starts the command.
use crate::terminal_manager::TerminalManager;
use regex::Regex;
use serde_json::{Value, json};
use std::{sync::OnceLock, time::Duration};
use tokio::sync::oneshot;

const COMPLETION_SENTINEL_PREFIX: &str = "__T3_SETUP_DONE__";
const OUTPUT_LINE_MAX_LENGTH: usize = 400;
/// A partial line longer than this is a byte stream, not a line. Only its tail is kept.
const PARTIAL_LINE_MAX_LENGTH: usize = 4_096;
/// The shell redraws its prompt just after the sentinel; closing before that would read the
/// redraw as new activity and keep an idle shell. A shell with an empty prompt never prints one.
const PROMPT_WAIT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionShell {
    Posix,
    Fish,
    PowerShell,
}

/// Predicts the shell the terminal service spawns: `$SHELL` on POSIX and PowerShell on Windows.
pub fn resolve_completion_shell(windows: bool, shell: &str) -> CompletionShell {
    if windows {
        return CompletionShell::PowerShell;
    }
    match shell.rsplit('/').next().unwrap_or(shell) {
        "fish" => CompletionShell::Fish,
        "pwsh" | "powershell" => CompletionShell::PowerShell,
        _ => CompletionShell::Posix,
    }
}

pub fn completion_sentinel(token: &str) -> String {
    format!("{COMPLETION_SENTINEL_PREFIX}_{token}:")
}

fn completion_pattern(token: &str) -> Regex {
    Regex::new(&format!("{COMPLETION_SENTINEL_PREFIX}_{}:(-?\\d+)", regex::escape(token)))
        .expect("the sentinel pattern is valid")
}

/// Removes ANSI escape sequences and cursor controls so lines can be shown as plain text.
pub fn strip_terminal_control(text: &str) -> String {
    static ESCAPES: OnceLock<Regex> = OnceLock::new();
    static CONTROLS: OnceLock<Regex> = OnceLock::new();
    let escapes = ESCAPES.get_or_init(|| {
        Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][A-Za-z0-9]|\x1b[=>]").unwrap()
    });
    let controls = CONTROLS.get_or_init(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").unwrap());
    controls.replace_all(&escapes.replace_all(text, ""), "").into_owned()
}

/// The shell input for the setup script. The command runs inside a block that closes on its own
/// line, so a trailing `# comment` or a heredoc terminator cannot swallow the sentinel. The
/// shell reads the whole block before running any of it, so a script that reads stdin cannot
/// consume the sentinel line either. Lines are separated by `\r`, the Enter key of every shell's
/// line editor.
pub fn wrap_command_for_completion(command: &str, shell: CompletionShell, sentinel: &str) -> String {
    let body = command.replace("\r\n", "\r").replace('\n', "\r");
    match shell {
        CompletionShell::PowerShell => format!(
            "$global:LASTEXITCODE = $null; & {{\r{body}\r}}; if ($null -ne $LASTEXITCODE) {{ $__t3c = $LASTEXITCODE }} elseif ($?) {{ $__t3c = 0 }} else {{ $__t3c = 1 }}; Write-Host \"{sentinel}$__t3c\""
        ),
        CompletionShell::Fish => format!("begin\r{body}\rend; printf '\\n{sentinel}%s\\n' $status"),
        CompletionShell::Posix => format!("( {body}\r); printf '\\n{sentinel}%s\\n' \"$?\""),
    }
}

/// How a setup script that was started proceeds.
pub struct Started {
    pub name: String,
    pub command: String,
    pub terminal_id: String,
    /// False when the script's `async` flag asks the agent to wait for it.
    pub async_: bool,
    /// Resolves with the script's exit code, `None` when the terminal exited or closed before
    /// the sentinel arrived. Only present when completion was observed. An exit code of 0 closes
    /// the setup shell if it has nothing left running.
    pub completion: Option<oneshot::Receiver<Option<i32>>>,
}

pub struct Input<'a> {
    pub thread_id: &'a str,
    pub project_root: &'a str,
    /// Where the script runs: the thread's worktree, or the project root.
    pub cwd: &'a str,
    pub script: &'a Value,
    pub shell: CompletionShell,
    /// Wrap the command to read back its exit code, and forward each cleaned output line.
    pub observe: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
}

/// The first script that runs when a worktree is created (`setupProjectScript`).
pub fn setup_script(scripts: &[Value]) -> Option<&Value> {
    scripts.iter().find(|script| script["runOnWorktreeCreate"] == true)
}

fn terminal_input(value: Value) -> Result<t3_contracts::TerminalOpenInput, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// Splits buffered terminal output into complete lines. A bare carriage return is how
/// installers redraw a progress line in place, so each redraw becomes a short line of its own.
fn split_lines(buffer: &str) -> (Vec<String>, String) {
    let mut lines = vec![];
    let mut current = String::new();
    let mut characters = buffer.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                lines.push(std::mem::take(&mut current));
            }
            '\n' => lines.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    (lines, current)
}

/// Opens the setup terminal and types the script into it. `operation` names the failing step
/// for the launch error: `open-terminal` or `write-command`.
pub async fn run(
    terminals: &TerminalManager,
    input: Input<'_>,
) -> Result<Started, (&'static str, String)> {
    let script = input.script;
    let script_id = script["id"].as_str().unwrap_or("setup").to_owned();
    let command = script["command"].as_str().unwrap_or("").to_owned();
    let terminal_id = format!("setup-{script_id}");
    let token = input.observe.as_ref().map(|_| uuid::Uuid::new_v4().simple().to_string());
    let sentinel = token.as_deref().map(completion_sentinel);
    let command_line = match &sentinel {
        Some(sentinel) => wrap_command_for_completion(&command, input.shell, sentinel),
        None => command.clone(),
    };
    // Subscribe before writing so the sentinel cannot race past the listener.
    let observed = match (&input.observe, &token, &sentinel) {
        (Some(on_line), Some(token), Some(sentinel)) => Some(observe(
            terminals.clone(),
            input.thread_id.to_owned(),
            terminal_id.clone(),
            token.clone(),
            sentinel.clone(),
            command_line.split('\r').filter(|line| !line.is_empty()).map(str::to_owned).collect(),
            on_line.clone(),
        )),
        _ => None,
    };
    let env = json!({
        "T3CODE_PROJECT_ROOT": input.project_root,
        "T3CODE_WORKTREE_PATH": input.cwd,
        "COLORTERM": "",
        // Setup may run before a terminal client attaches to answer color probes.
        "NO_COLOR": "1",
        "FORCE_COLOR": "0",
    });
    let open = terminal_input(json!({
        "threadId": input.thread_id, "terminalId": terminal_id, "cwd": input.cwd,
        "worktreePath": input.cwd, "env": env,
    }))
    .map_err(|detail| ("open-terminal", detail))?;
    if let Err(failure) = terminals.open(open).await {
        if let Some(observed) = &observed {
            observed.abort();
        }
        return Err(("open-terminal", serde_json::to_string(&failure.wire()).unwrap_or_default()));
    }
    let write = serde_json::from_value::<t3_contracts::TerminalWriteInput>(
        json!({"threadId": input.thread_id, "terminalId": terminal_id, "data": format!("{command_line}\r")}),
    )
    .map_err(|error| ("write-command", error.to_string()))?;
    if let Err(failure) = terminals.write(write).await {
        // Nothing will ever settle the completion if the command never ran.
        if let Some(observed) = &observed {
            observed.abort();
        }
        return Err(("write-command", serde_json::to_string(&failure.wire()).unwrap_or_default()));
    }
    Ok(Started {
        name: script["name"].as_str().unwrap_or("").to_owned(),
        command,
        terminal_id,
        async_: script["async"] != false,
        completion: observed.map(|observed| observed.completion),
    })
}

struct Observed {
    completion: oneshot::Receiver<Option<i32>>,
    task: tokio::task::AbortHandle,
}
impl Observed {
    fn abort(&self) {
        self.task.abort();
    }
}

/// Watches the setup terminal for the completion sentinel. Terminal output is a byte stream,
/// so partial lines are buffered until a newline. The subscription ends with the sentinel, an
/// exit, or a close.
fn observe(
    terminals: TerminalManager,
    thread_id: String,
    terminal_id: String,
    token: String,
    sentinel: String,
    echoed: Vec<String>,
    on_line: std::sync::Arc<dyn Fn(String) + Send + Sync>,
) -> Observed {
    let mut events = terminals.events();
    let pattern = completion_pattern(&token);
    let (settle, completion) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut buffer = String::new();
        let mut exit: Option<Option<i32>> = None;
        let mut prompt_returned = false;
        while exit.is_none() || !prompt_returned {
            let next = if exit.is_some() {
                match tokio::time::timeout(PROMPT_WAIT, events.recv()).await {
                    Ok(next) => next,
                    // A shell with an empty prompt never prints one; do not wait forever.
                    Err(_) => break,
                }
            } else {
                events.recv().await
            };
            let Ok(Some(event)) = next else {
                if exit.is_none() {
                    exit = Some(None);
                }
                break;
            };
            if event["threadId"] != thread_id.as_str() || event["terminalId"] != terminal_id.as_str() {
                continue;
            }
            match event["type"].as_str() {
                Some("output") => {
                    buffer.push_str(event["data"].as_str().unwrap_or(""));
                    let (lines, rest) = split_lines(&buffer);
                    buffer = rest;
                    // A script that never prints a newline must not grow this forever. The
                    // sentinel is always on its own line, so keeping the tail is safe.
                    if buffer.encode_utf16().count() > PARTIAL_LINE_MAX_LENGTH {
                        let keep: Vec<char> = buffer.chars().collect();
                        let start = keep.len().saturating_sub(PARTIAL_LINE_MAX_LENGTH);
                        buffer = keep[start..].iter().collect();
                    }
                    for raw in lines {
                        if exit.is_some() {
                            break;
                        }
                        if let Some(found) = pattern.captures(&raw) {
                            exit = Some(found[1].parse().ok());
                            continue;
                        }
                        let cleaned = strip_terminal_control(&raw).trim_end().to_owned();
                        if cleaned.is_empty()
                            || cleaned.contains(&sentinel)
                            || echoed.iter().any(|line| cleaned.ends_with(line.as_str()))
                        {
                            continue;
                        }
                        on_line(cleaned.chars().take(OUTPUT_LINE_MAX_LENGTH).collect());
                    }
                    // A prompt has no newline, so it is what remains once the sentinel is in.
                    if exit.is_some() && !buffer.is_empty() {
                        prompt_returned = true;
                    }
                }
                Some("exited" | "closed") => {
                    if exit.is_none() {
                        exit = Some(None);
                    }
                    prompt_returned = true;
                }
                _ => {}
            }
        }
        let code = exit.flatten();
        // A clean run leaves only an idle prompt behind; its output stays in the terminal
        // history. A failed run keeps its shell open for a look.
        if code == Some(0) {
            terminals.close_idle(&thread_id, Some(&terminal_id)).await;
        }
        let _ = settle.send(code);
    });
    Observed { completion, task: task.abort_handle() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wrapper_fits_each_shell_and_keeps_the_command_inside_a_block() {
        let sentinel = completion_sentinel("tok");
        assert_eq!(sentinel, "__T3_SETUP_DONE___tok:");
        let command = "echo one # trailing\nvp install\r\necho two";
        assert_eq!(
            wrap_command_for_completion(command, CompletionShell::Posix, &sentinel),
            "( echo one # trailing\rvp install\recho two\r); printf '\\n__T3_SETUP_DONE___tok:%s\\n' \"$?\""
        );
        assert_eq!(
            wrap_command_for_completion("make", CompletionShell::Fish, &sentinel),
            "begin\rmake\rend; printf '\\n__T3_SETUP_DONE___tok:%s\\n' $status"
        );
        assert_eq!(
            wrap_command_for_completion("make", CompletionShell::PowerShell, &sentinel),
            "$global:LASTEXITCODE = $null; & {\rmake\r}; if ($null -ne $LASTEXITCODE) { $__t3c = $LASTEXITCODE } elseif ($?) { $__t3c = 0 } else { $__t3c = 1 }; Write-Host \"__T3_SETUP_DONE___tok:$__t3c\""
        );
    }

    #[test]
    fn the_completion_shell_follows_the_platform_and_shell_variable() {
        assert_eq!(resolve_completion_shell(true, "/bin/zsh"), CompletionShell::PowerShell);
        assert_eq!(resolve_completion_shell(false, "/usr/bin/fish"), CompletionShell::Fish);
        assert_eq!(resolve_completion_shell(false, "pwsh"), CompletionShell::PowerShell);
        assert_eq!(resolve_completion_shell(false, "/bin/zsh"), CompletionShell::Posix);
        assert_eq!(resolve_completion_shell(false, ""), CompletionShell::Posix);
    }

    #[test]
    fn terminal_control_is_removed_from_output_lines() {
        assert_eq!(strip_terminal_control("\x1b[32mgreen\x1b[0m \x1b]0;title\x07done\x08\x7f"), "green done");
        assert_eq!(strip_terminal_control("\x1b(Bplain\x1b="), "plain");
    }

    #[test]
    fn output_splits_on_every_line_ending_and_keeps_the_partial_line() {
        let (lines, rest) = split_lines("a\r\nb\rprogress 1\rprogress 2\nprompt$ ");
        assert_eq!(lines, ["a", "b", "progress 1", "progress 2"]);
        assert_eq!(rest, "prompt$ ");
    }

    #[test]
    fn only_a_script_that_runs_on_worktree_creation_is_the_setup_script() {
        let scripts = vec![json!({"id":"a","runOnSettle":true}), json!({"id":"b","runOnWorktreeCreate":true}), json!({"id":"c","runOnWorktreeCreate":true})];
        assert_eq!(setup_script(&scripts).unwrap()["id"], "b");
        assert!(setup_script(&scripts[..1]).is_none());
    }
}
