//! A GitLab merge request through the mounted application and the real native server, against a fake `glab`
//! (the answers `pull_request_gitlab_flow.rs` scripts in the server crate, which this crate cannot import).
//! The panel follows what the host says it can do rather than assuming GitHub: its own name on the link
//! out, one "Pipeline" check, a review that is a comment or an approval, an update that is a rebase, no
//! labels to edit and no revert, and viewed marks that are kept by this environment and say so; the list
//! says nothing the host did not summarise. No network, no real `glab`.
use super::diff_tests::{Fixture, error, git, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::source_control_tests::world;
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, control, control_attribute};

const WRITER: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SourceControlWrite];

fn user(id: u64, username: &str) -> Value {
    json!({"id":id,"username":username,"name":username.to_uppercase(),"avatar_url":format!("https://gitlab.example/{username}.png")})
}
fn merge_request(number: u64) -> Value {
    json!({
        "iid":number,"title":format!("Fix {number}"),"web_url":format!("https://gitlab.com/acme/web/-/merge_requests/{number}"),"description":"Body text",
        "author":user(1,"alice"),"source_branch":"feature","target_branch":"main","state":"opened","draft":false,"merge_status":"can_be_merged","has_conflicts":false,
        "created_at":"2026-01-01T00:00:00.000Z","updated_at":"2026-01-02T00:00:00.000Z","merged_at":null,"closed_at":null,
        "reviewers":[user(2,"bob")],"labels":["bug"],"changes_count":"3",
        "head_pipeline":{"status":"success","web_url":"https://gitlab.com/acme/web/-/pipelines/9","source":"merge_request_event"},
        "user":{"can_merge":true},"merge_when_pipeline_succeeds":false,"squash_on_merge":false,"diverged_commits_count":2
    })
}
fn glab_row(number: u64) -> String {
    json!({"iid":number,"title":format!("Fix {number}"),"web_url":format!("https://gitlab.com/acme/web/-/merge_requests/{number}"),"source_branch":"feature","target_branch":"main","state":"opened","draft":false,"updated_at":"2026-01-02T00:00:00.000Z"}).to_string()
}
/// What `glab` answers, by what it is asked: the first route whose text the arguments contain. A route may need
/// its standard input to contain something too (the GraphQL ones).
fn routes() -> Vec<(&'static str, Option<&'static str>, String)> {
    let award = |name: &str, username: &str| json!({"name":name,"user":{"username":username}});
    vec![
        ("--version", None, "glab 1.50.0\n".into()),
        ("auth status", None, "git.example.com\n  \u{2713} Logged in to git.example.com as carol (token)\n".into()),
        ("api user", None, "{\"username\":\"carol\"}".into()),
        ("merge_requests/7?include_diverged", None, merge_request(7).to_string()),
        ("?license=false", None, "{\"merge_method\":\"merge\",\"squash_option\":\"default_off\"}".into()),
        ("web/merge_requests?state", None, json!([merge_request(7)]).to_string()),
        ("/merge_requests/7/notes?", None, json!([{"id":11,"body":"Looks fine","author":user(2,"bob"),"created_at":"2026-01-02T00:00:00.000Z","system":false,"type":null}]).to_string()),
        ("/merge_requests/7/commits?", None, json!([{"id":"abc1234","title":"Add it","committed_date":"2026-01-02T00:00:00.000Z","parent_ids":["0000000"],"author_name":"Alice","stats":{"additions":3,"deletions":1}}]).to_string()),
        ("/merge_requests/7/discussions?", None, "[]".into()),
        ("api graphql", Some("awardEmoji"), json!({"data":{"currentUser":{"username":"carol"},"project":{"mergeRequest":{"awardEmoji":{"nodes":[award("tada","bob")]},"notes":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}).to_string()),
        ("api graphql", Some("blobs"), json!({"data":{"project":{"repository":{"blobs":{"nodes":[{"path":"a.ts","oid":"blob-1"}]}}}}}).to_string()),
        ("/merge_requests/7/diffs?", None, json!([{"old_path":"a.ts","new_path":"a.ts","diff":"@@ -1 +1 @@\n-a\n+b\n","new_file":false,"renamed_file":false,"deleted_file":false}]).to_string()),
        ("web/users?", None, json!([user(1,"alice"),user(2,"bob"),user(3,"carol")]).to_string()),
        ("--method", None, "{}".into()),
        ("mr view", None, glab_row(7)),
        ("mr list", None, format!("[{}]", glab_row(7))),
        ("mr ", None, String::new()),
        ("merge_requests/7", None, json!({"diff_refs":{"base_sha":"b0","head_sha":"h1","start_sha":"s2"}}).to_string()),
    ]
}
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}
fn install_glab() {
    let mut body = String::from("joined=\"$*\"\nin=$(cat)\n");
    for (needle, stdin, answer) in routes() {
        let reply = format!("printf '%s' {}; exit 0", quoted(&answer));
        body.push_str(&match stdin {
            Some(stdin) => format!("case \"$joined\" in *{}*) case \"$in\" in *{}*) {reply};; esac;; esac\n", quoted(needle), quoted(stdin)),
            None => format!("case \"$joined\" in *{}*) {reply};; esac\n", quoted(needle)),
        });
    }
    body.push_str("echo \"no route: $joined\" >&2\nexit 97");
    world().install_cli("glab", &body);
}

/// A thread on the `feature` branch of a project whose remote is GitLab.
async fn mount() -> (Fixture, VirtualDom, Store<UiModel>, tokio::sync::MutexGuard<'static, ()>) {
    let alone = world().exclusive().lock().await;
    world().reset_everything_signed_in();
    install_glab();
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["remote", "add", "origin", "https://gitlab.com/acme/web.git"]);
    git(&fixture.workspace, &["checkout", "-q", "-b", "feature"]);
    let (mut dom, props) = fixture.client(Gate::default(), &WRITER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    (fixture, dom, state, alone)
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_gitlab_merge_request_is_shown_as_the_host_says_it_can_be_used() {
    let (fixture, mut dom, state, _alone) = mount().await;
    // The badge speaks the host's words, and the panel opens from it.
    wait_for_rendered_text(&mut dom, "MR #7 \u{b7} Open").await;
    click_control(&mut dom, "merge request #7");
    wait_for_rendered_text(&mut dom, "Body text").await;
    let text = rendered_text(&dom);
    // The host names itself on the link out, and the control keeps the original's name.
    assert!(control(&dom, "Open pull request #7 on host").is_some() && text.contains("Open on GitLab"), "{text}");
    // One check, "Pipeline"; the reviewer GitLab lists; no review decision it does not give.
    assert!(text.contains("Pipeline"), "{text}");
    assert!(text.contains("alice") && text.contains("bob"), "{text}");
    // Merge is offered with the methods this project allows, and a rebase is not one of them.
    assert!(control(&dom, "Merge method").is_none_or(|_| !text.contains("Rebase")), "{text}");
    // Labels and reverts are not on this host: the label is read, there is nothing to edit it with, and no revert.
    assert!(text.contains("bug") && control(&dom, "Edit labels").is_none() && control(&dom, "Labels").is_none());
    assert!(control(&dom, "Revert pull request").is_none() && !text.contains("Revert"));
    // The branch is behind, and the host can only rebase it.
    assert!(control(&dom, "Update branch").is_some() && text.contains("2 commits behind"), "{text}");
    assert_eq!(error(state), None);

    // Review: a comment or an approval, nothing else.
    click_control(&mut dom, "Pull request conversation");
    wait_for_rendered_text(&mut dom, "Looks fine").await;
    until(&mut dom, "the review form", |dom| control(dom, "Review verdict").is_some()).await;
    let verdicts = rendered_text(&dom);
    assert!(verdicts.contains("Comment") && verdicts.contains("Approve") && !verdicts.contains("Request changes"), "{verdicts}");

    // Files: the marks are this environment's, and the page says so rather than implying the host has them.
    click_control(&mut dom, "Files changed");
    wait_for_rendered_text(&mut dom, "0 / 1 viewed in T3 Code").await;
    assert!(control(&dom, "These ticks are kept here, not on the host").is_some());
    click_control(&mut dom, "Viewed a.ts");
    assert!(disabled(&dom, "Viewed a.ts"), "a mark is written once and the box waits for the server");
    wait_for_rendered_text(&mut dom, "1 / 1 viewed in T3 Code").await;
    assert_eq!(control_attribute(&dom, "Viewed a.ts", "checked").and_then(|(_, value)| value).as_deref(), Some("true"));
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_list_names_the_host_and_leaves_out_what_it_did_not_summarise() {
    let (fixture, mut dom, state, _alone) = mount().await;
    state.view().set(View::PullRequests);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    wait_for_rendered_text(&mut dom, "Fix 7").await;
    let text = rendered_text(&dom);
    assert!(text.contains("GitLab acme/web"), "{text}");
    // GitLab counts no lines for a row, summarises no checks and no reviews: not zero, nothing.
    assert!(!text.contains("+0") && !text.contains("Checks") && !text.contains("Approved") && !text.contains("Review required"), "{text}");
    fixture.finish(dom).await;
}
