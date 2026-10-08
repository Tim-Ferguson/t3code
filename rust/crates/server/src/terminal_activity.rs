//! Shared process-table inspection for terminal activity. A failed or truncated
//! table is never equivalent to an idle terminal.
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};
use t3_contracts::base::trim_wire_string;

/// JavaScript process-table keys are integral Numbers, rather than wire IDs.
/// Preserve their numeric identity (including -0 and rounded large integers).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ProcessId(u64);
impl ProcessId {
    pub fn new(value: f64) -> Option<Self> {
        (value.is_finite() && value.fract() == 0.0).then(|| {
            Self(if value == 0.0 {
                0.0_f64.to_bits()
            } else {
                value.to_bits()
            })
        })
    }
    pub fn number(self) -> f64 {
        f64::from_bits(self.0)
    }
}
impl Serialize for ProcessId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let number = self.number();
        if number >= i64::MIN as f64 && number < -(i64::MIN as f64) {
            serializer.serialize_i64(number as i64)
        } else {
            serializer.serialize_f64(number)
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProcessTable {
    children: HashMap<ProcessId, Vec<ProcessId>>,
    commands: HashMap<ProcessId, String>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub has_running_subprocess: bool,
    pub child_command: Option<String>,
    pub process_ids: Vec<ProcessId>,
}
impl ProcessTable {
    pub fn from_entries(entries: impl IntoIterator<Item = (f64, f64, String)>) -> Self {
        let mut table = Self::default();
        for (pid, parent, command) in entries {
            if let (Some(pid), Some(parent)) = (ProcessId::new(pid), ProcessId::new(parent)) {
                table
                    .commands
                    .insert(pid, trim_wire_string(&command).to_owned());
                table.children.entry(parent).or_default().push(pid);
            }
        }
        table
    }
    pub fn posix(stdout: &str) -> Self {
        static LINE: OnceLock<regex::Regex> = OnceLock::new();
        let regex = LINE.get_or_init(|| regex::Regex::new(concat!(
            r"^[\t-\r \x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]*([0-9]+)",
            r"[\t-\r \x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]+([0-9]+)",
            r"[\t-\r \x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]+([^\r\n\x{2028}\x{2029}]+)$"
        )).unwrap());
        Self::from_entries(stdout.split('\n').filter_map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let captures = regex.captures(line)?;
            Some((
                captures[1].parse().ok()?,
                captures[2].parse().ok()?,
                captures[3].to_owned(),
            ))
        }))
    }
    pub fn windows(stdout: &str) -> Self {
        Self::from_entries(stdout.split('\n').filter_map(|line| {
            // JS split("|", 3) ignores everything after the third component.
            let mut fields = trim_wire_string(line).split('|');
            let pid = javascript_number(fields.next()?);
            let parent = javascript_number(fields.next()?);
            (pid > 0.0).then(|| (pid, parent, fields.next().unwrap_or("").to_owned()))
        }))
    }
    pub fn inspect(&self, terminal_pid: ProcessId, platform: &str) -> Inspection {
        let command = |pid: ProcessId| {
            self.commands
                .get(&pid)
                .and_then(|name| normalize_command(name, platform))
        };
        let shell = command(terminal_pid);
        let selected = self
            .children
            .get(&terminal_pid)
            .into_iter()
            .flatten()
            .find(|&&pid| {
                shell.is_none()
                    || command(pid) != shell
                    || self.children.get(&pid).is_some_and(|v| !v.is_empty())
            })
            .copied();
        let Some(selected) = selected else {
            return Inspection {
                has_running_subprocess: false,
                child_command: None,
                process_ids: vec![],
            };
        };
        let mut process_ids = vec![terminal_pid];
        let mut seen = HashSet::from([terminal_pid]);
        let mut pending = vec![terminal_pid];
        while let Some(parent) = pending.pop() {
            for &pid in self.children.get(&parent).into_iter().flatten() {
                if seen.insert(pid) {
                    process_ids.push(pid);
                    pending.push(pid);
                }
            }
        }
        Inspection {
            has_running_subprocess: true,
            child_command: command(selected).map(|command| truncate_label(&command)),
            process_ids,
        }
    }
}

pub fn normalize_command(raw: &str, platform: &str) -> Option<String> {
    let mut value = trim_wire_string(raw);
    if (value.starts_with('[') && value.ends_with(']'))
        || (value.starts_with('(') && value.ends_with(')'))
    {
        value = trim_wire_string(&value[1..value.len() - 1]);
    }
    let first = value
        .split(|c: char| trim_wire_string(&c.to_string()).is_empty())
        .next()?;
    let basename = first
        .rsplit(|c| c == '/' || (platform == "win32" && c == '\\'))
        .next()?;
    let basename = if platform == "win32" && basename.to_lowercase().ends_with(".exe") {
        &basename[..basename.len() - 4]
    } else {
        basename
    };
    (!basename.is_empty()).then(|| basename.to_owned())
}
fn truncate_label(value: &str) -> String {
    // Rust strings cannot retain an unpaired surrogate created by JS slice.
    // A split surrogate uses the same replacement convention as other Rust
    // terminal labels; full Unicode scalars and UTF-16 lengths are preserved.
    String::from_utf16_lossy(&value.encode_utf16().take(128).collect::<Vec<_>>())
}
fn javascript_number(value: &str) -> f64 {
    let value = trim_wire_string(value);
    if value.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = value.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix)
                .map(|v| v as f64)
                .unwrap_or(f64::NAN);
        }
    }
    value.parse().unwrap_or(f64::NAN)
}
pub fn poll_delay_ms(interval_ms: u64, failure_count: u32) -> u64 {
    interval_ms
        .saturating_mul(1_u64.checked_shl(failure_count).unwrap_or(u64::MAX))
        .min(60_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_oracle_process_tables_commands_and_backoff() {
        for (index, line) in include_str!("../tests/fixtures/terminal-activity.jsonl")
            .lines()
            .enumerate()
        {
            let case: serde_json::Value = serde_json::from_str(line).unwrap();
            let actual = match case["kind"].as_str().unwrap() {
                "normalize" => serde_json::to_value(normalize_command(
                    case["raw"].as_str().unwrap(),
                    case["platform"].as_str().unwrap(),
                ))
                .unwrap(),
                "delay" => serde_json::json!(poll_delay_ms(
                    case["interval"].as_u64().unwrap(),
                    case["failures"].as_u64().unwrap() as u32
                )),
                kind => {
                    let table = match kind {
                        "posix" => ProcessTable::posix(case["stdout"].as_str().unwrap()),
                        "windows" => ProcessTable::windows(case["stdout"].as_str().unwrap()),
                        "entries" => ProcessTable::from_entries(
                            case["entries"].as_array().unwrap().iter().map(|row| {
                                (
                                    row["pid"].as_f64().unwrap(),
                                    row["ppid"].as_f64().unwrap(),
                                    row["name"].as_str().unwrap().to_owned(),
                                )
                            }),
                        ),
                        _ => panic!("unknown fixture"),
                    };
                    serde_json::to_value(table.inspect(
                        ProcessId::new(case["pid"].as_f64().unwrap()).unwrap(),
                        case["platform"].as_str().unwrap(),
                    ))
                    .unwrap()
                }
            };
            assert_eq!(actual, case["expected"], "source fixture {index}: {case}");
        }
    }
}
