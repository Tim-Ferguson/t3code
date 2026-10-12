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

/// `siblingPullRequestUrl`: the URL of another change request in the same repository, in the form the given one is written in.
pub(crate) fn sibling_pull_request_url(input: &str, number: u64) -> Option<String> {
    static ROUTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/(-/merge_requests|pulls?|pull-requests|pullrequest)/\d+(?:/|$)").unwrap());
    let reference = parsed(input)?;
    if number == 0 || number > 9_007_199_254_740_991 {
        return None;
    }
    let mut url = url::Url::parse(input).ok()?;
    let repository = reference["repository"].as_str()?.to_owned();
    let route = {
        let rest = url.path().get(repository.len() + 1..)?;
        ROUTE.captures(rest)?.get(1)?.as_str().to_owned()
    };
    url.set_path(&format!("/{repository}/{route}/{number}"));
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
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

/// `changeRequestUrlFor`: the web URL a host writes for a change request, `None` for a host shape not known.
pub(crate) fn change_request_url_for(kind: Option<&str>, host: &str, repository: &str, number: u64, remote_url: Option<&str>) -> Option<String> {
    Some(match kind? {
        "github" => format!("https://{host}/{repository}/pull/{number}"),
        "forgejo" => {
            let web = remote_url.and_then(|remote| url::Url::parse(remote).ok()).filter(|remote| {
                matches!(remote.scheme(), "http" | "https") && {
                    let host = host.to_lowercase();
                    remote.host_str().is_some_and(|name| name.to_lowercase() == host) || crate::source_control_detect::js_host(remote).to_lowercase() == host
                }
            });
            match web {
                Some(remote) => format!("{}/{repository}/pulls/{number}", remote.origin().ascii_serialization()),
                None => format!("https://{host}/{repository}/pulls/{number}"),
            }
        }
        "gitlab" => format!("https://{host}/{repository}/-/merge_requests/{number}"),
        "bitbucket" => format!("https://{host}/{repository}/pull-requests/{number}"),
        "azure-devops" => format!("https://{}/pullrequest/{number}", crate::source_control_detect::canonical_repository_key(&format!("{host}/{repository}").to_lowercase())),
        _ => return None,
    })
}

/// A stack of pull requests, bottom to top: one the host reports (`native`) or one chained by
/// matching a base branch to another's head branch (`derived`).
pub(crate) struct Chain {
    pub kind: &'static str,
    pub layers: Vec<Value>,
}

fn key_of(link: &Value) -> String {
    let key = normalize(link);
    format!("{}/{}#{}", key["host"].as_str().unwrap_or_default(), key["repository"].as_str().unwrap_or_default(), key["number"])
}

/// Links a user should see: tombstoned stack members stay in the array only so a sync does not re-add them.
pub(crate) fn visible(links: &[Value]) -> Vec<Value> {
    links.iter().filter(|link| link["source"] != "stack-dismissed").cloned().collect()
}

/// `resolveThreadPullRequestChains`: native stacks first, then links chained by branch, then the
/// links nothing chains to as one-layer chains.
pub(crate) fn chains(links: &[Value]) -> Vec<Chain> {
    use std::collections::{HashMap, HashSet};
    let visible = visible(links);
    let mut chains: Vec<Chain> = vec![];
    let mut placed: HashSet<String> = HashSet::new();
    let mut native: Vec<(String, Vec<Value>)> = vec![];
    for link in visible.iter().filter(|link| !link["stack"].is_null()) {
        let key = normalize(link);
        let stack_key = format!("{}/{}#stack:{}", key["host"].as_str().unwrap_or_default(), key["repository"].as_str().unwrap_or_default(), link["stack"]["id"].as_str().unwrap_or_default());
        match native.iter_mut().find(|(existing, _)| *existing == stack_key) {
            Some((_, members)) => members.push(link.clone()),
            None => native.push((stack_key, vec![link.clone()])),
        }
    }
    for (_, mut members) in native {
        let order: HashMap<u64, usize> = members[0]["stack"]["layers"].as_array().into_iter().flatten().enumerate().filter_map(|(index, layer)| Some((layer["number"].as_u64()?, index))).collect();
        members.sort_by_key(|member| member["number"].as_u64().and_then(|number| order.get(&number).copied()).unwrap_or(0));
        placed.extend(members.iter().map(key_of));
        chains.push(Chain { kind: "native", layers: members });
    }
    let remaining: Vec<Value> = visible.iter().filter(|link| !placed.contains(&key_of(link))).cloned().collect();
    let branch_key = |link: &Value, branch: &Value| {
        let key = normalize(link);
        format!("{}/{}:{}", key["host"].as_str().unwrap_or_default(), key["repository"].as_str().unwrap_or_default(), branch.as_str().unwrap_or_default())
    };
    // Reused head names cannot identify a parent unambiguously.
    let mut by_head: HashMap<String, Option<Value>> = HashMap::new();
    for link in remaining.iter().filter(|link| !link["snapshot"].is_null()) {
        let key = branch_key(link, &link["snapshot"]["headBranch"]);
        let value = if by_head.contains_key(&key) { None } else { Some(link.clone()) };
        by_head.insert(key, value);
    }
    let mut has_child: HashSet<String> = HashSet::new();
    for link in remaining.iter().filter(|link| !link["snapshot"].is_null()) {
        if let Some(Some(parent)) = by_head.get(&branch_key(link, &link["snapshot"]["baseBranch"])) {
            if key_of(parent) != key_of(link) {
                has_child.insert(key_of(parent));
            }
        }
    }
    for top in &remaining {
        if has_child.contains(&key_of(top)) {
            continue;
        }
        let mut layers: Vec<Value> = vec![];
        let mut cursor = Some(top.clone());
        while let Some(link) = cursor.filter(|link| !placed.contains(&key_of(link))) {
            placed.insert(key_of(&link));
            cursor = if link["snapshot"].is_null() { None } else { by_head.get(&branch_key(&link, &link["snapshot"]["baseBranch"])).cloned().flatten() };
            layers.insert(0, link);
        }
        if !layers.is_empty() {
            chains.push(Chain { kind: "derived", layers });
        }
    }
    // Cycles have no top. Keep those links visible without inventing a stack order.
    for link in &remaining {
        if !placed.contains(&key_of(link)) {
            chains.push(Chain { kind: "derived", layers: vec![link.clone()] });
        }
    }
    chains
}

/// `latestExecutedRun`: the run that most recently ran, skipping queued runs and those cancelled
/// before they started. An unfinished run is the latest; equal ends are ordered by ordinal.
pub(crate) fn latest_executed_run(projection: &Value) -> Option<&Value> {
    let end = |run: &Value| run["completedAt"].as_str().and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok()).map(|at| at.timestamp_millis());
    let later = |run: &Value, other: &Value| match (end(run), end(other)) {
        (None, None) => run["ordinal"].as_u64() > other["ordinal"].as_u64(),
        (None, Some(_)) => true,
        (Some(_), None) => false,
        (Some(left), Some(right)) => left > right || (left == right && run["ordinal"].as_u64() > other["ordinal"].as_u64()),
    };
    let mut latest: Option<&Value> = None;
    for run in projection["runs"].as_array().into_iter().flatten() {
        if run["status"] == "queued" || (run["status"] == "cancelled" && run["startedAt"].is_null()) {
            continue;
        }
        if latest.is_none_or(|current| later(run, current)) {
            latest = Some(run);
        }
    }
    latest
}

/// `stopReachedRun`: whether Stop reached the run, as its interrupt request item records. Its agent
/// can call tools until the provider stops it, but nothing such a call starts may outlive the Stop.
pub(crate) fn stop_reached_run(projection: &Value, run_id: &str) -> bool {
    projection["turnItems"].as_array().into_iter().flatten().any(|item| item["type"] == "run_interrupt_request" && item["runId"] == run_id)
}

/// Whether Stop reached the latest run that executed.
pub(crate) fn stop_reached_latest_run(projection: &Value) -> bool {
    latest_executed_run(projection).and_then(|run| run["id"].as_str()).is_some_and(|id| stop_reached_run(projection, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn link(number: u64, head: &str, base: &str) -> Value {
        json!({"host":"github.com","repository":"acme/web","number":number,"url":format!("https://github.com/acme/web/pull/{number}"),"source":"agent","linkedAt":"2026-01-01T00:00:00.000Z","snapshot":{"state":"open","headBranch":head,"baseBranch":base},"stack":null})
    }

    #[test]
    fn a_sibling_url_keeps_the_hosts_route_and_drops_everything_after_it() {
        assert_eq!(sibling_pull_request_url("https://github.com/Acme/Web/pull/4/files?x=1#top", 7).as_deref(), Some("https://github.com/acme/web/pull/7"));
        assert_eq!(sibling_pull_request_url("https://gitlab.com/g/p/-/merge_requests/4", 9).as_deref(), Some("https://gitlab.com/g/p/-/merge_requests/9"));
        assert_eq!(sibling_pull_request_url("https://bitbucket.org/w/r/pull-requests/4", 2).as_deref(), Some("https://bitbucket.org/w/r/pull-requests/2"));
        assert_eq!(sibling_pull_request_url("https://github.com/acme/web/pull/4", 0), None);
        assert_eq!(sibling_pull_request_url("https://example.com/acme/web/pull/4", 5), None);
    }

    #[test]
    fn links_chain_by_branch_into_derived_stacks_and_native_stacks_win() {
        // Three layers given out of order chain bottom to top; an unrelated link stands alone.
        let links = vec![link(3, "c", "b"), link(1, "a", "main"), link(9, "z", "main"), link(2, "b", "a")];
        let found = chains(&links);
        let numbers: Vec<Vec<u64>> = found.iter().map(|chain| chain.layers.iter().map(|layer| layer["number"].as_u64().unwrap()).collect()).collect();
        assert_eq!(numbers, [vec![1, 2, 3], vec![9]]);
        assert!(found.iter().all(|chain| chain.kind == "derived"));
        // A head name reused by two links cannot name a parent.
        let ambiguous = chains(&[link(1, "a", "main"), link(2, "a", "main"), link(3, "c", "a")]);
        assert_eq!(ambiguous.iter().map(|chain| chain.layers.len()).sum::<usize>(), 3);
        assert!(ambiguous.iter().all(|chain| chain.layers.len() == 1));
        // A native stack keeps the host's order and is not chained again.
        let mut native: Vec<Value> = vec![link(5, "e", "main"), link(4, "d", "e")];
        for link in &mut native {
            link["stack"] = json!({"kind":"github","id":"s","number":4,"url":"https://github.com/acme/web/pull/4","base":"main","layers":[{"number":4,"headBranch":"d","state":"open"},{"number":5,"headBranch":"e","state":"open"}]});
        }
        let stacks = chains(&native);
        assert_eq!((stacks.len(), stacks[0].kind, stacks[0].layers.iter().map(|layer| layer["number"].as_u64().unwrap()).collect::<Vec<_>>()), (1, "native", vec![4, 5]));
        // Dismissed stack members are not shown.
        let mut dismissed = link(1, "a", "main");
        dismissed["source"] = json!("stack-dismissed");
        assert!(chains(&[dismissed]).is_empty());
    }

    #[test]
    fn web_urls_follow_each_hosts_shape_and_stop_reaches_the_latest_executed_run() {
        assert_eq!(change_request_url_for(Some("github"), "github.com", "a/b", 7, None).as_deref(), Some("https://github.com/a/b/pull/7"));
        assert_eq!(change_request_url_for(Some("gitlab"), "gitlab.com", "g/p", 7, None).as_deref(), Some("https://gitlab.com/g/p/-/merge_requests/7"));
        assert_eq!(change_request_url_for(Some("bitbucket"), "bitbucket.org", "w/r", 7, None).as_deref(), Some("https://bitbucket.org/w/r/pull-requests/7"));
        assert_eq!(change_request_url_for(Some("forgejo"), "git.example.com:3000", "o/r", 7, Some("http://git.example.com:3000/o/r.git")).as_deref(), Some("http://git.example.com:3000/o/r/pulls/7"));
        assert_eq!(change_request_url_for(Some("forgejo"), "git.example.com", "o/r", 7, Some("git@git.example.com:o/r.git")).as_deref(), Some("https://git.example.com/o/r/pulls/7"));
        assert_eq!(change_request_url_for(None, "example.com", "o/r", 7, None), None);
        let projection = json!({"runs":[
            {"id":"old","ordinal":1,"status":"completed","startedAt":"2026-01-01T00:00:00.000Z","completedAt":"2026-01-01T00:00:01.000Z"},
            {"id":"never","ordinal":3,"status":"cancelled","startedAt":null,"completedAt":"2026-01-01T00:00:05.000Z"},
            {"id":"queued","ordinal":4,"status":"queued","startedAt":null,"completedAt":null},
            {"id":"latest","ordinal":2,"status":"running","startedAt":"2026-01-01T00:00:02.000Z","completedAt":null}],
            "turnItems":[{"type":"run_interrupt_request","runId":"old"}]});
        assert_eq!(latest_executed_run(&projection).unwrap()["id"], "latest");
        assert!(stop_reached_run(&projection, "old") && !stop_reached_latest_run(&projection));
    }

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
