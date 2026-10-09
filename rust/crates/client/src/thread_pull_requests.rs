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
