//! Which links the app hands to the operating system (`parseSafeExternalUrl` in `ElectronShell.ts`): web
//! pages, and the deep links that open a remote folder in a local editor. Anything else (`file:`, `javascript:`,
//! custom schemes of other apps) is refused, because the text comes from pages and agents.

/// The editors' remote-folder schemes (`remoteScheme` in `packages/contracts/src/editor.ts`).
const REMOTE_EDITOR_SCHEMES: [&str; 5] = ["cursor", "vscode", "vscode-insiders", "vscodium", "zed"];

/// The link to open, normalised, or none if it must not be opened.
pub fn safe_external_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once(':')?;
    let scheme = scheme.to_ascii_lowercase();
    if !scheme.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')) {
        return None;
    }
    let authority_and_path = rest.strip_prefix("//")?;
    let end = authority_and_path.find(['/', '?', '#']).unwrap_or(authority_and_path.len());
    let (authority, path) = authority_and_path.split_at(end);
    // A name and password before the host are never part of a link to open.
    if authority.is_empty() || authority.contains('@') || authority.chars().any(|character| character.is_whitespace() || character.is_control()) || path.chars().any(char::is_control) {
        return None;
    }
    let host = authority.rsplit_once(':').filter(|(_, port)| port.bytes().all(|byte| byte.is_ascii_digit())).map_or(authority, |(host, _)| host);
    let allowed = match scheme.as_str() {
        "http" | "https" => !host.is_empty(),
        "zed" => host.eq_ignore_ascii_case("ssh") && path.strip_prefix('/').is_some_and(|rest| rest.split('/').next().is_some_and(|first| !first.is_empty() && !first.contains([':', '@'])) && rest.contains('/')),
        scheme if REMOTE_EDITOR_SCHEMES.contains(&scheme) => host.eq_ignore_ascii_case("vscode-remote") && path.strip_prefix("/ssh-remote+").is_some_and(|host| !host.is_empty()),
        _ => false,
    };
    allowed.then(|| format!("{scheme}://{authority_and_path}"))
}

#[cfg(test)]
mod tests {
    use super::safe_external_url as safe;

    #[test]
    fn web_pages_and_remote_editor_links_open_and_nothing_else_does() {
        assert_eq!(safe("https://github.com/acme/web/pull/7?x=1#top").as_deref(), Some("https://github.com/acme/web/pull/7?x=1#top"));
        assert_eq!(safe("  HTTP://localhost:3000/a ").as_deref(), Some("http://localhost:3000/a"));
        assert_eq!(safe("https://example.com").as_deref(), Some("https://example.com"));
        assert_eq!(safe("vscode://vscode-remote/ssh-remote+devbox/home/me/project").as_deref(), Some("vscode://vscode-remote/ssh-remote+devbox/home/me/project"));
        assert_eq!(safe("cursor://vscode-remote/ssh-remote+devbox/p").as_deref(), Some("cursor://vscode-remote/ssh-remote+devbox/p"));
        assert_eq!(safe("zed://ssh/devbox/home/me/project").as_deref(), Some("zed://ssh/devbox/home/me/project"));
        for refused in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "mailto:me@example.com",
            "data:text/html,<b>x</b>",
            "ssh://host/path",
            "x-apple.systempreferences:com.apple.preference.security",
            "https:evil.example",
            "https://",
            "https://user:pw@example.com/",
            "https://exa mple.com/",
            "https://example.com/a\nb",
            "not a url",
            "",
            // Editor links only for the remote-folder forms.
            "vscode://file/Users/me/.ssh/id_rsa",
            "vscode://vscode-remote/ssh-remote+",
            "vscode://vscode-remote/other",
            "zed://ssh/",
            "zed://ssh/host",
            "zed://ssh/user@host/path",
            "zed://elsewhere/host/path",
            "unknown-editor://vscode-remote/ssh-remote+devbox/p",
        ] {
            assert_eq!(safe(refused), None, "{refused:?}");
        }
    }
}
