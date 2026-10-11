//! `packages/shared/src/relayUrl.ts` and the issuer normalisation in `relayJwt.ts`.

use url::Url;

/// A relay URL as its HTTPS origin, or `None` when it is anything but a bare secure origin
/// (credentials, query, fragment and a path other than `/` are refused).
pub fn normalize_secure_relay_url(value: &str) -> Option<String> {
    let url = Url::parse(value.trim()).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some_and(|q| !q.is_empty())
        // WHATWG `URL.search` is empty for a bare `?`, so an empty query passes in the original.
        || url.fragment().is_some_and(|f| !f.is_empty())
        || !url.path().chars().all(|c| c == '/')
    {
        return None;
    }
    Some(origin_string(&url))
}

pub fn is_secure_relay_url(value: &str) -> bool {
    normalize_secure_relay_url(value).is_some()
}

/// `URL.origin`: scheme, host and a non-default port.
pub fn origin_string(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Issuer / audience form of a relay URL: trimmed, trailing slashes removed.
pub fn normalize_relay_issuer(value: &str) -> String {
    value.trim().trim_end_matches('/').to_owned()
}
