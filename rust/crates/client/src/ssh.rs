//! SSH environments, the decisions without a window or a process: the host list the person types into (`desktopSshHosts.ts`),
//! the target a typed host becomes (`parseManualDesktopSshTarget` of `ConnectionsSettings.tsx`), how a target and a failure
//! read (`formatDesktopSshTarget`, `formatDesktopSshConnectionError`) and the password prompt's clock
//! (`SshPasswordPromptDialog.tsx`). What runs ssh is the desktop shell (`t3-desktop`); the Connections page shows these.

/// `DesktopSshEnvironmentTarget`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub alias: String,
    pub hostname: String,
    pub username: Option<String>,
    pub port: Option<i64>,
}

/// `DesktopDiscoveredSshHost`: a host from the ssh configuration or the known hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHost {
    pub target: Target,
    /// `ssh-config` or `known-hosts`.
    pub source: String,
}

/// `DesktopSshPasswordPromptRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRequest {
    pub request_id: String,
    pub destination: String,
    pub username: Option<String>,
    pub prompt: String,
    /// Milliseconds since the Unix epoch.
    pub expires_at_ms: u64,
}

/// `formatDesktopSshTarget`.
pub fn format_target(target: &Target) -> String {
    let authority = match target.username.as_deref().filter(|username| !username.is_empty()) {
        Some(username) => format!("{username}@{}", target.hostname),
        None => target.hostname.clone(),
    };
    match target.port.filter(|port| *port != 0) {
        Some(port) => format!("{authority}:{port}"),
        None => authority,
    }
}

/// `filterDiscoveredSshHosts`: the hosts whose alias starts with the query, then those that contain it.
pub fn filter_hosts<'a>(hosts: &'a [DiscoveredHost], query: &str) -> Vec<&'a DiscoveredHost> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return hosts.iter().collect();
    }
    let alias = |host: &DiscoveredHost| host.target.alias.to_lowercase();
    let prefix = hosts.iter().filter(|host| alias(host).starts_with(&query));
    let inner = hosts.iter().filter(|host| !alias(host).starts_with(&query) && alias(host).contains(&query));
    prefix.chain(inner).collect()
}

/// JavaScript's `Number.parseInt(text, 10)`: the leading digits after an optional sign, none being NaN.
fn parse_int(text: &str) -> Option<i128> {
    let text = text.trim_start();
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let end = rest.bytes().position(|byte| !byte.is_ascii_digit()).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    // Further digits only make a value no port can be; the clamp keeps the arithmetic exact.
    let value = rest[..end].bytes().fold(0i128, |value, byte| (value * 10 + i128::from(byte - b'0')).min(1 << 100));
    Some(if negative { -value } else { value })
}

/// `parseManualDesktopSshTarget`: `user@host`, `host:port` and `[::1]:port` are read; the Username and Port fields win.
pub fn parse_manual_target(host: &str, username: &str, port: &str) -> Result<Target, String> {
    const HOST_REQUIRED: &str = "SSH host or alias is required.";
    let raw_host = host.trim();
    if raw_host.is_empty() {
        return Err(HOST_REQUIRED.into());
    }
    let mut hostname = raw_host.to_owned();
    let mut username = Some(username.trim().to_owned()).filter(|username| !username.is_empty());
    let mut parsed_port: Option<Option<i128>> = None;
    if let Some(at) = hostname.rfind('@').filter(|at| *at > 0) {
        let inline = hostname[..at].trim().to_owned();
        hostname = hostname[at + 1..].trim().to_owned();
        if username.is_none() && !inline.is_empty() {
            username = Some(inline);
        }
    }
    let numeric = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    // /^\[([^\]]+)\](?::(\d+))?$/u
    let bracketed = hostname
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .filter(|(inner, tail)| !inner.is_empty() && (tail.is_empty() || (tail.starts_with(':') && numeric(&tail[1..]))))
        .map(|(inner, tail)| (inner.to_owned(), tail.to_owned()));
    if let Some((inner, tail)) = bracketed {
        if let Some(number) = tail.strip_prefix(':') {
            parsed_port = Some(parse_int(number));
        }
        hostname = inner.trim().to_owned();
    } else {
        let segments: Vec<&str> = hostname.split(':').collect();
        if segments.len() == 2 && numeric(segments[1]) {
            parsed_port = Some(parse_int(segments[1]));
            hostname = segments[0].trim().to_owned();
        }
    }
    let raw_port = port.trim();
    if !raw_port.is_empty() {
        parsed_port = Some(parse_int(raw_port));
    }
    if hostname.is_empty() {
        return Err(HOST_REQUIRED.into());
    }
    let port = match parsed_port {
        None => None,
        Some(Some(port)) if (1..=65_535).contains(&port) => Some(port as i64),
        Some(_) => return Err("SSH port must be between 1 and 65535.".into()),
    };
    Ok(Target { alias: hostname.clone(), hostname, username, port })
}

/// `formatDesktopSshConnectionError`: the message without the IPC wrapper's prefix or the error class's.
pub fn format_connection_error(message: &str) -> String {
    use std::sync::OnceLock;
    static IPC: OnceLock<regex::Regex> = OnceLock::new();
    static TAGGED: OnceLock<regex::Regex> = OnceLock::new();
    let ipc = IPC.get_or_init(|| regex::Regex::new(r"^Error invoking remote method 'desktop:ensure-ssh-environment':\s*").expect("pattern"));
    let tagged = TAGGED.get_or_init(|| regex::Regex::new(r"^Ssh[A-Za-z]+Error:\s*").expect("pattern"));
    let message = ipc.replace(message, "");
    let message = tagged.replace(&message, "");
    let message = message.trim();
    if message.is_empty() { "Failed to connect SSH host.".into() } else { message.into() }
}

/// `describeSshTarget` of the password dialog.
pub fn describe_prompt_target(request: &PromptRequest) -> String {
    match request.username.as_deref().filter(|username| !username.is_empty()) {
        Some(username) => format!("{username}@{}", request.destination),
        None => request.destination.clone(),
    }
}

/// The prompt's clock: whole seconds left (rounded up), zero once the time is up.
pub fn seconds_left(request: &PromptRequest, now_ms: u64) -> u64 {
    request.expires_at_ms.saturating_sub(now_ms).div_ceil(1_000)
}

/// `formatRemainingSeconds`.
pub fn format_remaining(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

pub const EXPIRED_MESSAGE: &str = "This SSH password prompt expired. Try connecting again.";

/// `getPromptErrorMessage`: what the dialog says when the answer could not be delivered.
pub fn prompt_error_message(message: &str) -> String {
    if message.contains("expired") || message.contains("no longer pending") { EXPIRED_MESSAGE.into() } else { message.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(alias: &str) -> DiscoveredHost {
        DiscoveredHost { target: Target { alias: alias.into(), hostname: alias.into(), username: None, port: None }, source: "ssh-config".into() }
    }

    #[test]
    fn hosts_match_by_prefix_before_substring_and_case_is_ignored() {
        let hosts = [host("web-box"), host("devbox"), host("Dev-2"), host("box")];
        let names = |query: &str| filter_hosts(&hosts, query).iter().map(|host| host.target.alias.clone()).collect::<Vec<_>>();
        assert_eq!(names(""), ["web-box", "devbox", "Dev-2", "box"]);
        assert_eq!(names("  DEV "), ["devbox", "Dev-2"]);
        assert_eq!(names("box"), ["box", "web-box", "devbox"]);
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn a_typed_host_is_read_as_the_original_reads_it() {
        let parse = parse_manual_target;
        let target = |alias: &str, username: Option<&str>, port: Option<i64>| Target { alias: alias.into(), hostname: alias.into(), username: username.map(Into::into), port };
        assert_eq!(parse("devbox", "", ""), Ok(target("devbox", None, None)));
        assert_eq!(parse("julius@devbox:2222", "", ""), Ok(target("devbox", Some("julius"), Some(2222))));
        assert_eq!(parse("julius@devbox", "root", ""), Ok(target("devbox", Some("root"), None)), "the Username field wins");
        assert_eq!(parse("devbox:22", "", "2200"), Ok(target("devbox", None, Some(2200))), "the Port field wins");
        assert_eq!(parse("[::1]:2222", "", ""), Ok(target("::1", None, Some(2222))));
        assert_eq!(parse("[fe80::1]", "", ""), Ok(target("fe80::1", None, None)));
        assert_eq!(parse("a:b:22", "", ""), Ok(target("a:b:22", None, None)), "more than one colon is no port");
        assert_eq!(parse("  ", "", "").unwrap_err(), "SSH host or alias is required.");
        assert_eq!(parse("julius@", "", "").unwrap_err(), "SSH host or alias is required.");
        for bad in ["0", "65536", "-1", "abc"] {
            assert_eq!(parse("devbox", "", bad).unwrap_err(), "SSH port must be between 1 and 65535.", "{bad}");
        }
        assert_eq!(parse("devbox", "", "22abc"), Ok(target("devbox", None, Some(22))), "parseInt reads the leading digits");
        assert!(parse("devbox:99999999999999999999999", "", "").is_err());
    }

    #[test]
    fn targets_and_failures_read_as_in_the_original() {
        let target = Target { alias: "devbox".into(), hostname: "devbox.example.com".into(), username: Some("julius".into()), port: Some(2222) };
        assert_eq!(format_target(&target), "julius@devbox.example.com:2222");
        assert_eq!(format_target(&Target { username: None, port: None, ..target }), "devbox.example.com");
        assert_eq!(format_connection_error("Error invoking remote method 'desktop:ensure-ssh-environment': SshLaunchError: boom "), "boom");
        assert_eq!(format_connection_error("  "), "Failed to connect SSH host.");
    }

    #[test]
    fn the_prompt_counts_down_in_whole_seconds_and_names_who_asks() {
        let request = PromptRequest { request_id: "r".into(), destination: "devbox".into(), username: Some("julius".into()), prompt: "p".into(), expires_at_ms: 10_000 };
        assert_eq!(describe_prompt_target(&request), "julius@devbox");
        assert_eq!(describe_prompt_target(&PromptRequest { username: None, ..request.clone() }), "devbox");
        assert_eq!((seconds_left(&request, 8_001), seconds_left(&request, 10_000), seconds_left(&request, 12_000)), (2, 0, 0));
        assert_eq!((format_remaining(180), format_remaining(65), format_remaining(5)), ("3:00".to_owned(), "1:05".to_owned(), "0:05".to_owned()));
        assert_eq!(prompt_error_message("SSH password prompt expired. Try connecting again."), EXPIRED_MESSAGE);
        assert_eq!(prompt_error_message("It is no longer pending"), EXPIRED_MESSAGE);
        assert_eq!(prompt_error_message("boom"), "boom");
    }
}
