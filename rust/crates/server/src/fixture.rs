//! What `t3-server serve --fixture` changes, for testing only: a hand-started server that talks to a fake
//! GitHub on this machine and offers no editors, so a browser can be pointed at it without anything leaving
//! the machine or launching the developer's applications (`rust/tools/browser_fixture`).
//!
//! The flag is the only switch. The origin variable alone is never read for its effect: set without the flag
//! it is an error, so a stray variable cannot redirect where credentials are sent. The origin must be a
//! loopback origin and nothing more, so a GitHub token is only ever sent to this machine.
use crate::source_control_host::{Host, HostOptions};
use std::sync::Arc;

/// The fake GitHub API's origin, read only with `--fixture`.
pub const GITHUB_API_ORIGIN_VARIABLE: &str = "T3CODE_FIXTURE_GITHUB_API_ORIGIN";

/// The validated origin of a fake GitHub API: `http://127.0.0.1:<port>`, `http://[::1]:<port>` or
/// `http://localhost:<port>`, with no credentials, path, query or fragment.
pub fn github_api_origin(fixture: bool, variable: Option<String>) -> Result<Option<String>, String> {
    let Some(origin) = variable else { return Ok(None) };
    if !fixture {
        return Err(format!("{GITHUB_API_ORIGIN_VARIABLE} is only read with --fixture, a testing flag. Unset it, or pass --fixture."));
    }
    let refuse = |why: &str| Err(format!("{GITHUB_API_ORIGIN_VARIABLE} must be http://127.0.0.1:<port>, http://[::1]:<port> or http://localhost:<port>: {why}."));
    let Some(authority) = origin.strip_prefix("http://") else { return refuse("it must start with http://") };
    if authority.contains('@') {
        return refuse("it must not carry credentials");
    }
    if authority.contains(['/', '?', '#', '\\']) {
        return refuse("it must not have a path, query or fragment");
    }
    let Some((host, port)) = authority.rsplit_once(':') else { return refuse("it needs a port") };
    if !matches!(host, "127.0.0.1" | "[::1]" | "localhost") {
        return refuse("it must be a loopback host");
    }
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|byte| byte.is_ascii_digit()) || port.parse::<u16>().ok().filter(|port| *port > 0).is_none() {
        return refuse("the port must be a number from 1 to 65535");
    }
    Ok(Some(origin))
}

/// `(rest, graphql)` for `host` on the fake API: `{origin}/rest/{host}` and `{origin}/graphql/{host}`.
pub fn github_api_endpoints(origin: &str, host: &str) -> (String, String) {
    (format!("{origin}/rest/{host}"), format!("{origin}/graphql/{host}"))
}

/// Puts the fixture in place. It must run before the first use of `Host::process_default`.
pub fn install(github_api_origin: Option<&str>) -> Result<(), String> {
    crate::external_launcher::offer_no_editors();
    let Some(origin) = github_api_origin else { return Ok(()) };
    let origin = origin.to_owned();
    let host = Host::new(HostOptions { github_api: Some(Arc::new(move |host| github_api_endpoints(&origin, host))), ..HostOptions::default() });
    if Host::install(host) { Ok(()) } else { Err("The source control host was already in use before --fixture could install the GitHub origin.".into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_origins_are_accepted() {
        for good in ["http://127.0.0.1:1", "http://127.0.0.1:47851", "http://[::1]:8080", "http://localhost:65535"] {
            assert_eq!(github_api_origin(true, Some(good.into())), Ok(Some(good.into())), "{good}");
        }
        for bad in [
            "https://127.0.0.1:1",
            "http://127.0.0.1",
            "http://127.0.0.1:",
            "http://127.0.0.1:0",
            "http://127.0.0.1:65536",
            "http://127.0.0.1:+80",
            "http://127.0.0.1:1/",
            "http://127.0.0.1:1/api",
            "http://127.0.0.1:1?x=1",
            "http://127.0.0.1:1#x",
            "http://user@127.0.0.1:1",
            "http://user:pass@127.0.0.1:1",
            "http://127.0.0.1:1@evil.example:80",
            "http://127.0.0.1:1\\@evil.example",
            "http://api.github.com:80",
            "http://localhost.evil.example:80",
            "http://127.0.0.2:80",
            "http://0.0.0.0:80",
            "http://[::2]:80",
            "http://LOCALHOST:80",
            "http:// 127.0.0.1:80",
            "127.0.0.1:80",
            "",
        ] {
            assert!(github_api_origin(true, Some(bad.into())).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_variable_without_the_flag_is_an_error_and_without_the_variable_nothing_is_asked() {
        assert!(github_api_origin(false, Some("http://127.0.0.1:1".into())).unwrap_err().contains("--fixture"));
        assert_eq!(github_api_origin(false, None), Ok(None));
        assert_eq!(github_api_origin(true, None), Ok(None));
    }

    #[test]
    fn endpoints_name_the_host_under_the_origin() {
        assert_eq!(github_api_endpoints("http://127.0.0.1:9", "github.example"), ("http://127.0.0.1:9/rest/github.example".to_owned(), "http://127.0.0.1:9/graphql/github.example".to_owned()));
    }
}
