//! The pull requests a thread is filed under: its `pullRequests` links and the legacy
//! `linkedPullRequest` older clients wrote, which of them the header shows, and the commands that
//! link and unlink one. Port of the visible-link part of `threadPullRequests.ts`; stacks are not
//! chained here, every visible link is its own entry.
use crate::pull_request::Reference;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The legacy link names no host.
    pub host: Option<String>,
    pub repository: String,
    pub number: u64,
    pub url: String,
    pub state: Option<String>,
    pub title: Option<String>,
    /// The server is watching it for the thread, and wakes the agent when it changes.
    pub watching: bool,
}
impl Link {
    pub fn reference(&self, project_id: &str) -> Reference {
        Reference { project_id: project_id.to_owned(), host: self.host.clone(), repository: self.repository.clone(), number: self.number }
    }
    pub fn is(&self, reference: &Reference) -> bool {
        same_request(self.host.as_deref(), &self.repository, self.number, reference)
    }
}
/// Identity of a link: host-level, case-insensitive on host and repository. A side that names no
/// host (the legacy link) matches any.
fn same_request(host: Option<&str>, repository: &str, number: u64, reference: &Reference) -> bool {
    number == reference.number
        && repository.trim().to_lowercase() == reference.repository.trim().to_lowercase()
        && match (host, reference.host.as_deref()) {
            (Some(left), Some(right)) => left.trim().to_lowercase() == right.trim().to_lowercase(),
            _ => true,
        }
}

fn is_open(link: &Value) -> bool {
    link["snapshot"].is_null() || link["snapshot"]["state"] == "open"
}

/// The links the header offers: the thread's visible links (a stack member the user dismissed only
/// stays in the array so the sync does not add it back), open ones first and then the most
/// recently linked, and the legacy link when no entry names the same pull request.
pub fn links(thread: &Value) -> Vec<Link> {
    let mut entries: Vec<&Value> = thread["pullRequests"].as_array().into_iter().flatten().filter(|link| link["source"] != "stack-dismissed").collect();
    entries.sort_by(|left, right| is_open(right).cmp(&is_open(left)).then_with(|| right["linkedAt"].as_str().cmp(&left["linkedAt"].as_str())));
    let mut result: Vec<Link> = entries
        .into_iter()
        .filter_map(|link| {
            Some(Link {
                host: link["host"].as_str().map(str::to_owned),
                repository: link["repository"].as_str()?.to_owned(),
                number: link["number"].as_u64()?,
                url: link["url"].as_str()?.to_owned(),
                state: link["snapshot"]["state"].as_str().map(str::to_owned),
                title: link["snapshot"]["title"].as_str().map(str::to_owned),
                watching: link["watch"].is_object(),
            })
        })
        .collect();
    let legacy = &thread["linkedPullRequest"];
    if let (Some(repository), Some(number), Some(url)) = (legacy["repository"].as_str(), legacy["number"].as_u64(), legacy["url"].as_str()) {
        let link = Link { host: None, repository: repository.to_owned(), number, url: url.to_owned(), state: None, title: None, watching: false };
        if !result.iter().any(|existing| link.is(&existing.reference(""))) {
            result.push(link);
        }
    }
    result
}

// ---- the badge a thread row carries ---------------------------------------------------------------------------------------

/// What a pull request link says about itself in a row, header or tab: `PULL_REQUEST_STATE_PRESENTATION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Draft,
    Closed,
    Merged,
}
impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Open => "Open",
            State::Draft => "Draft",
            State::Closed => "Closed",
            State::Merged => "Merged",
        }
    }
    pub fn class(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Draft => "draft",
            State::Closed => "closed",
            State::Merged => "merged",
        }
    }
    /// `resolvePullRequestState`.
    pub fn of(state: &str, draft: bool) -> State {
        match (state, draft) {
            ("open", true) => State::Draft,
            ("closed", _) => State::Closed,
            ("merged", _) => State::Merged,
            _ => State::Open,
        }
    }
}

/// `resolveThreadPullRequestBadge`: the one thing a row shows for all the links a thread has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// Links that build on one another: the badge counts the layers and opens the list.
    Stack { layers: usize, state: State },
    /// One link, or several unrelated ones: `others` more than the shown one.
    PullRequest { others: usize, state: State },
}
impl Badge {
    pub fn state(self) -> State {
        match self {
            Badge::Stack { state, .. } | Badge::PullRequest { state, .. } => state,
        }
    }
    /// Whether a click opens the thread's list of pull requests rather than the one request.
    pub fn opens_list(self) -> bool {
        matches!(self, Badge::Stack { .. } | Badge::PullRequest { others: 1.., .. })
    }
}

fn visible(thread: &Value) -> Vec<&Value> {
    thread["pullRequests"].as_array().into_iter().flatten().filter(|link| link["source"] != "stack-dismissed").collect()
}
fn identity(link: &Value) -> String {
    format!("{}/{}#{}", link["host"].as_str().unwrap_or("unknown").trim().to_lowercase(), link["repository"].as_str().unwrap_or_default().trim().to_lowercase(), link["number"])
}
fn updated_at(link: &Value) -> i64 {
    link["snapshot"]["updatedAt"].as_str().or(link["linkedAt"].as_str()).and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok()).map_or(0, |at| at.timestamp_millis())
}
fn linked_at(link: &Value) -> i64 {
    link["linkedAt"].as_str().and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok()).map_or(i64::MIN, |at| at.timestamp_millis())
}

/// `resolveThreadPullRequestChains`: native stacks, then links chained by one's base branch being another's head branch in the
/// same repository, bottom to top. A link that chains to nothing is a chain of one.
fn chains<'a>(visible: &[&'a Value]) -> Vec<Vec<&'a Value>> {
    let mut chains: Vec<Vec<&Value>> = vec![];
    let mut placed = std::collections::HashSet::new();
    let mut native: Vec<(String, Vec<&Value>)> = vec![];
    for link in visible.iter().filter(|link| link["stack"].is_object()) {
        let key = format!("{}#stack:{}", identity(link).rsplit_once('#').map_or_else(String::new, |(head, _)| head.to_owned()), link["stack"]["id"]);
        match native.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, members)) => members.push(link),
            None => native.push((key, vec![link])),
        }
    }
    for (_, mut members) in native {
        let order: Vec<u64> = members[0]["stack"]["layers"].as_array().into_iter().flatten().filter_map(|layer| layer["number"].as_u64()).collect();
        let position = |link: &Value| link["number"].as_u64().and_then(|number| order.iter().position(|layer| *layer == number)).unwrap_or(0);
        members.sort_by_key(|link| position(link));
        placed.extend(members.iter().map(|member| identity(member)));
        chains.push(members);
    }
    let remaining: Vec<&Value> = visible.iter().copied().filter(|link| !placed.contains(&identity(link))).collect();
    let branch = |link: &Value, branch: &Value| format!("{}:{}", identity(link).rsplit_once('#').map_or_else(String::new, |(head, _)| head.to_owned()), branch.as_str().unwrap_or_default());
    // Reused head names cannot name a parent unambiguously.
    let mut by_head: std::collections::HashMap<String, Option<&Value>> = Default::default();
    for link in remaining.iter().filter(|link| !link["snapshot"].is_null()) {
        let key = branch(link, &link["snapshot"]["headBranch"]);
        let value = if by_head.contains_key(&key) { None } else { Some(*link) };
        by_head.insert(key, value);
    }
    let mut has_child = std::collections::HashSet::new();
    for link in remaining.iter().filter(|link| !link["snapshot"].is_null()) {
        if let Some(Some(parent)) = by_head.get(&branch(link, &link["snapshot"]["baseBranch"])) {
            if identity(parent) != identity(link) {
                has_child.insert(identity(parent));
            }
        }
    }
    for top in remaining.iter().filter(|top| !has_child.contains(&identity(top))) {
        let mut layers: Vec<&Value> = vec![];
        let mut cursor = Some(*top);
        while let Some(link) = cursor.filter(|link| !placed.contains(&identity(link))) {
            placed.insert(identity(link));
            layers.insert(0, link);
            cursor = if link["snapshot"].is_null() { None } else { by_head.get(&branch(link, &link["snapshot"]["baseBranch"])).copied().flatten() };
        }
        if !layers.is_empty() {
            chains.push(layers);
        }
    }
    // Cycles have no top; their links stay visible without an invented order.
    for link in remaining {
        if !placed.contains(&identity(link)) {
            chains.push(vec![link]);
        }
    }
    chains
}

/// `resolveThreadCurrentPullRequestLink`: the request a one-slot surface shows: open work first, the highest open layer of a
/// chain, a finished chain's top, and among unrelated finished links the most recently updated.
pub fn current(thread: &Value) -> Option<Link> {
    let visible = visible(thread);
    if visible.is_empty() {
        return None;
    }
    let open: Vec<&Value> = visible.iter().copied().filter(|link| is_open(link)).collect();
    let chains = chains(&visible);
    let chosen: &Value = if open.len() == 1 {
        open[0]
    } else if open.len() > 1 {
        let mut open_chains: Vec<Vec<&Value>> = chains.iter().map(|chain| chain.iter().rev().copied().filter(|link| is_open(link)).collect::<Vec<_>>()).filter(|layers| !layers.is_empty()).collect();
        open_chains.sort_by_key(|layers| std::cmp::Reverse(layers.iter().map(|link| linked_at(link)).max()));
        open_chains[0][0]
    } else if chains.len() == 1 {
        chains[0].last().copied()?
    } else {
        visible.iter().copied().max_by_key(|link| updated_at(link))?
    };
    to_link(chosen)
}

fn to_link(link: &Value) -> Option<Link> {
    Some(Link {
        host: link["host"].as_str().map(str::to_owned),
        repository: link["repository"].as_str()?.to_owned(),
        number: link["number"].as_u64()?,
        url: link["url"].as_str()?.to_owned(),
        state: link["snapshot"]["state"].as_str().map(str::to_owned),
        title: link["snapshot"]["title"].as_str().map(str::to_owned),
        watching: link["watch"].is_object(),
    })
}

/// The state glyph a link is drawn with: its snapshot's state, draft included; a link not yet synced reads as open.
pub fn state_of(thread: &Value, link: &Link) -> State {
    let found = visible(thread).into_iter().find(|candidate| candidate["number"].as_u64() == Some(link.number) && candidate["repository"].as_str().is_some_and(|repository| repository.eq_ignore_ascii_case(&link.repository)));
    found.map_or(State::Open, |found| State::of(found["snapshot"]["state"].as_str().unwrap_or("open"), found["snapshot"]["isDraft"] == true))
}

/// `resolveThreadPullRequestBadge`.
pub fn badge(thread: &Value) -> Option<Badge> {
    let visible = visible(thread);
    if visible.is_empty() {
        return None;
    }
    let states: Vec<&str> = visible.iter().map(|link| link["snapshot"]["state"].as_str().unwrap_or("open")).collect();
    let state = if visible.iter().all(|link| link["snapshot"]["state"] == "open" && link["snapshot"]["isDraft"] == true) {
        State::Draft
    } else if states.contains(&"open") {
        State::Open
    } else if states.iter().all(|state| *state == "merged") {
        State::Merged
    } else {
        State::Closed
    };
    if visible.len() > 1 && chains(&visible).len() == 1 {
        return Some(Badge::Stack { layers: visible.len(), state });
    }
    Some(Badge::PullRequest { others: visible.len() - 1, state })
}

/// Whether the thread is filed under this pull request.
pub fn linked_here(thread: &Value, reference: &Reference) -> bool {
    links(thread).iter().any(|link| link.is(reference))
}

/// The `thread.pull-request.link` or `.unlink` command for this pull request, when its host is known.
pub fn command(thread_id: &str, reference: &Reference, url: &str, link: bool) -> Option<(&'static str, Value)> {
    let host = reference.host.as_deref()?;
    let mut input = json!({"threadId":thread_id,"host":host,"repository":reference.repository,"number":reference.number});
    if link {
        input["url"] = json!(url);
        input["source"] = json!("manual");
    }
    Some((if link { "thread.pull-request.link" } else { "thread.pull-request.unlink" }, input))
}

/// The `thread.pull-request.watch` command: the server watches `link` for the thread, or stops.
pub fn watch_command(thread_id: &str, host: &str, repository: &str, number: u64, watching: bool) -> Value {
    json!({"type":"thread.pull-request.watch","threadId":thread_id,"host":host,"repository":repository,"number":number,"watching":watching})
}

/// The menu entries of a pull request's watch.
pub const WATCH: &str = "Watch for changes";
pub const STOP_WATCHING: &str = "Stop watching";
pub const WATCHING_HINT: &str = "Watching: the agent wakes when checks finish, someone comments, or the branch conflicts.";
pub const WATCH_FAILED: &str = "Could not change the pull request watch";

pub const LINK_FAILED: &str = "Could not link the pull request";
pub const UNLINK_FAILED: &str = "Could not unlink the pull request";
pub const LINK: &str = "Link to this thread";
pub const UNLINK: &str = "Unlink from this thread";

#[cfg(test)]
mod tests {
    use super::*;

    fn link(host: &str, repository: &str, number: u64, state: Option<&str>, linked_at: &str, source: &str) -> Value {
        json!({"host":host,"repository":repository,"number":number,"url":format!("https://{host}/{repository}/pull/{number}"),"source":source,"linkedAt":linked_at,"snapshot":state.map(|state| json!({"state":state,"title":format!("PR {number}")})),"stack":null})
    }

    #[test]
    fn open_links_come_first_then_the_newest_and_dismissed_ones_never() {
        let thread = json!({"pullRequests":[
            link("github.com", "acme/web", 1, Some("merged"), "2026-01-03T00:00:00Z", "manual"),
            link("github.com", "acme/web", 2, Some("open"), "2026-01-01T00:00:00Z", "manual"),
            link("github.com", "acme/web", 3, None, "2026-01-02T00:00:00Z", "created"),
            link("github.com", "acme/web", 4, Some("open"), "2026-01-04T00:00:00Z", "stack-dismissed"),
        ]});
        assert_eq!(links(&thread).iter().map(|link| link.number).collect::<Vec<_>>(), [3, 2, 1]);
    }

    #[test]
    fn the_legacy_link_shows_only_when_nothing_else_names_that_pull_request() {
        let legacy = json!({"projectId":"p","repository":"Acme/Web","number":2,"url":"https://github.com/acme/web/pull/2"});
        let both = json!({"pullRequests":[link("github.com", "acme/web", 2, Some("open"), "2026-01-01T00:00:00Z", "manual")],"linkedPullRequest":legacy});
        assert_eq!(links(&both).len(), 1, "the same pull request, whatever the case of its repository");
        let only = json!({"linkedPullRequest":legacy});
        let found = links(&only);
        assert_eq!((found.len(), found[0].host.clone(), found[0].number), (1, None, 2));
        let other = json!({"pullRequests":[link("github.com", "acme/web", 9, None, "2026-01-01T00:00:00Z", "manual")],"linkedPullRequest":legacy});
        assert_eq!(links(&other).iter().map(|link| link.number).collect::<Vec<_>>(), [9, 2]);
    }

    fn stacked(number: u64, head: &str, base: &str, state: &str) -> Value {
        let mut link = link("github.com", "acme/web", number, Some(state), "2026-01-01T00:00:00Z", "manual");
        link["snapshot"]["headBranch"] = json!(head);
        link["snapshot"]["baseBranch"] = json!(base);
        link
    }

    #[test]
    fn a_row_shows_a_stack_by_its_layers_and_unrelated_links_by_their_count() {
        let stack = json!({"pullRequests":[stacked(1, "a", "main", "open"), stacked(2, "b", "a", "open"), stacked(3, "c", "b", "merged")]});
        assert_eq!(badge(&stack), Some(Badge::Stack { layers: 3, state: State::Open }));
        assert!(badge(&stack).unwrap().opens_list());
        // The highest open layer is the one the row links to.
        assert_eq!(current(&stack).unwrap().number, 2);
        let unrelated = json!({"pullRequests":[stacked(1, "a", "main", "merged"), stacked(2, "b", "main", "closed")]});
        assert_eq!(badge(&unrelated), Some(Badge::PullRequest { others: 1, state: State::Closed }));
        let one = json!({"pullRequests":[stacked(7, "a", "main", "open")]});
        assert_eq!(badge(&one), Some(Badge::PullRequest { others: 0, state: State::Open }));
        assert!(!badge(&one).unwrap().opens_list());
        let mut draft = stacked(8, "a", "main", "open");
        draft["snapshot"]["isDraft"] = json!(true);
        assert_eq!(badge(&json!({"pullRequests":[draft]})).map(Badge::state), Some(State::Draft));
        assert_eq!(badge(&json!({"pullRequests":[]})), None);
    }

    #[test]
    fn linked_here_compares_host_and_repository_without_case() {
        let thread = json!({"pullRequests":[link("GitHub.com", "Acme/Web", 5, None, "2026-01-01T00:00:00Z", "manual")]});
        let reference = |host: Option<&str>, number| Reference { project_id: "p".into(), host: host.map(str::to_owned), repository: "acme/web".into(), number };
        assert!(linked_here(&thread, &reference(Some("github.com"), 5)));
        assert!(!linked_here(&thread, &reference(Some("gitlab.com"), 5)));
        assert!(!linked_here(&thread, &reference(Some("github.com"), 6)));
    }

    #[test]
    fn linking_and_unlinking_name_the_pull_request_by_its_host_level_identity() {
        let reference = Reference { project_id: "p".into(), host: Some("github.com".into()), repository: "acme/web".into(), number: 7 };
        let (kind, input) = command("thread-a", &reference, "https://github.com/acme/web/pull/7", true).unwrap();
        assert_eq!((kind, input["source"].as_str(), input["url"].as_str()), ("thread.pull-request.link", Some("manual"), Some("https://github.com/acme/web/pull/7")));
        let (kind, input) = command("thread-a", &reference, "", false).unwrap();
        assert_eq!((kind, input.get("url"), input.get("source")), ("thread.pull-request.unlink", None, None));
        assert!(command("thread-a", &Reference { host: None, ..reference }, "", true).is_none(), "a pull request with no host cannot be filed");
    }
}
