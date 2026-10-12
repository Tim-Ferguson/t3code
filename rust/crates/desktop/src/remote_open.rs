//! "Open in <editor>" for an environment on another machine (`apps/web/src/remoteOpen.ts`, and `buildRemoteOpenUrl` and
//! `remoteSchemeForEditor` of `packages/contracts/src/editor.ts`). When the window is not on the machine the server runs on, the
//! server cannot start an editor for the person; the window hands the system a deep link instead
//! (`vscode://vscode-remote/ssh-remote+<host><path>`, `zed://ssh/<host><path>`) and the editor here connects over SSH.

/// The editors that take such a link, with the scheme of it (`REMOTE_CAPABLE_EDITOR_IDS`, in the order of the editor table).
pub const REMOTE_CAPABLE: [(&str, &str); 5] = [("cursor", "cursor"), ("vscode", "vscode"), ("vscode-insiders", "vscode-insiders"), ("vscodium", "vscodium"), ("zed", "zed")];

/// What is offered when this machine cannot be asked which of them it has (`REMOTE_FALLBACK_EDITORS`).
pub const FALLBACK_EDITORS: [&str; 1] = ["vscode"];

pub fn remote_scheme(editor: &str) -> Option<&'static str> {
    REMOTE_CAPABLE.iter().find(|(id, _)| *id == editor).map(|(_, scheme)| *scheme)
}

/// `encodeURIComponent`: everything but letters, digits and `-_.!~*'()` as the percent-encoded bytes of its UTF-8.
pub fn encode_uri_component(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// `buildRemoteOpenUrl`: the link that opens `absolute_path` on `host` in the local editor over SSH; none for an editor without one.
pub fn build_remote_open_url(editor: &str, host: &str, absolute_path: &str) -> Option<String> {
    let scheme = remote_scheme(editor)?;
    // Windows server paths (`C:\...`) appear as `/C:/...` in vscode-remote URIs.
    let posix = absolute_path.replace('\\', "/");
    let rooted = if posix.starts_with('/') { posix } else { format!("/{posix}") };
    let host = encode_uri_component(host);
    let encode_path = |path: &str| path.split('/').map(encode_uri_component).collect::<Vec<_>>().join("/");
    if editor == "zed" {
        // Zed's remote server resolves a rooted path on the system drive, so `C:\Users\x` becomes `/Users/x`. Other drives are kept as
        // they are, and a POSIX path that happens to start with `/C:` is left alone.
        let bytes = absolute_path.as_bytes();
        let system_drive = bytes.len() >= 3 && bytes[0].eq_ignore_ascii_case(&b'c') && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\');
        let zed_path = if system_drive { &rooted[3..] } else { &rooted[..] };
        return Some(format!("{scheme}://ssh/{host}{}", encode_path(zed_path)));
    }
    Some(format!("{scheme}://vscode-remote/ssh-remote+{host}{}", encode_path(&rooted)))
}

/// Where the link's SSH host name came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    /// The `~/.ssh/config` alias of an environment this app reached over SSH.
    SshAlias,
    /// A name the server advertised (`remoteOpenTargets`): the tailnet's, or mDNS.
    Tailscale,
    Mdns,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHost {
    pub kind: HostKind,
    pub host: String,
}

/// `RemoteOpenState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// The server is on this machine, and starts the editor.
    LocalExec,
    RemoteLinks(RemoteHost),
    /// The server is elsewhere and there is no name to reach it by.
    RemoteUnavailable,
}

/// What `resolveRemoteOpenState` reads of an environment.
pub struct Environment<'a> {
    /// The environment is the server this app runs for itself (or the one `T3_SERVER_URL` names), which is on this machine whatever its
    /// address is.
    pub own_server: bool,
    /// The real `~/.ssh/config` alias of an environment this app reached over SSH.
    pub ssh_alias: Option<&'a str>,
    /// The hosts the server advertises, most reachable first; none when the server predates the feature.
    pub advertised: &'a [(HostKind, String)],
}

/// `resolveRemoteOpenState`. An SSH alias beats the names a server advertises: it is what the person's own `ssh` already reaches the
/// machine by. Among the advertised names the server's order stands (the tailnet's before mDNS).
pub fn resolve_state(environment: &Environment) -> State {
    if environment.own_server {
        return State::LocalExec;
    }
    if let Some(alias) = environment.ssh_alias.filter(|alias| !alias.is_empty()) {
        return State::RemoteLinks(RemoteHost { kind: HostKind::SshAlias, host: alias.to_owned() });
    }
    match environment.advertised.first() {
        Some((kind, host)) => State::RemoteLinks(RemoteHost { kind: *kind, host: host.clone() }),
        None => State::RemoteUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn links_are_the_ones_the_original_builds_for_every_editor_host_and_path() {
        let mut checked = 0;
        for line in include_str!("../tests/fixtures/remote_open_urls.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let built = build_remote_open_url(row["editor"].as_str().unwrap(), row["host"].as_str().unwrap(), row["absolutePath"].as_str().unwrap());
            assert_eq!(built.as_deref(), row["url"].as_str(), "{row}");
            checked += 1;
        }
        assert_eq!(checked, 22 * 7 * 12, "21 editors and one that is none, seven hosts, twelve paths");
    }

    #[test]
    fn the_remote_capable_editors_are_the_originals_in_its_order() {
        let original: Vec<(String, String)> = serde_json::from_str(include_str!("../tests/fixtures/remote_capable_editors.json")).unwrap();
        let ours: Vec<(String, String)> = REMOTE_CAPABLE.iter().map(|(id, scheme)| ((*id).to_owned(), (*scheme).to_owned())).collect();
        assert_eq!(ours, original);
        assert_eq!(remote_scheme("idea"), None);
    }

    #[test]
    fn this_machine_is_local_a_ssh_alias_beats_what_the_server_advertises_and_no_name_means_no_route() {
        let advertised = vec![(HostKind::Tailscale, "box.tail.ts.net".to_owned()), (HostKind::Mdns, "box.local".to_owned())];
        let state = |own_server, ssh_alias, advertised: &[(HostKind, String)]| resolve_state(&Environment { own_server, ssh_alias, advertised });
        // The app's own server is on this machine, whatever the server says about itself.
        assert_eq!(state(true, Some("devbox"), &advertised), State::LocalExec);
        assert_eq!(state(false, Some("devbox"), &advertised), State::RemoteLinks(RemoteHost { kind: HostKind::SshAlias, host: "devbox".into() }));
        // Among advertised names the first stands: the tailnet's before mDNS.
        assert_eq!(state(false, None, &advertised), State::RemoteLinks(RemoteHost { kind: HostKind::Tailscale, host: "box.tail.ts.net".into() }));
        assert_eq!(state(false, Some(""), &advertised), State::RemoteLinks(RemoteHost { kind: HostKind::Tailscale, host: "box.tail.ts.net".into() }), "a blank alias is none");
        assert_eq!(state(false, None, &[]), State::RemoteUnavailable);
    }
}
