//! Pull-request identity and legacy metadata updates, preserving multi-link
//! records and their watches. Network synchronization remains a separate owner.
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;
use t3_contracts::trim_wire_string;

fn lower(value: &str) -> String {
    trim_wire_string(value).to_lowercase()
}

fn canonical(key: &str) -> String {
    static SSH: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^(?:ssh\.dev\.azure\.com|vs-ssh\.visualstudio\.com)/v3/([^/]+)/([^/]+)/([^/]+)$",
        )
        .unwrap()
    });
    static OLD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^([^.]+)\.visualstudio\.com/(?:defaultcollection/)?([^/]+)/_git/([^/]+)$")
            .unwrap()
    });
    if let Some(groups) = SSH.captures(key) {
        return format!(
            "dev.azure.com/{}/{}/_git/{}",
            &groups[1], &groups[2], &groups[3]
        );
    }
    if let Some(groups) = OLD.captures(key) {
        return format!(
            "dev.azure.com/{}/{}/_git/{}",
            &groups[1], &groups[2], &groups[3]
        );
    }
    key.into()
}

fn host_of(host: &str, apex: &str, label: Option<&str>) -> bool {
    host == apex
        || host.ends_with(&format!(".{apex}"))
        || label.is_some_and(|label| host.split('.').any(|part| part == label))
}

fn parsed(input: &str) -> Option<Value> {
    static GITHUB: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^/([^/]+/[^/]+)/pull/([0-9]+)(?:/|$)").unwrap());
    static FORGEJO: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^/([^/]+(?:/[^/]+)+)/pulls/([0-9]+)(?:/|$)").unwrap());
    static GITLAB: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^/([^/]+(?:/[^/]+)+)/-/merge_requests/([0-9]+)(?:/|$)").unwrap()
    });
    static BITBUCKET: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^/([^/]+/[^/]+)/pull-requests/([0-9]+)(?:/|$)").unwrap());
    static AZURE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^/((?:[^/]+/)*_git/[^/]+)/pullrequest/([0-9]+)(?:/|$)").unwrap()
    });
    let url = url::Url::parse(input).ok()?;
    if !matches!(url.scheme(), "https" | "http") {
        return None;
    }
    let host = url.host_str()?.to_lowercase();
    let claim = |regex: &Regex, authority: bool| -> Option<Value> {
        let groups = regex.captures(url.path())?;
        let number: u64 = groups[2].parse().ok()?;
        if number == 0 || number > 9_007_199_254_740_991 {
            return None;
        }
        let mut result = json!({"host":host,"repository":groups[1].to_lowercase(),"number":number});
        if authority {
            result["authority"] = json!(match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.clone(),
            });
        }
        Some(result)
    };
    if host_of(&host, "github.com", Some("github")) && GITHUB.is_match(url.path()) {
        return claim(&GITHUB, false);
    }
    if FORGEJO.is_match(url.path()) {
        return claim(&FORGEJO, true);
    }
    if GITLAB.is_match(url.path()) {
        return claim(&GITLAB, false);
    }
    if host_of(&host, "bitbucket.org", Some("bitbucket")) {
        return claim(&BITBUCKET, false);
    }
    if host_of(&host, "dev.azure.com", None) || host.ends_with(".visualstudio.com") {
        return claim(&AZURE, false);
    }
    None
}

/// `parseChangeRequestUrl`: the host, repository and number a pull request URL names.
pub(crate) fn parse_change_request_url(input: &str) -> Option<Value> {
    parsed(input)
}

pub(crate) fn normalize(key: &Value) -> Value {
    let repository = lower(key["repository"].as_str().unwrap());
    let parsed = key["url"].as_str().and_then(parsed);
    let recovered = parsed
        .as_ref()
        .filter(|parsed| parsed["repository"] == repository && parsed["number"] == key["number"])
        .and_then(|parsed| parsed["authority"].as_str());
    let authority = key["authority"]
        .as_str()
        .or(recovered)
        .unwrap_or(key["host"].as_str().unwrap());
    let canonical = canonical(&format!("{}/{repository}", lower(authority)));
    let (host, repository) = canonical.split_once('/').unwrap();
    json!({"host":host,"repository":repository,"number":key["number"]})
}

pub(crate) fn legacy_key(link: &Value, fallback_host: Option<&str>) -> Value {
    if let Some(parsed) = link["url"].as_str().and_then(parsed) {
        if parsed["number"] == link["number"]
            && (parsed.get("authority").is_some()
                || canonical(&format!(
                    "{}/{}",
                    parsed["host"].as_str().unwrap(),
                    parsed["repository"].as_str().unwrap()
                ))
                .starts_with("dev.azure.com/"))
        {
            return normalize(&parsed);
        }
    }
    let parsed_url = link["url"]
        .as_str()
        .and_then(|value| url::Url::parse(value).ok());
    let host = lower(fallback_host.unwrap_or_else(|| {
        parsed_url
            .as_ref()
            .and_then(url::Url::host_str)
            .unwrap_or("unknown")
    }));
    json!({"host":if host.is_empty(){"unknown"}else{&host},"repository":lower(link["repository"].as_str().unwrap()),"number":link["number"]})
}

fn equal(left: &Value, right: &Value) -> bool {
    normalize(left) == normalize(right)
}

fn legacy_entry(link: &Value, at: &str) -> Value {
    let mut entry = legacy_key(link, None);
    for (key, value) in [
        ("url", link["url"].clone()),
        ("source", json!("manual")),
        ("linkedAt", json!(at)),
        ("snapshot", Value::Null),
        ("stack", Value::Null),
    ] {
        entry[key] = value;
    }
    entry
}

pub(crate) fn links(thread: &Value) -> Vec<Value> {
    if let Some(links) = thread.get("pullRequests") {
        return links.as_array().cloned().unwrap_or_default();
    }
    match thread
        .get("linkedPullRequest")
        .filter(|value| !value.is_null())
    {
        Some(link) => vec![legacy_entry(link, "1970-01-01T00:00:00.000Z")],
        None => vec![],
    }
}

pub(crate) fn update_legacy(thread: &mut Value, linked: &Value, now: &str) {
    let previous_key = thread
        .get("linkedPullRequest")
        .filter(|value| !value.is_null())
        .map(|value| legacy_key(value, None));
    let next_key = (!linked.is_null()).then(|| legacy_key(linked, None));
    let current = links(thread);
    let mut next: Vec<_> = current
        .iter()
        .filter(|link| {
            !previous_key.as_ref().is_some_and(|key| equal(link, key))
                && !next_key.as_ref().is_some_and(|key| equal(link, key))
        })
        .cloned()
        .collect();
    if let Some(key) = next_key {
        let mut entry = legacy_entry(linked, now);
        if let Some(watch) = current
            .iter()
            .find(|link| equal(link, &key))
            .and_then(|link| link.get("watch"))
        {
            entry["watch"] = watch.clone();
        }
        next.push(entry);
    }
    thread["linkedPullRequest"] = linked.clone();
    thread["pullRequests"] = json!(next);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_legacy_keys_and_metadata_updates_preserve_watches_and_other_links() {
        let mut count = 0;
        for line in include_str!("../tests/fixtures/thread-pull-requests.jsonl")
            .split('\n')
            .filter(|line| !line.is_empty())
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let actual = match row["type"].as_str().unwrap() {
                "legacy" => legacy_key(&row["input"], row["fallback"].as_str()),
                "normalize" => normalize(&row["input"]),
                "update" => {
                    let mut thread = row["thread"].clone();
                    update_legacy(&mut thread, &row["linked"], row["now"].as_str().unwrap());
                    json!({"linkedPullRequest":thread["linkedPullRequest"],"pullRequests":thread["pullRequests"]})
                }
                _ => panic!("unknown witness"),
            };
            assert_eq!(actual, row["result"], "witness {count}");
            count += 1;
        }
        assert_eq!(count, 188);
    }
}
