//! Pure listener parsing, loopback URL projection and terminal PID ownership.
//! HTTP probing and lifecycle live in the discovery service.
use crate::terminal_activity::ProcessId;
use indexmap::{IndexMap, IndexSet};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use t3_contracts::{ThreadId, ValidationError, trim_wire_string};
use url::Url;
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOwner {
    pub thread_id: ThreadId,
    pub terminal_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalServer {
    pub host: String,
    pub port: u16,
    pub url: String,
    pub process_name: Option<String>,
    pub pid: Option<ProcessId>,
    pub terminal: Option<TerminalOwner>,
}
#[derive(Clone, Default)]
pub struct TerminalRegistry(Arc<Mutex<IndexMap<String, (TerminalOwner, IndexSet<ProcessId>)>>>);
impl TerminalRegistry {
    pub fn register(
        &self,
        thread: &str,
        terminal: &str,
        pids: impl IntoIterator<Item = f64>,
    ) -> Result<(), ValidationError> {
        let owner = TerminalOwner {
            thread_id: ThreadId::new(thread)?,
            terminal_id: terminal.into(),
        };
        let key = format!("{}\0{}", owner.thread_id, terminal);
        let pids = pids
            .into_iter()
            .filter(|pid| *pid > 0.0)
            .filter_map(ProcessId::new)
            .collect::<IndexSet<_>>();
        let mut registered = self.0.lock().unwrap();
        if pids.is_empty() {
            registered.shift_remove(&key);
        } else {
            registered.insert(key, (owner, pids));
        }
        Ok(())
    }
    pub fn unregister(&self, thread: &str, terminal: &str) {
        self.0
            .lock()
            .unwrap()
            .shift_remove(&format!("{thread}\0{terminal}"));
    }
    pub fn snapshot(&self) -> HashMap<ProcessId, TerminalOwner> {
        self.0
            .lock()
            .unwrap()
            .values()
            .flat_map(|(owner, pids)| pids.iter().map(move |pid| (*pid, owner.clone())))
            .collect()
    }
}
pub fn loopback_host(host: &str) -> bool {
    matches!(
        host,
        "localhost" | "127.0.0.1" | "0.0.0.0" | "::1" | "[::1]"
    )
}
fn listener_host(host: &str) -> bool {
    loopback_host(host) || matches!(host, "*" | "[::]")
}
pub fn server_key(host: &str, port: u16) -> String {
    format!(
        "{}:{port}",
        if loopback_host(host) {
            "loopback".into()
        } else {
            host.to_lowercase()
        }
    )
}
fn decimal_prefix(raw: &str) -> Option<f64> {
    let raw = trim_wire_string(raw);
    let raw = raw.strip_prefix('+').unwrap_or(raw);
    let negative = raw.starts_with('-');
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    let count = digits.bytes().take_while(u8::is_ascii_digit).count();
    if count == 0 {
        return None;
    }
    let number = digits[..count].parse::<f64>().ok()?;
    let number = if negative { -number } else { number };
    number.is_finite().then_some(number)
}
pub fn port_from_lsof_name(raw: &str) -> Option<u16> {
    let name = trim_wire_string(raw.split(' ').next().unwrap_or(""));
    let (host, port) = name.rsplit_once(':')?;
    if !listener_host(host) {
        return None;
    }
    let number = decimal_prefix(port)?;
    (number > 0.0 && number < 65536.0).then_some(number as u16)
}
fn candidate(
    port: u16,
    pid: Option<ProcessId>,
    process_name: Option<String>,
    owners: &HashMap<ProcessId, TerminalOwner>,
) -> LocalServer {
    LocalServer {
        host: "localhost".into(),
        port,
        url: format!("http://localhost:{port}"),
        process_name,
        pid,
        terminal: pid.and_then(|pid| owners.get(&pid).cloned()),
    }
}
pub fn parse_lsof(raw: &str, owners: &HashMap<ProcessId, TerminalOwner>) -> Vec<LocalServer> {
    let mut seen = IndexMap::new();
    let mut pid = None;
    let mut process_name = None;
    for line in raw.split('\n').filter(|line| !line.is_empty()) {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                pid = decimal_prefix(value)
                    .filter(|number| *number > 0.0)
                    .and_then(ProcessId::new);
                process_name = None;
            }
            'c' => {
                let name = trim_wire_string(value);
                process_name = (!name.is_empty()).then(|| name.to_owned());
            }
            'n' => {
                if let Some(port) = port_from_lsof_name(value) {
                    seen.entry(port)
                        .or_insert_with(|| candidate(port, pid, process_name.clone(), owners));
                }
            }
            _ => {}
        }
    }
    let mut servers = seen.into_values().collect::<Vec<_>>();
    servers.sort_by_key(|server| server.port);
    servers
}
pub fn parse_windows(raw: &str, owners: &HashMap<ProcessId, TerminalOwner>) -> Vec<LocalServer> {
    let mut seen = IndexMap::new();
    for line in raw.split('\n') {
        let mut fields = trim_wire_string(line).split('|');
        let host = trim_wire_string(fields.next().unwrap_or(""));
        if !listener_host(host) && host != "::" {
            continue;
        }
        let port = javascript_number(fields.next().unwrap_or("undefined"));
        let pid = javascript_number(fields.next().unwrap_or("undefined"));
        if !port.is_finite() || port.fract() != 0.0 || port <= 0.0 || port >= 65536.0 {
            continue;
        }
        let name = trim_wire_string(fields.next().unwrap_or(""));
        let name = (!name.is_empty()).then(|| name.to_owned());
        let pid = if pid > 0.0 { ProcessId::new(pid) } else { None };
        seen.entry(port as u16)
            .or_insert_with(|| candidate(port as u16, pid, name, owners));
    }
    let mut servers = seen.into_values().collect::<Vec<_>>();
    servers.sort_by_key(|server| server.port);
    servers
}
fn javascript_number(raw: &str) -> f64 {
    let raw = trim_wire_string(raw);
    if raw.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = raw.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            let mut value = 0.0;
            for digit in digits.chars() {
                let Some(digit) = digit.to_digit(radix) else {
                    return f64::NAN;
                };
                value = value * f64::from(radix) + f64::from(digit);
            }
            return value;
        }
    }
    // Rust accepts inf/NaN; JS Number does not, except spelled Infinity.
    if raw == "Infinity" || raw == "+Infinity" {
        return f64::INFINITY;
    }
    if raw == "-Infinity" {
        return f64::NEG_INFINITY;
    }
    if raw.contains(|ch: char| !ch.is_ascii_digit() && !matches!(ch, '+' | '-' | '.' | 'e' | 'E')) {
        return f64::NAN;
    }
    raw.parse().unwrap_or(f64::NAN)
}
pub fn configured_urls(raw: &[String]) -> Vec<String> {
    raw.iter()
        .take(32)
        .filter(|raw| raw.encode_utf16().count() <= 2048)
        .filter_map(|raw| {
            let mut url = Url::parse(raw).ok()?;
            if !matches!(url.scheme(), "http" | "https") || !loopback_host(url.host_str()?) {
                return None;
            }
            if url.as_str().encode_utf16().count() > 2048 {
                return None;
            }
            if url.host_str() == Some("0.0.0.0") {
                url.set_host(Some("localhost")).ok()?;
            }
            (url.as_str().encode_utf16().count() <= 2048).then(|| url.to_string())
        })
        .collect::<IndexSet<_>>()
        .into_iter()
        .collect()
}
pub fn cache_key(raw: &str) -> Option<String> {
    let mut url = Url::parse(raw).ok()?;
    url.set_fragment(None);
    Some(url.to_string())
}
pub fn url_port(url: &Url) -> u16 {
    url.port_or_known_default()
        .unwrap_or(if url.scheme() == "http" { 80 } else { 443 })
}
pub fn web_response(status: u16, location: Option<&str>, content_type: Option<&str>) -> bool {
    if matches!(status, 301 | 302 | 303 | 307 | 308)
        && location.is_some_and(|value| !trim_wire_string(value).is_empty())
    {
        return true;
    }
    if !(200..300).contains(&status) || matches!(status, 204 | 205) {
        return false;
    }
    content_type.is_some_and(|content_type| {
        matches!(
            trim_wire_string(content_type.split(';').next().unwrap_or(""))
                .to_lowercase()
                .as_str(),
            "text/html" | "application/xhtml+xml"
        )
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    #[test]
    fn native_listener_and_url_parsing_match_original_source_oracle() {
        for (index, line) in include_str!("../tests/fixtures/resource-ports.jsonl")
            .lines()
            .enumerate()
        {
            let case: Value = serde_json::from_str(line).unwrap();
            let owners = case["owners"]
                .as_array()
                .map(|owners| {
                    owners
                        .iter()
                        .map(|row| {
                            (
                                ProcessId::new(row[0].as_f64().unwrap()).unwrap(),
                                TerminalOwner {
                                    thread_id: ThreadId::new(row[1]["threadId"].as_str().unwrap())
                                        .unwrap(),
                                    terminal_id: row[1]["terminalId"].as_str().unwrap().into(),
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let actual = match case["kind"].as_str().unwrap() {
                "port" => json!(port_from_lsof_name(case["input"].as_str().unwrap())),
                "lsof" => {
                    serde_json::to_value(parse_lsof(case["input"].as_str().unwrap(), &owners))
                        .unwrap()
                }
                "windows" => {
                    serde_json::to_value(parse_windows(case["input"].as_str().unwrap(), &owners))
                        .unwrap()
                }
                "urls" => json!(configured_urls(
                    &serde_json::from_value::<Vec<String>>(case["input"].clone()).unwrap()
                )),
                "cache" => json!(cache_key(case["input"].as_str().unwrap())),
                _ => unreachable!(),
            };
            assert_eq!(
                actual, case["expected"],
                "PortScanner witness{index}: {case}"
            );
        }
    }
    #[test]
    fn registration_replacement_empty_release_and_shared_pid_order_follow_source_map() {
        let registry = TerminalRegistry::default();
        registry
            .register("one", "term", [12.0, 12.0, -1.0, 0.0, 1.5, f64::INFINITY])
            .unwrap();
        registry.register("two", "term", [12.0, 13.0]).unwrap();
        assert_eq!(
            registry.snapshot()[&ProcessId::new(12.0).unwrap()]
                .thread_id
                .as_str(),
            "two"
        );
        // Map replacement preserves original insertion position.
        registry.register("one", "term", [12.0]).unwrap();
        assert_eq!(
            registry.snapshot()[&ProcessId::new(12.0).unwrap()]
                .thread_id
                .as_str(),
            "two"
        );
        registry.unregister("two", "term");
        assert_eq!(
            registry.snapshot()[&ProcessId::new(12.0).unwrap()]
                .thread_id
                .as_str(),
            "one"
        );
        registry.register("one", "term", []).unwrap();
        assert!(registry.snapshot().is_empty());
    }
}
