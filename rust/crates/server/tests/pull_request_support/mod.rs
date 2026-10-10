//! A fake `glab` shared by the GitLab pull request tests: it numbers each call under a lock, keeps
//! its arguments and stdin, answers from the first route whose fragments its arguments (and
//! stdin) contain, and can be switched to fail as a signed-out, rate-limited or missing-resource
//! CLI would by writing `auth`, `rate` or `notfound` to its `mode` file.
#![allow(dead_code)]
use crate::source_control_support::FakeCliDir;
use serde_json::{Value, json};

pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A `glab` that numbers each call under a lock, keeps its arguments and stdin, and answers from
/// the first route whose fragments its arguments (and stdin) contain.
pub fn install_glab(dir: &FakeCliDir, routes: &[Value]) {
    if routes.iter().all(|route| route["spawn"] == true) {
        return;
    }
    let mut body = String::from(
        "d=\"${0%/*}\"\nwhile ! mkdir \"$d/lock\" 2>/dev/null; do :; done\nn=0\nif [ -f \"$d/seq\" ]; then read -r n < \"$d/seq\"; fi\necho $((n+1)) > \"$d/seq\"\nrmdir \"$d/lock\"\nprintf '%s\\037' \"$@\" > \"$d/call.$n.args\"\ncat > \"$d/call.$n.stdin\"\njoined=\"$*\"\nin=$(cat \"$d/call.$n.stdin\")\nmode=ok\nif [ -f \"$d/mode\" ]; then read -r mode < \"$d/mode\"; fi\ncase \"$mode\" in\nauth) echo 'glab auth login' >&2; exit 1;;\nrate) echo 'HTTP 429' >&2; exit 1;;\nnotfound) echo '404 Not Found' >&2; exit 1;;\nesac\n",
    );
    for (index, route) in routes.iter().enumerate() {
        std::fs::write(dir.dir.path().join(format!("route.{index}.out")), route["stdout"].as_str().unwrap_or("")).unwrap();
        std::fs::write(dir.dir.path().join(format!("route.{index}.err")), route["stderr"].as_str().unwrap_or("")).unwrap();
        let answer = format!(
            "cat \"$d/route.{index}.out\"; cat \"$d/route.{index}.err\" >&2; exit {}",
            route["code"].as_i64().unwrap_or(0)
        );
        let inner = match route["stdinIncludes"].as_str() {
            Some(needle) => format!("case \"$in\" in *{}*) {answer};; esac", shell_quote(needle)),
            None => answer,
        };
        body.push_str(&format!("case \"$joined\" in *{}*) {inner};; esac\n", shell_quote(route["match"].as_str().unwrap())));
    }
    body.push_str("echo \"no route: $joined\" >&2\nexit 97\n");
    let path = dir.dir.path().join("glab");
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The fake's PATH holds nothing but the tools linked into it.
    for tool in ["mkdir", "rmdir"] {
        let found = String::from_utf8(std::process::Command::new("which").arg(tool).output().unwrap().stdout).unwrap();
        let _ = std::os::unix::fs::symlink(found.trim(), dir.dir.path().join(tool));
    }
}

/// Every call the fake saw since the last look, in the order they started.
pub fn take_calls(dir: &FakeCliDir) -> Vec<Value> {
    let mut numbered: Vec<(u64, Value)> = vec![];
    for entry in std::fs::read_dir(dir.dir.path()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(number) = name.strip_prefix("call.").and_then(|rest| rest.strip_suffix(".args")).and_then(|number| number.parse::<u64>().ok()) else { continue };
        let arguments = std::fs::read_to_string(entry.path()).unwrap();
        let args: Vec<&str> = arguments.strip_suffix('\u{1f}').map(|text| text.split('\u{1f}').collect()).unwrap_or_default();
        let stdin = std::fs::read_to_string(dir.dir.path().join(format!("call.{number}.stdin"))).unwrap();
        numbered.push((number, json!({"args": args, "stdin": if stdin.is_empty() { Value::Null } else { json!(stdin) }})));
        std::fs::remove_file(entry.path()).unwrap();
        std::fs::remove_file(dir.dir.path().join(format!("call.{number}.stdin"))).unwrap();
    }
    numbered.sort_by_key(|(number, _)| *number);
    numbered.into_iter().map(|(_, call)| call).collect()
}

