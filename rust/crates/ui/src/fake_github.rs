//! A GitHub that answers by what it is asked, for tests that mount the panel against the real
//! server. The answer shapes are the ones `pull_request_flow.rs` and `pull_request_actions_flow.rs`
//! in the server crate script for the same reads and writes; this file adds only what a UI test
//! changes between steps (title, state, permission, comments, a refusal). The server is pointed
//! at it through the stub in `source_control_tests`, so no real `gh` and no network is involved.
use super::source_control_tests::{Asked, World};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use t3_server::pull_request_queries as queries;

pub(crate) struct Data {
    pub(crate) number: u64,
    pub(crate) title: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub(crate) state: &'static str,
    pub(crate) draft: bool,
    pub(crate) permission: &'static str,
    pub(crate) comments: Vec<Value>,
    pub(crate) checks: Vec<Value>,
    pub(crate) threads: Vec<Value>,
    pub(crate) viewed: Vec<(String, bool)>,
    /// The sentence the host refuses a merge with, when it does.
    pub(crate) merge_refusal: Option<String>,
    /// The patch the host serves for the pull request.
    pub(crate) patch: String,
    pub(crate) reviewers: Vec<Value>,
    /// The description, as the host holds it.
    pub(crate) body: String,
    /// The sentence the host refuses an edit or a reaction with, when it does.
    pub(crate) write_refusal: Option<String>,
    /// `reactionGroups` of the description and of each remark, by the host's id for the subject.
    pub(crate) reactions: Vec<(String, Vec<Value>)>,
    /// The host's stack of this pull request, in the shape of its REST `stacks` answer; none is a host that has no stack for it.
    pub(crate) stack: Option<Value>,
    /// What the host's list of stacks says when it says something other than `stack` (a stack that moved after the panel read its
    /// details): the server's own read before an action is the list, the panel's is the details.
    pub(crate) stack_list: Option<Value>,
    /// What the host answers a stack merge with: `merged`, `pending` or `failed`.
    pub(crate) stack_merge: &'static str,
    /// Whether the account may push to every branch of the stack (the permission read before a rebase).
    pub(crate) stack_branches_writable: bool,
}
impl Data {
    fn new(number: u64) -> Self {
        Self {
            number,
            title: "Fix it".into(),
            state: "OPEN",
            draft: false,
            permission: "WRITE",
            comments: vec![json!({"id":"IC_1","body":"first remark","createdAt":"2026-01-02T00:00:00Z","lastEditedAt":null,"url":"https://c/1","author":actor("bob")})],
            checks: vec![check("build", "COMPLETED", Some("SUCCESS")), check("lint", "IN_PROGRESS", None)],
            threads: vec![json!({"id":"T_1","isResolved":false,"isOutdated":false,"path":"a.ts","line":1,"diffSide":"RIGHT","comments":{"totalCount":1,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"RC_1","author":actor("bob"),"body":"nit about a","createdAt":"2026-01-02T00:30:00Z","url":"https://t/1","reactionGroups":[]}]}})],
            viewed: vec![("a.ts".into(), false), ("b.ts".into(), false)],
            merge_refusal: None,
            patch: "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/b.ts b/b.ts\n--- a/b.ts\n+++ b/b.ts\n@@ -1 +1 @@\n-c\n+d\n".into(),
            reviewers: vec![],
            body: "Please **merge** this.".into(),
            write_refusal: None,
            reactions: vec![],
            stack: None,
            stack_list: None,
            stack_merge: "merged",
            stack_branches_writable: true,
        }
    }
    fn groups(&self, subject: &str) -> Value {
        self.reactions.iter().find(|(id, _)| id == subject).map_or(json!([]), |(_, groups)| json!(groups))
    }
}

/// A stack as the host's REST API gives it: `(number, state, draft, head sha)` of each layer, bottom to top, on `main`.
pub(crate) fn stack_of(number: u64, layers: &[(u64, &str, bool, &str)]) -> Value {
    json!({"number":number,"url":format!("https://github.com/acme/web/stacks/{number}"),"base":{"ref":"main"},"pull_requests":layers.iter().map(|(layer, state, draft, sha)| json!({
        "number":layer,"title":format!("Layer {layer}"),"draft":draft,"state":if *state == "merged" { "closed" } else { *state },"merged_at":if *state == "merged" { json!("2026-01-03T00:00:00Z") } else { Value::Null },
        "head":{"sha":sha,"ref":format!("layer-{layer}")}})).collect::<Vec<_>>()})
}

pub(crate) fn actor(login: &str) -> Value {
    json!({"__typename":"User","login":login,"avatarUrl":format!("https://avatars.example/{login}"),"name":login.to_uppercase(),"id":format!("ID_{login}")})
}
fn check(name: &str, status: &str, conclusion: Option<&str>) -> Value {
    json!({"__typename":"CheckRun","name":name,"status":status,"conclusion":conclusion,"detailsUrl":format!("https://ci/{name}"),"checkSuite":{"workflowRun":{"workflow":{"name":"CI"}}}})
}

/// A write the host holds back until the test lets it through, so a test can look at the panel
/// while the request is in flight.
#[derive(Default)]
pub(crate) struct Gate {
    open: Mutex<bool>,
    changed: std::sync::Condvar,
}
impl Gate {
    pub(crate) fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    fn pass(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
}

#[derive(Clone)]
pub(crate) struct Github(Arc<Mutex<Data>>, Arc<Mutex<Option<Arc<Gate>>>>);

/// Whether the request is this document; the server appends a rate-limit field, so a document is
/// known by all of it but its last brace.
fn is(query: &str, document: &str) -> bool {
    query.starts_with(&document[..document.rfind('}').unwrap_or(document.len())])
}
pub(crate) fn query_of(asked: &Asked) -> String {
    asked.body.as_ref().and_then(|body| body["query"].as_str()).unwrap_or("").to_owned()
}
fn variables_of(asked: &Asked) -> Value {
    asked.body.as_ref().map(|body| body["variables"].clone()).unwrap_or(Value::Null)
}

/// What the request is, as a word a test can count.
pub(crate) fn kind_of(asked: &Asked) -> String {
    if !asked.path.starts_with("/graphql/") {
        return format!("{} {}", asked.method, asked.path.trim_start_matches("/rest/github.com/"));
    }
    let query = query_of(asked);
    if query.starts_with("mutation") {
        let known = [
            ("closePullRequest", "close"),
            ("reopenPullRequest", "reopen"),
            ("markPullRequestReadyForReview", "ready"),
            ("convertPullRequestToDraft", "draft"),
            ("mergePullRequest", "merge"),
            ("enablePullRequestAutoMerge", "autoMerge"),
            ("disablePullRequestAutoMerge", "disableAutoMerge"),
            ("updatePullRequestBranch", "updateBranch"),
            ("addComment", "comment"),
            ("updatePullRequestReviewComment", "editReviewComment"),
            ("updateIssueComment", "editComment"),
            ("addPullRequestReviewThreadReply", "reply"),
            ("unresolveReviewThread", "unresolve"),
            ("resolveReviewThread", "resolve"),
            ("updatePullRequest(", "update"),
            ("addReaction", "react"),
            ("removeReaction", "unreact"),
            ("markFileAsViewed", "viewed"),
            ("unmarkFileAsViewed", "viewed"),
        ];
        return known.iter().find(|(needle, _)| query.contains(needle)).map_or("mutation", |(_, kind)| *kind).to_owned();
    }
    if query.starts_with("query($q: String!, $after: String)") {
        return "search".into();
    }
    if query.starts_with("query {\n  s0: repository") {
        return "stats".into();
    }
    if query.contains("query PullRequestActionState") {
        return "state".into();
    }
    if query.starts_with("query PullRequestSummaries") {
        return "summary".into();
    }
    if query.starts_with("query PullRequestsByHead") {
        return "byHead".into();
    }
    let known = [
        (queries::VIEWER_PERMISSIONS_GRAPHQL_QUERY, "viewerPermissions"),
        (queries::PULL_REQUEST_NODE_ID_GRAPHQL_QUERY, "nodeId"),
        (queries::REVIEWER_CANDIDATES_GRAPHQL_QUERY, "reviewerCandidates"),
        (queries::LABEL_CANDIDATES_GRAPHQL_QUERY, "labelCandidates"),
        (queries::PULL_REQUEST_HEADS_GRAPHQL_QUERY, "heads"),
        (queries::CORE_GITHUB_COM, "core"),
        (queries::SUMMARY_SINGLE_WITH_STACKS, "summary"),
        (queries::PULL_REQUEST_PREVIEW_GRAPHQL_QUERY, "preview"),
        (queries::PULL_REQUEST_ACTIVITY_GRAPHQL_QUERY, "activity"),
        (queries::REVIEW_THREADS_GRAPHQL_QUERY, "threads"),
        (queries::REVIEW_THREAD_COMMENTS_GRAPHQL_QUERY, "threadComments"),
        (queries::PULL_REQUEST_FILES_VIEWED_GRAPHQL_QUERY, "filesViewed"),
        (queries::CHECK_CONTEXTS_GITHUB_COM, "checkContexts"),
        (queries::REACTION_SUBJECT_PULL_REQUEST_GRAPHQL_QUERY, "subject"),
    ];
    known.iter().find(|(text, _)| is(&query, text)).map(|(_, name)| (*name).to_owned()).unwrap_or_else(|| query.lines().find(|line| !line.is_empty()).unwrap_or("").to_owned())
}

impl Github {
    /// Installs a fresh host for pull request `number` as the answer to everything the server asks.
    pub(crate) fn install(world: &World, number: u64) -> Self {
        let github = Self(Arc::new(Mutex::new(Data::new(number))), Arc::default());
        let (data, hold) = (github.0.clone(), github.1.clone());
        world.script(Arc::new(move |asked| {
            // Held before the data is locked, so the other reads go on while a write waits.
            let gate = hold.lock().unwrap().clone().filter(|_| matches!(kind_of(asked).as_str(), "merge" | "draft" | "close" | "react" | "unreact" | "update" | "editComment"));
            if let Some(gate) = gate {
                gate.pass();
            }
            Some(answer(&mut data.lock().unwrap(), asked))
        }));
        github
    }
    /// Every check passes, which is what offers a plain merge instead of auto-merge.
    pub(crate) fn passing(&self) {
        self.set(|data| data.checks = vec![check("build", "COMPLETED", Some("SUCCESS")), check("lint", "COMPLETED", Some("SUCCESS"))]);
    }
    /// Holds every merge, draft and close from now until the returned gate is released.
    pub(crate) fn hold_writes(&self) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        *self.1.lock().unwrap() = Some(gate.clone());
        gate
    }
    pub(crate) fn set(&self, change: impl FnOnce(&mut Data)) {
        change(&mut self.0.lock().unwrap());
    }
}

fn core(data: &Data) -> Value {
    let write = data.permission == "WRITE";
    json!({"data":{"repository":{"mergeCommitAllowed":true,"squashMergeAllowed":true,"rebaseMergeAllowed":false,"viewerPermission":data.permission,"pullRequest":{
        "number":data.number,"title":data.title,"url":format!("https://github.com/acme/web/pull/{}", data.number),"body":data.body,"state":data.state,"isDraft":data.draft,"mergeable":"MERGEABLE",
        "reviewDecision":"REVIEW_REQUIRED","additions":10,"deletions":2,"changedFiles":2,"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z","mergedAt":if data.state == "MERGED" { json!("2026-01-03T00:00:00Z") } else { Value::Null },"closedAt":null,
        "headRefName":"feature","baseRefName":"main","headRefOid":"abc1234","isCrossRepository":false,"headRepositoryOwner":{"login":"acme"},"author":actor("alice"),"autoMergeRequest":null,
        "viewerCanUpdate":write,"viewerDidAuthor":false,"viewerCanUpdateBranch":write,"baseRef":{"compare":{"behindBy":0}},
        "reviewRequests":{"nodes":data.reviewers},"labels":{"nodes":[{"name":"bug","color":"ff0000"}]},
        "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":data.checks,"pageInfo":{"hasNextPage":false}}}}}]}
    }}}})
}

fn summary(data: &Data) -> Value {
    json!({"data":{"s0":{"pullRequest":{"number":data.number,"title":data.title,"url":format!("https://github.com/acme/web/pull/{}", data.number),"state":data.state,"isDraft":data.draft,"mergeable":"MERGEABLE","reviewDecision":"APPROVED",
        "additions":5,"deletions":1,"changedFiles":2,"updatedAt":"2026-01-02T00:00:00Z","mergedAt":null,"closedAt":null,"headRefName":"feature","baseRefName":"main",
        "author":{"__typename":"User","login":"alice","avatarUrl":null,"name":"Alice"},"latestReviews":{"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"PENDING"}}}]},"stack":null}}}})
}

fn answer(data: &mut Data, asked: &Asked) -> (u16, String) {
    let json = |body: Value| (200, body.to_string());
    if asked.path == "/rest/github.com/user" {
        return json(json!({"id":4242,"login":"octo"}));
    }
    if asked.path.starts_with("/graphql/") && query_of(asked).contains("maintainerCanModify") {
        // The access to every branch of the stack, asked before a rebase touches one.
        let layers = data.stack.as_ref().and_then(|stack| stack["pull_requests"].as_array().cloned()).unwrap_or_default();
        let access = json!({"headRepository":{"viewerPermission":if data.stack_branches_writable { "WRITE" } else { "READ" }},"maintainerCanModify":false});
        return json(json!({"data":{"repository":layers.iter().map(|layer| (format!("pr{}", layer["number"]), access.clone())).collect::<serde_json::Map<_, _>>()}}));
    }
    if asked.path.starts_with("/graphql/") {
        let variables = variables_of(asked);
        let query = query_of(asked);
        return match kind_of(asked).as_str() {
            "byHead" => {
                let head = variables["h0"].as_str().unwrap_or("");
                let nodes = if head == "feature" {
                    json!([{"number":data.number,"title":data.title,"url":format!("https://github.com/acme/web/pull/{}", data.number),"baseRefName":"main","headRefName":"feature","headRefOid":"abc1234","state":data.state,"isDraft":data.draft,
                        "mergedAt":null,"closedAt":null,"updatedAt":"2026-01-02T00:00:00Z","isCrossRepository":false,"headRepository":{"name":"web","nameWithOwner":"acme/web"},"headRepositoryOwner":{"login":"acme"}}])
                } else {
                    json!([])
                };
                json(json!({"data":{"repository":{"h0":{"nodes":nodes}}}}))
            }
            "viewerPermissions" => {
                let write = data.permission == "WRITE";
                json(json!({"data":{"repository":{"mergeCommitAllowed":true,"squashMergeAllowed":true,"rebaseMergeAllowed":false,"viewerPermission":data.permission,"pullRequest":{"viewerCanUpdate":write,"viewerDidAuthor":false}}}}))
            }
            "state" => json(json!({"data":{"repository":{"pullRequest":{"id":"PR_node","headRefOid":"def4567","isMergeQueueEnabled":false,"mergeStateStatus":"CLEAN","baseRef":{"compare":{"behindBy":0}}}}}})),
            "nodeId" => json(json!({"data":{"repository":{"pullRequest":{"id":"PR_node"}}}})),
            "core" => json(core(data)),
            "summary" => {
                if query.starts_with("query PullRequestSummaries") {
                    json(json!({"data":{"s0":summary(data)["data"]["s0"].clone()}}))
                } else {
                    json(summary(data))
                }
            }
            "activity" => json(json!({"data":{"repository":{"pullRequest":{"author":actor("alice"),
                "commits":{"nodes":[{"commit":{"oid":"c1","messageHeadline":"one","committedDate":"2026-01-01T00:00:00Z","authors":{"nodes":[{"name":"Al","email":"a@x","user":{"login":"alice"}}]}}}]},
                "comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":data.comments},
                "reviews":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"R_1","body":"","state":"APPROVED","submittedAt":"2026-01-02T01:00:00Z","url":"https://r/1","author":actor("carol")}]}}}}})),
            "threads" => json(json!({"data":{"viewer":{"login":"octo"},"repository":{"pullRequest":{
                "reviewThreads":{"totalCount":data.threads.len(),"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":data.threads},
                "viewerCanUpdate":data.permission == "WRITE","viewerDidAuthor":false,"author":actor("alice"),"reactionGroups":data.groups("PR_node"),
                "comments":{"nodes":data.comments.iter().map(|comment| json!({"id":comment["id"],"lastEditedAt":comment["lastEditedAt"],"author":comment["author"],"reactionGroups":data.groups(comment["id"].as_str().unwrap_or_default())})).collect::<Vec<_>>()},
                "reviews":{"nodes":[]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[]},"reviewDismissals":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]},
                "commits":{"nodes":[{"commit":{"oid":"c1","messageHeadline":"one","committedDate":"2026-01-01T00:00:00Z","additions":5,"deletions":1,"parents":{"totalCount":1},"authors":{"nodes":[{"name":"Al","avatarUrl":"https://a","user":{"login":"alice"}}]}}}]}}}}})),
            "threadComments" => {
                let after = variables["cursor"].as_str().unwrap_or("");
                let page = data.threads.iter().find(|thread| thread["id"] == variables["threadId"]).map(|thread| thread["comments"]["nodes"].as_array().cloned().unwrap_or_default()).unwrap_or_default();
                let nodes: Vec<Value> = page.into_iter().skip(usize::from(!after.is_empty())).collect();
                json(json!({"data":{"viewer":{"login":"octo"},"repository":{"pullRequest":{"id":"PR_node"}},"node":{"pullRequest":{"id":"PR_node"},"comments":{"pageInfo":{"hasNextPage":false,"endCursor":"end"},"nodes":nodes}}}}))
            }
            "filesViewed" => json(json!({"data":{"repository":{"pullRequest":{"files":{"pageInfo":{"hasNextPage":false,"endCursor":null},
                "nodes":data.viewed.iter().map(|(path, viewed)| json!({"path":path,"viewerViewedState":if *viewed { "VIEWED" } else { "UNVIEWED" }})).collect::<Vec<_>>()}}}}})),
            "merge" => match &data.merge_refusal {
                Some(sentence) => json(json!({"data":null,"errors":[{"message":sentence}]})),
                None => {
                    data.state = "MERGED";
                    json(json!({"data":{}}))
                }
            },
            "draft" => {
                data.draft = true;
                json(json!({"data":{}}))
            }
            "viewed" => {
                let viewed = !query.contains("f0: unmarkFileAsViewed");
                let path = variables["path0"].as_str().unwrap_or("").to_owned();
                for entry in data.viewed.iter_mut().filter(|(existing, _)| *existing == path) {
                    entry.1 = viewed;
                }
                json(json!({"data":{}}))
            }
            "reply" => {
                let body = variables["body"].as_str().unwrap_or("").to_owned();
                if let Some(thread) = data.threads.iter_mut().find(|thread| thread["id"] == variables["threadId"]) {
                    let count = thread["comments"]["nodes"].as_array().map_or(0, Vec::len);
                    thread["comments"]["nodes"].as_array_mut().unwrap().push(json!({"id":format!("RC_{}", count + 1),"author":actor("octo"),"body":body,"createdAt":"2026-01-04T00:00:00Z","url":"https://t/2","reactionGroups":[]}));
                    thread["comments"]["totalCount"] = json!(count + 1);
                }
                json(json!({"data":{}}))
            }
            "resolve" | "unresolve" => {
                if let Some(thread) = data.threads.iter_mut().find(|thread| thread["id"] == variables["threadId"]) {
                    thread["isResolved"] = json!(kind_of(asked) == "resolve");
                }
                json(json!({"data":{}}))
            }
            "comment" => {
                let body = variables["body"].as_str().unwrap_or("").to_owned();
                let id = format!("IC_{}", data.comments.len() + 1);
                data.comments.push(json!({"id":id,"body":body,"createdAt":"2026-01-04T00:00:00Z","lastEditedAt":null,"url":"https://c/2","author":actor("octo")}));
                json(json!({"data":{}}))
            }
            "search" => json(json!({"data":{"search":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[
                {"number":data.number,"title":data.title,"url":format!("https://github.com/acme/web/pull/{}", data.number),"author":actor("alice"),"headRefName":"feature","baseRefName":"main","state":data.state,"isDraft":data.draft,
                 "mergeable":"MERGEABLE","reviewDecision":"APPROVED","latestReviews":{"nodes":[]},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z","mergedAt":null,"repository":{"nameWithOwner":"acme/web"},
                 "reviewRequests":{"nodes":[]},"labels":{"nodes":[{"name":"bug","color":"ff0000"}]},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}}]}}})),
            "stats" => json(json!({"data":{"s0":{"pullRequest":{"additions":5,"deletions":1}}}})),
            "reviewerCandidates" => json(json!({"data":{"repository":{
                "assignableUsers":{"pageInfo":{"hasNextPage":false},"nodes":[{"login":"alice","name":"Alice","avatarUrl":null},{"login":"bob","name":"Bob","avatarUrl":null},{"login":"carol","name":null,"avatarUrl":null}]},
                "pullRequest":{"author":{"login":"alice"},"reviewRequests":{"nodes":data.reviewers}}}}})),
            "labelCandidates" => json(json!({"data":{"repository":{
                "labels":{"pageInfo":{"hasNextPage":false},"nodes":[{"name":"bug","color":"ff0000","description":"Broken"},{"name":"docs","color":null,"description":null}]},
                "pullRequest":{"labels":{"nodes":[{"name":"bug"}]}}}}})),
            "update" | "editComment" | "react" | "unreact" if data.write_refusal.is_some() => json(json!({"data":null,"errors":[{"message":data.write_refusal}]})),
            "update" => {
                if let Some(title) = variables["title"].as_str() {
                    data.title = title.to_owned();
                }
                if let Some(body) = variables["body"].as_str() {
                    data.body = body.to_owned();
                }
                json(json!({"data":{}}))
            }
            "editComment" => {
                if let Some(comment) = data.comments.iter_mut().find(|comment| comment["id"] == variables["commentId"]) {
                    comment["body"] = variables["body"].clone();
                    comment["lastEditedAt"] = json!("2026-01-05T00:00:00Z");
                }
                json(json!({"data":{}}))
            }
            "react" | "unreact" => {
                let (subject, content, reacted) = (variables["subjectId"].as_str().unwrap_or("").to_owned(), variables["content"].as_str().unwrap_or("").to_owned(), kind_of(asked) == "react");
                if !data.reactions.iter().any(|(id, _)| *id == subject) {
                    data.reactions.push((subject.clone(), vec![]));
                }
                let groups = &mut data.reactions.iter_mut().find(|(id, _)| *id == subject).unwrap().1;
                let others = groups.iter().find(|group| group["content"] == content.as_str()).map_or(0, |group| group["reactors"]["totalCount"].as_u64().unwrap_or(0) - u64::from(group["viewerHasReacted"] == true));
                groups.retain(|group| group["content"] != content.as_str());
                let total = others + u64::from(reacted);
                if total > 0 {
                    groups.push(json!({"content":content,"viewerHasReacted":reacted,"reactors":{"totalCount":total,"nodes":(0..others).map(|n| json!({"login":format!("other{n}")})).collect::<Vec<_>>()}}));
                }
                json(json!({"data":{}}))
            }
            "close" | "reopen" | "ready" | "autoMerge" | "disableAutoMerge" | "updateBranch" | "editReviewComment" | "mutation" => json(json!({"data":{}})),
            "subject" => json(json!({"data":{"repository":{"pullRequest":{"id":"PR_node"}},"node":{"id":variables["subjectId"],"pullRequest":{"id":"PR_node"}}}})),
            other => (500, json!({"message":format!("unscripted document {other}")}).to_string()),
        };
    }
    if asked.method == "POST" && asked.path.ends_with("/reviews") {
        return json(json!({"id":1}));
    }
    if asked.path.ends_with(&format!("/pulls/{}", data.number)) && asked.accept.as_deref() == Some("application/vnd.github.diff") {
        return (200, data.patch.clone());
    }
    if asked.path.ends_with("/requested_reviewers") || asked.path.contains("/labels") {
        return json(json!([]));
    }
    if asked.path.contains("/contents/") {
        return (200, (1..=30).map(|n| format!("line {n}\n")).collect());
    }
    if asked.path.ends_with(&format!("/pulls/{}", data.number)) {
        return json(json!({"base":{"sha":"1111111"},"head":{"sha":"2222222"}}));
    }
    if asked.path.contains("/stacks") {
        return match (&data.stack, &data.stack_list) {
            (_, Some(list)) if asked.path.ends_with("/stacks") => (200, json!([list]).to_string()),
            (Some(stack), _) if asked.path.ends_with("/stacks") => (200, json!([stack]).to_string()),
            (Some(stack), _) => (200, stack.to_string()),
            _ => (404, "{}".into()),
        };
    }
    if asked.method == "PUT" && asked.path.contains("/merge-async") {
        return (200, json!({"status":data.stack_merge,"details":{"uuid":"job-1"}}).to_string());
    }
    (500, json!({"message":format!("unscripted {} {}", asked.method, asked.path)}).to_string())
}
