//! Azure DevOps and Bitbucket pull requests through the mounted application and the real native server, against a fake
//! `az` (the answers `pull_request_azure_flow.rs` scripts in the server crate, which this crate cannot import) and a
//! Bitbucket that has no credentials. The panel follows what the host says it can do: Azure has no comment composer,
//! reaction, label, verdict, reply or resolution, no list of people to ask for a review, and reports no checks and no
//! counts; an unsigned-in Bitbucket points at Settings, not at a command line. No network, no real `az`.
//! Forgejo, Gitea and the Bitbucket panel are covered over host-shaped answers in `pull_request_panel`'s tests.
use super::diff_tests::{Fixture, error, git, open_thread, rendered_text, seed, wait_for_rendered_text};
use super::source_control_tests::world;
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, control};

const WRITER: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SourceControlWrite];

fn identity(name: &str, unique: &str) -> Value {
    json!({"displayName":name,"uniqueName":unique,"imageUrl":format!("https://img.example/{name}.png")})
}
fn pull_request(number: u64) -> Value {
    json!({
        "pullRequestId":number,"title":format!("Fix {number}"),"description":"Body text","status":"active","isDraft":false,"mergeStatus":"succeeded",
        "createdBy":identity("Alice","alice@acme.io"),"reviewers":[identity("Bob","bob@acme.io")],
        "sourceRefName":"refs/heads/feature","targetRefName":"refs/heads/main","creationDate":"2026-01-02T00:00:00Z","closedDate":null,
        "url":format!("https://dev.azure.com/acme/_apis/git/repositories/web/pullRequests/{number}"),
        "repository":{"name":"web","webUrl":"https://dev.azure.com/acme/shop/_git/web","project":{"name":"shop"}},
        "_links":{"web":{"href":format!("https://dev.azure.com/acme/shop/_git/web/pullrequest/{number}")}}
    })
}
/// What `az` answers, by what it is asked: the first route whose text the arguments contain.
fn routes() -> Vec<(&'static str, String)> {
    let comment = |id: u64, content: &str, at: &str| json!({"id":id,"content":content,"author":identity("Bob","bob@acme.io"),"publishedDate":at});
    vec![
        ("--version", "azure-cli 2.60.0\n".into()),
        ("account show --query user.name", "carol@acme.io\n".into()),
        ("account show --query user", "{\"name\":\"carol@acme.io\",\"type\":\"user\"}".into()),
        ("--id 7 --only", pull_request(7).to_string()),
        ("repos pr list", json!([pull_request(7)]).to_string()),
        ("--resource pullRequestThreads", json!({"value":[
            {"id":1,"comments":[comment(1,"Looks fine","2026-01-02T00:00:00Z")]},
            {"id":2,"threadContext":{"filePath":"/src/a.ts"},"comments":[comment(1,"Rename this","2026-01-02T00:01:00Z")]}
        ]}).to_string()),
        ("repos pr update", "{}".into()),
        ("repos pr reviewer", "[]".into()),
    ]
}
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}
fn install_az() {
    let mut body = String::from("joined=\"$*\"\n");
    for (needle, answer) in routes() {
        body.push_str(&format!("case \"$joined\" in *{}*) printf '%s' {}; exit 0;; esac\n", quoted(needle), quoted(&answer)));
    }
    body.push_str("echo \"no route: $joined\" >&2\nexit 97");
    world().install_cli("az", &body);
}

async fn mount(remote: &str) -> (Fixture, VirtualDom, Store<UiModel>, tokio::sync::MutexGuard<'static, ()>) {
    let alone = world().exclusive().lock().await;
    world().reset_everything_signed_in();
    install_az();
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["remote", "add", "origin", remote]);
    git(&fixture.workspace, &["checkout", "-q", "-b", "feature"]);
    let (mut dom, props) = fixture.client(Gate::default(), &WRITER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    (fixture, dom, state, alone)
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn an_azure_devops_pull_request_shows_only_what_the_host_can_do() {
    let (fixture, mut dom, state, _alone) = mount("https://dev.azure.com/acme/shop/_git/web").await;
    // The status of this fake branch does not name the pull request, so the panel is opened on it directly.
    let reference = t3_client::pull_request::Reference { project_id: "project".into(), host: None, repository: "web".into(), number: 7 };
    dom.in_scope(ScopeId::APP, || crate::pull_request_panel::open(state, crate::pull_request_panel::Target { thread_id: "thread-a".into(), reference }));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    wait_for_rendered_text(&mut dom, "Body text").await;
    let text = rendered_text(&dom);
    assert!(control(&dom, "Open pull request #7 on host").is_some() && text.contains("Open on Azure DevOps"), "{text}");
    // No checks (null, not an empty list), no counts the host did not give, and no list of people to ask.
    assert!(!text.contains("Checks") && !text.contains("+0") && !text.contains("0 files"), "{text}");
    assert!(text.contains("Reviewers: ") && text.contains("bob@acme.io") && control(&dom, "Request a review").is_none(), "{text}");
    assert!(!text.contains("Labels:") && control(&dom, "Add a reaction").is_none(), "{text}");
    assert_eq!(error(state), None);

    // The conversation: what the host's threads said, and no composer, verdict, reply or resolution.
    click_control(&mut dom, "Pull request conversation");
    wait_for_rendered_text(&mut dom, "Looks fine").await;
    let conversation = rendered_text(&dom);
    assert!(control(&dom, "Add a comment").is_none() && control(&dom, "Comment").is_none(), "{conversation}");
    assert!(control(&dom, "Review verdict").is_none() && control(&dom, "Submit review").is_none(), "{conversation}");
    click_control(&mut dom, "Files changed");
    assert!(control(&dom, "Reply to thread on /src/a.ts").is_none() && control(&dom, "Resolve thread on /src/a.ts").is_none());
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

/// Bitbucket has no command line: with no credentials the list says to add them in Settings, and never asks for
/// `gh auth login` or `glab auth login`.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_bitbucket_without_credentials_points_at_settings_and_not_a_command() {
    let (fixture, mut dom, state, _alone) = mount("https://bitbucket.org/acme/web.git").await;
    // The host refuses the anonymous server, as Bitbucket does a request without a credential.
    world().route_bitbucket(401, json!({"error":{"message":"Token is invalid, expired, or not supported for this endpoint."}}));
    state.view().set(View::PullRequests);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    // The server keeps a list by state for a while, across the servers of this process; a state no other test reads is a fresh one.
    crate::runtime::transport_tests::change_control(&mut dom, "Pull request state", "merged");
    wait_for_rendered_text(&mut dom, "Source Control").await;
    let text = rendered_text(&dom);
    assert!(text.contains("Settings") && !text.contains("auth login") && !text.contains("`gh`") && !text.contains("glab"), "{text}");
    fixture.finish(dom).await;
}
