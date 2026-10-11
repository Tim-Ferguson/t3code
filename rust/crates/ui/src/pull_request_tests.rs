//! The pull request panel against the real native server: a project whose host cannot be read as a
//! source of pull requests answers "not available for this host", which the panel shows as a state,
//! not as a failure. The panel's own states (actions, permissions, merge box) are covered beside
//! the component. The rest mounts the whole application against the real server with GitHub
//! faked at its API (`fake_github`): the badge from git status, the overview, checks and
//! conversation read from the host, the refresh notice, the comment, and the actions with their
//! confirmations, double clicks, refusals, missing permission and a lost connection.
use super::diff_tests::{Fixture, Harness, assert_repaints_explained_by_events, error, git, open_thread, rendered_text, repaints, seed, until, wait_for_rendered_text};
use super::keys_tests::{PALETTE, chord, key_on, open_palette, press};
use t3_client::keybindings::Dom;
use super::fake_github::{Gate as Hold, Github, kind_of};
use super::source_control_tests::world;
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{change_control, click_control, click_when_enabled, control, control_attribute, force_click_control, input_control};
use t3_client::pull_request::Reference;

const WRITER: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SourceControlWrite];
const READER: [AuthEnvironmentScope; 2] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];

/// Runs a test body on a mounted application; the fixture's `Server` stops its services even
/// when the body panics.
async fn with_app(number: u64, scopes: &[AuthEnvironmentScope], body: impl AsyncFnOnce(&mut Mounted)) {
    run_app(number, scopes, false, body).await;
}
async fn with_probed_app(number: u64, scopes: &[AuthEnvironmentScope], body: impl AsyncFnOnce(&mut Mounted)) {
    run_app(number, scopes, true, body).await;
}
#[derive(Clone)]
struct ProbeHarness {
    inner: Harness,
    probe: crate::RenderProbe,
}
fn probe_harness(props: ProbeHarness) -> Element {
    use_context_provider(|| props.probe.clone());
    super::diff_tests::harness(props.inner.clone())
}
async fn run_app(number: u64, scopes: &[AuthEnvironmentScope], probed: bool, body: impl AsyncFnOnce(&mut Mounted)) {
    let mut app = Mounted::start(number, scopes, probed).await;
    body(&mut app).await;
    app.finish().await;
}

/// A mounted application on a thread whose branch has a pull request at the fake host. Tests of
/// it run one at a time: the host the server asks is shared by the whole test binary.
struct Mounted {
    probe: Option<crate::RenderProbe>,
    fixture: Fixture,
    dom: VirtualDom,
    props: Harness,
    state: Store<UiModel>,
    github: Github,
    number: u64,
    _alone: tokio::sync::MutexGuard<'static, ()>,
}
impl Mounted {
    async fn start(number: u64, scopes: &[AuthEnvironmentScope], probed: bool) -> Self {
        let world = world();
        let alone = world.exclusive().lock().await;
        world.reset_everything_signed_in();
        let github = Github::install(world, number);
        let fixture = Fixture::start(true).await;
        git(&fixture.workspace, &["remote", "add", "origin", "https://github.com/acme/web.git"]);
        git(&fixture.workspace, &["checkout", "-q", "-b", "feature"]);
        let (mut dom, props) = fixture.client(Gate::default(), scopes);
        let probe = probed.then(crate::RenderProbe::default);
        if let Some(probe) = &probe {
            // The same application, with the render counters in its context.
            dom = VirtualDom::new_with_props(probe_harness, ProbeHarness { inner: props.clone(), probe: probe.clone() });
            dom.rebuild_in_place();
        }
        let state = seed(&mut dom, &props, &fixture.workspace).await;
        open_thread(&mut dom, &props, state, "thread-a").await;
        Self { probe, fixture, dom, props, state, github, number, _alone: alone }
    }
    /// The badge from git status, then the panel it opens.
    async fn open_panel(&mut self) {
        wait_for_rendered_text(&mut self.dom, &format!("PR #{} \u{b7} Open", self.number)).await;
        click_control(&mut self.dom, &format!("pull request #{}", self.number));
        wait_for_rendered_text(&mut self.dom, "Please").await;
        // Where the host has stacks, what can be done with the pull request is known once its stack lookup has answered.
        until(&mut self.dom, "the stack lookup answered", |dom| control(dom, crate::pull_request_summary::CHECKING_STACK).is_none()).await;
    }
    fn asked(&self, kind: &str) -> usize {
        world().asked().iter().filter(|asked| kind_of(asked) == kind).count()
    }
    async fn finish(self) {
        self.fixture.finish(self.dom).await;
    }
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_host_without_pull_request_support_is_shown_as_not_available_and_not_as_a_failure() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SourceControlWrite]);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    dom.in_scope(ScopeId::APP, || {
        crate::pull_request_panel::open(state, crate::pull_request_panel::Target { thread_id: "thread-a".into(), reference: Reference { project_id: "project".into(), host: Some("example.invalid".into()), repository: "acme/web".into(), number: 7 } })
    });
    wait_for_rendered_text(&mut dom, "Pull requests are not available for this host.").await;
    assert!(control(&dom, "Retry loading pull request").is_none(), "nothing failed, so there is nothing to retry");
    assert_eq!(error(state), None, "and the app banner stays clear");
    assert!(!rendered_text(&dom).contains("could not complete"));
    // The panel closes from its own button, and the diff panel and this one never share the margin.
    click_control(&mut dom, "Close pull request panel");
    assert!(control(&dom, "Pull request panel").is_none());
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_badge_the_overview_the_checks_and_the_conversation_come_from_the_host() {
    with_app(7, &WRITER, async |app| {
    app.open_panel().await;
    let text = rendered_text(&app.dom);
    assert!(text.contains("Fix it") && text.contains("feature \u{2192} main") && text.contains("alice"), "{text}");
    // The checks, in the original's words: the wire's one in-flight status is "Running".
    assert!(text.contains("Running") && text.contains("lint") && text.contains("Passed") && text.contains("build") && !text.contains("Pending"), "{text}");
    assert!(text.contains("Please") && text.contains("+10"), "the description and the size: {text}");
    // The tabs are the original's, in its order.
    for label in ["Summary", "Timeline", "Code"] {
        assert!(control(&app.dom, label).is_some(), "the {label} tab");
    }
    assert!(control(&app.dom, "Overview").is_none() && control(&app.dom, "Files changed").is_none() && control(&app.dom, "Pull request conversation").is_none());
    // The Summary lists the comments, newest first unless asked otherwise; the activity behind them is read once.
    wait_for_rendered_text(&mut app.dom, "first remark").await;
    assert_eq!(app.asked("activity"), 1);
    let text = rendered_text(&app.dom);
    assert!(text.contains("bob") && text.contains("Comments (3)") && text.contains("Newest first"), "{text}");
    assert!(text.contains("commented") && !text.contains("issue-comment") && text.contains("ago"), "an entry says what its author did, and when, not a wire name: {text}");
    // The Timeline is what happened in order: the commit with its short id, the conversation, and the opening.
    click_control(&mut app.dom, "Timeline");
    let text = rendered_text(&app.dom);
    let timeline = &text[text.find("Newest first").unwrap()..];
    assert!(timeline.contains("c1") && timeline.contains("one") && timeline.contains("2 comments") && timeline.contains("approved") && timeline.contains("Pull request opened"), "the verdict is a row of its own, the two remarks one conversation: {timeline}");
    assert_eq!(app.asked("activity"), 1, "changing tabs does not read the activity again");
    assert!(timeline.find("approved").unwrap() < timeline.find("2 comments").unwrap() && timeline.find("2 comments").unwrap() < timeline.find("Pull request opened").unwrap(), "newest first: {timeline}");
    click_control(&mut app.dom, "Show oldest activity first");
    let text = rendered_text(&app.dom);
    let timeline = &text[text.find("Oldest first").unwrap()..];
    assert!(timeline.find("Pull request opened").unwrap() < timeline.find("approved").unwrap(), "the order flips: {timeline}");
    assert_eq!(error(app.state), None);
    }).await;
}

/// Writes the reference of the mounted pull request as the server wants it.
fn reference(app: &Mounted) -> Value {
    json!({"projectId":"project","host":"github.com","repository":"acme/web","number":app.number})
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_refresh_notice_after_a_change_elsewhere_makes_the_panel_read_again() {
    with_app(8, &WRITER, async |app| {
    app.open_panel().await;
    assert!(!rendered_text(&app.dom).contains("Renamed by a colleague"));
    let before = app.asked("core");
    app.github.set(|data| data.title = "Renamed by a colleague".into());
    // Somebody else's write reaches the server, which tells every reader; nothing here polls.
    let payload = json!({"reference":reference(app)});
    app.dom.in_scope(ScopeId::APP, || request(&app.props.transport, app.state, "pullRequests.invalidate", payload, RequestKind::Unary)).unwrap();
    wait_for_rendered_text(&mut app.dom, "Renamed by a colleague").await;
    assert!(app.asked("core") > before, "the detail was read again");
    assert_eq!(error(app.state), None);
    }).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_comment_is_posted_once_cleared_and_read_back_from_the_host() {
    with_app(9, &WRITER, async |app| {
    app.open_panel().await;
    wait_for_rendered_text(&mut app.dom, "first remark").await;
    // The composer floats over every tab; it opens on the comment, the mode with nothing else waiting.
    click_control(&mut app.dom, "Comment on pull request");
    assert!(disabled(&app.dom, "Comment"), "nothing to post yet");
    input_control(&mut app.dom, "Comment on this pull request", "  Ship it please  ");
    assert!(!disabled(&app.dom, "Comment"));
    click_control(&mut app.dom, "Comment");
    wait_for_rendered_text(&mut app.dom, "Ship it please").await;
    assert_eq!(app.asked("comment"), 1);
    let sent = world().asked().into_iter().find(|asked| kind_of(asked) == "comment").unwrap();
    assert_eq!(sent.body.unwrap()["variables"]["body"], "Ship it please", "the text is trimmed");
    assert_eq!(control_attribute(&app.dom, "Comment on this pull request", "value").unwrap().1.as_deref(), Some(""), "the form is cleared by the server's answer");
    assert_eq!(error(app.state), None);
    }).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_pull_request_hands_a_question_an_explanation_and_its_findings_to_the_threads_composer() {
    with_app(11, &WRITER, async |app| {
    app.open_panel().await;
    wait_for_rendered_text(&mut app.dom, "first remark").await;
    let chips = |app: &Mounted| -> Vec<String> {
        let target = crate::composer_chips::open_target(app.state).expect("a thread is open");
        crate::composer_chips::contexts_of(app.state, &target).iter().filter_map(|record| record["text"].as_str().map(str::to_owned)).collect()
    };
    // A question leaves the composer empty: the pull request travels as a chip.
    click_control(&mut app.dom, "More pull request actions");
    click_control(&mut app.dom, "Ask a question");
    assert_eq!(app.state.peek().draft, "");
    let held = chips(app);
    assert_eq!(held.len(), 1, "{held:?}");
    assert!(held[0].contains("The pull request is #11") && held[0].contains("Answer the question asked in this message."), "{held:?}");
    // Explaining takes the question's chip back and asks for itself; the reader's own words stay.
    app.state.draft().set("My own note".into());
    click_control(&mut app.dom, "More pull request actions");
    click_control(&mut app.dom, "Explain this PR");
    assert_eq!(app.state.peek().draft, "My own note\n\nExplain this pull request.");
    let held = chips(app);
    assert_eq!(held.len(), 1, "one hand-off's chips at a time: {held:?}");
    assert!(held[0].contains("Explain only. Do not change any code."), "{held:?}");
    // The findings: the unresolved threads as chips, the rest as the prompt.
    click_control(&mut app.dom, "More pull request actions");
    click_control(&mut app.dom, "Fix findings in this thread");
    assert!(app.state.peek().draft.starts_with("My own note\n\nFix the actionable findings on PR #11"), "{}", app.state.peek().draft);
    assert!(!app.state.peek().draft.contains("Explain this pull request."), "the earlier hand-off's sentence is replaced");
    assert!(!chips(app).iter().any(|text| text.contains("Explain only")));
    assert_eq!(error(app.state), None);
    }).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn an_action_is_disabled_without_the_connections_grant_and_absent_without_the_hosts() {
    with_app(10, &READER, async |app| {
    app.github.passing();
    app.open_panel().await;
    assert!(disabled(&app.dom, "Merge") && disabled(&app.dom, "Close pull request"), "this connection cannot change source control");
    assert_eq!(control_attribute(&app.dom, "Merge", "title").unwrap().1.as_deref(), Some("This connection cannot change source control."));
    wait_for_rendered_text(&mut app.dom, "first remark").await;
    assert!(disabled(&app.dom, "Comment") && disabled(&app.dom, "Comment on this pull request"));
    assert_eq!(app.asked("merge") + app.asked("comment") + app.asked("close"), 0);
    }).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn without_write_access_at_the_host_the_action_is_not_offered() {
    with_app(11, &WRITER, async |app| {
    app.github.passing();
    app.github.set(|data| data.permission = "READ");
    app.open_panel().await;
    let text = rendered_text(&app.dom);
    assert!(control(&app.dom, "Merge").is_none(), "merge: {text}");
    assert!(control(&app.dom, "Close pull request").is_none(), "close: {text}");
    }).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_merge_asks_first_is_sent_once_and_its_control_waits_for_the_servers_answer() {
    with_app(12, &WRITER, async |app| {
    app.github.passing();
    app.open_panel().await;
    let gate = app.github.hold_writes();
    click_control(&mut app.dom, "Merge");
    wait_for_rendered_text(&mut app.dom, "Merge pull request?").await;
    assert!(rendered_text(&app.dom).contains("This merges #12 using merge."));
    assert_eq!(app.asked("merge"), 0, "asking sends nothing");
    // Escape declines.
    crate::runtime::keys_tests::key_on(&mut app.dom, "Cancel", Key::Escape);
    assert!(!rendered_text(&app.dom).contains("Merge pull request?") && app.asked("merge") == 0);
    click_control(&mut app.dom, "Merge");
    click_control(&mut app.dom, "Confirm Merge");
    wait_for_rendered_text(&mut app.dom, "Working on merge").await;
    assert!(disabled(&app.dom, "Merge"), "disabled while in flight");
    // A second try while the first is in flight asks again but cannot send.
    force_click_control(&mut app.dom, "Merge");
    assert!(disabled(&app.dom, "Confirm Merge"));
    force_click_control(&mut app.dom, "Confirm Merge");
    gate.release();
    wait_for_rendered_text(&mut app.dom, "This pull request was merged.").await;
    assert_eq!(app.asked("merge"), 1, "the host was asked to merge exactly once");
    assert!(!rendered_text(&app.dom).contains("Working on merge"));
    }).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_double_click_on_an_action_that_runs_at_once_sends_it_once() {
    with_app(13, &WRITER, async |app| {
    app.open_panel().await;
    let gate = app.github.hold_writes();
    click_control(&mut app.dom, "Convert to draft");
    wait_for_rendered_text(&mut app.dom, "Working on draft").await;
    force_click_control(&mut app.dom, "Convert to draft");
    gate.release();
    wait_for_rendered_text(&mut app.dom, "This pull request is a draft.").await;
    assert_eq!(app.asked("draft"), 1);
    assert_eq!(error(app.state), None);
    }).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_refused_merge_shows_the_hosts_sentence_and_gives_the_control_back() {
    with_app(14, &WRITER, async |app| {
    app.github.passing();
    app.github.set(|data| data.merge_refusal = Some("Required status check build is expected.".into()));
    app.open_panel().await;
    click_control(&mut app.dom, "Merge");
    click_control(&mut app.dom, "Confirm Merge");
    wait_for_rendered_text(&mut app.dom, "Could not merge this pull request").await;
    let text = rendered_text(&app.dom);
    assert!(text.contains("Required status check build is expected."), "{text}");
    assert!(!disabled(&app.dom, "Merge"), "the answer gives the control back");
    assert_eq!(app.asked("merge"), 1);
    }).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_lost_connection_marks_the_pull_request_old_and_turns_every_write_off() {
    with_app(15, &WRITER, async |app| {
    app.github.passing();
    app.open_panel().await;
    assert!(!disabled(&app.dom, "Merge"));
    app.dom.in_scope(ScopeId::APP, || interrupt_connection(&app.props.transport, app.state, "the network went away"));
    wait_for_rendered_text(&mut app.dom, "Offline. This is the last pull request read.").await;
    assert!(disabled(&app.dom, "Merge") && disabled(&app.dom, "Close pull request"));
    assert!(rendered_text(&app.dom).contains("All checks passed"), "what was read stays, marked old");
    }).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn files_changed_come_from_the_hosts_patch_and_viewed_marks_are_the_hosts() {
    with_app(16, &WRITER, async |app| {
    app.open_panel().await;
    click_control(&mut app.dom, "Code");
    wait_for_rendered_text(&mut app.dom, "+b").await;
    let text = rendered_text(&app.dom);
    assert!(text.contains("a.ts") && text.contains("b.ts") && text.contains("+d"), "{text}");
    assert_eq!(app.asked("GET repos/acme/web/pulls/16"), 1, "the host is asked for the patch once");
    // Viewing a file is written to the host and read back; the box waits for the answer.
    assert_eq!(control_attribute(&app.dom, "Viewed a.ts", "checked").unwrap().1.as_deref(), Some("false"));
    click_control(&mut app.dom, "Viewed a.ts");
    until(&mut app.dom, "the mark", |dom| control_attribute(dom, "Viewed a.ts", "checked").and_then(|(_, value)| value).as_deref() == Some("true")).await;
    assert_eq!(app.asked("viewed"), 1);
    // A viewed file starts collapsed; opening it draws its rows again.
    assert!(control(&app.dom, "Expand a.ts").is_some());
    click_control(&mut app.dom, "Viewed a.ts");
    until(&mut app.dom, "the mark gone", |dom| control_attribute(dom, "Viewed a.ts", "checked").and_then(|(_, value)| value).as_deref() == Some("false")).await;
    assert_eq!(app.asked("viewed"), 2);
    assert_eq!(error(app.state), None);
    }).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_pull_request_refresh_repaints_only_the_panel() {
    with_probed_app(17, &WRITER, async |app| {
        app.open_panel().await;
        let probe = app.probe.clone().unwrap();
        // One round trip first, so the tail of what opened the panel has been delivered.
        let payload = json!({"reference":reference(app)});
        app.github.set(|data| data.title = "Settled".into());
        app.dom.in_scope(ScopeId::APP, || request(&app.props.transport, app.state, "pullRequests.invalidate", payload.clone(), RequestKind::Unary)).unwrap();
        wait_for_rendered_text(&mut app.dom, "Settled").await;
        let before = repaints(app.state, &probe);
        app.github.set(|data| data.title = "Refreshed".into());
        app.dom.in_scope(ScopeId::APP, || request(&app.props.transport, app.state, "pullRequests.invalidate", payload, RequestKind::Unary)).unwrap();
        wait_for_rendered_text(&mut app.dom, "Refreshed").await;
        let after = repaints(app.state, &probe);
        // The application (and so the sidebar), the conversation and the composer are not built again.
        assert_eq!(after.0, before.0, "a pull request refresh: app, conversation and composer repaints");
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_copy_number_shortcut_belongs_to_the_open_panel_and_the_palette_opens_it() {
    with_app(18, &WRITER, async |app| {
        let copy = || chord("k", true, false);
        assert_eq!(press(&mut app.dom, copy(), Dom::default()).await, None, "no panel, nothing to copy");
        // The palette offers the branch's pull request, and choosing it opens the panel.
        open_palette(&mut app.dom).await;
        wait_for_rendered_text(&mut app.dom, "PR #18").await;
        input_control(&mut app.dom, PALETTE, ">pull request");
        until(&mut app.dom, "the entry", |dom| control(dom, "Open pull request #18").is_some()).await;
        click_control(&mut app.dom, "Open pull request #18");
        wait_for_rendered_text(&mut app.dom, "Please").await;
        assert_eq!(press(&mut app.dom, copy(), Dom::default()).await.as_deref(), Some("pullRequest.copyNumber"));
        wait_for_rendered_text(&mut app.dom, "PR number copied").await;
        click_control(&mut app.dom, "Close pull request panel");
        assert_eq!(press(&mut app.dom, copy(), Dom::default()).await, None, "a closed panel hands the key back");
        assert_eq!(error(app.state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn review_threads_are_replied_to_and_resolved_and_a_review_is_submitted_once() {
    with_app(19, &WRITER, async |app| {
        app.github.passing();
        app.open_panel().await;
        click_control(&mut app.dom, "Code");
        wait_for_rendered_text(&mut app.dom, "nit about a").await;
        // A reply is sent once and shows when the host has it.
        assert!(disabled(&app.dom, "Reply in the thread on a.ts"), "nothing to say yet");
        input_control(&mut app.dom, "Reply to thread on a.ts", "  Thanks, fixed  ");
        click_control(&mut app.dom, "Reply in the thread on a.ts");
        wait_for_rendered_text(&mut app.dom, "Thanks, fixed").await;
        assert_eq!(app.asked("reply"), 1);
        // Resolving and unresolving are each a write of their own.
        click_control(&mut app.dom, "Resolve thread on a.ts");
        until(&mut app.dom, "the thread resolved", |dom| control(dom, "Unresolve thread on a.ts").is_some()).await;
        assert!(rendered_text(&app.dom).contains("Resolved"));
        click_control(&mut app.dom, "Unresolve thread on a.ts");
        until(&mut app.dom, "the thread open again", |dom| control(dom, "Resolve thread on a.ts").is_some()).await;
        assert_eq!((app.asked("resolve"), app.asked("unresolve")), (1, 1));
        // A review: an approval needs no words, anything else does; it is sent once. It is written in the composer.
        click_control(&mut app.dom, "Comment on pull request");
        click_control(&mut app.dom, "Review mode");
        assert!(disabled(&app.dom, "Submit review"), "a comment needs a summary");
        input_control(&mut app.dom, "Review summary", "Looks fine");
        change_control(&mut app.dom, "Review verdict", "approve");
        click_control(&mut app.dom, "Submit review");
        wait_for_rendered_text(&mut app.dom, "Pull request approved").await;
        let reviews: Vec<_> = world().asked().into_iter().filter(|asked| asked.method == "POST" && asked.path.ends_with("/pulls/19/reviews")).collect();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].body.clone().unwrap(), json!({"event":"APPROVE","body":"Looks fine","comments":[]}));
        assert_eq!(control_attribute(&app.dom, "Review summary", "value").unwrap().1.as_deref(), Some(""), "the form clears when the host has the review");
        assert_eq!(error(app.state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_reviewer_and_label_menus_read_fresh_each_time_and_each_change_is_sent_once() {
    with_app(20, &WRITER, async |app| {
        app.github.passing();
        app.open_panel().await;
        assert_eq!(app.asked("reviewerCandidates") + app.asked("labelCandidates"), 0, "nothing is held while a menu is closed");
        click_control(&mut app.dom, "Request a review");
        until(&mut app.dom, "the people", |dom| control(dom, "carol").is_some()).await;
        assert_eq!(app.asked("reviewerCandidates"), 1);
        key_on(&mut app.dom, "Request a review", Key::Escape);
        assert!(control(&app.dom, "carol").is_none(), "Escape closes the menu");
        click_control(&mut app.dom, "Request a review");
        until(&mut app.dom, "the people again", |dom| control(dom, "carol").is_some()).await;
        assert_eq!(app.asked("reviewerCandidates"), 2, "read again at the next opening");
        click_control(&mut app.dom, "carol");
        // Rows are never `disabled` while the change is in flight: a disabled button drops the focus, and
        // with it the Escape key that closes the menu.
        assert_ne!(control_attribute(&app.dom, "carol", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
        wait_for_rendered_text(&mut app.dom, "Review requested from carol").await;
        let posted: Vec<_> = world().asked().into_iter().filter(|asked| asked.method == "POST" && asked.path.ends_with("/pulls/20/requested_reviewers")).collect();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].body.clone().unwrap()["reviewers"], json!(["carol"]));
        // A click anywhere else closes the menu.
        assert!(control(&app.dom, "carol").is_some());
        click_control(&mut app.dom, "Close Request a review menu");
        assert!(control(&app.dom, "carol").is_none(), "a click outside closes the menu");
        click_control(&mut app.dom, "Change labels");
        until(&mut app.dom, "the labels", |dom| control(dom, "docs").is_some()).await;
        click_control(&mut app.dom, "docs");
        until(&mut app.dom, "the label request", |_| world().asked().iter().any(|asked| asked.method == "POST" && asked.path.contains("/labels"))).await;
        assert_eq!(world().asked().iter().filter(|asked| asked.method == "POST" && asked.path.contains("/labels")).count(), 1);
        assert_eq!(error(app.state), None);
    })
    .await;
}

// ---- reactions and rewriting words ------------------------------------------------------------

use crate::runtime::transport_tests::{all_controls, click_nth_control};

fn pressed(dom: &VirtualDom, label: &str) -> Option<bool> {
    control_attribute(dom, label, "aria-pressed").map(|(_, value)| value.as_deref() == Some("true"))
}
fn asked_count(kind: &str) -> usize {
    world().asked().iter().filter(|asked| kind_of(asked) == kind).count()
}
fn sent(kind: &str) -> Vec<Value> {
    world().asked().into_iter().filter(|asked| kind_of(asked) == kind).map(|asked| asked.body.unwrap()["variables"].clone()).collect()
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_reaction_shows_at_once_is_pressed_once_and_is_what_the_host_then_says() {
    with_app(30, &WRITER, async |app| {
        app.open_panel().await;
        wait_for_rendered_text(&mut app.dom, "first remark").await;
        assert!(control(&app.dom, "thumbs up, 1").is_none());
        let gate = app.github.hold_writes();
        click_nth_control(&mut app.dom, "Add a reaction", 1);
        assert!(control(&app.dom, "thumbs up").is_some(), "the picker opens");
        key_on(&mut app.dom, "thumbs up", Key::Escape);
        assert!(control(&app.dom, "thumbs up").is_none(), "Escape closes the picker");
        click_nth_control(&mut app.dom, "Add a reaction", 1);
        click_control(&mut app.dom, "thumbs up");
        // The pill is there before the host has said anything, and cannot be pressed again meanwhile.
        assert_eq!(pressed(&app.dom, "thumbs up, 1"), Some(true));
        assert!(disabled(&app.dom, "thumbs up, 1"));
        force_click_control(&mut app.dom, "thumbs up, 1");
        gate.release();
        until(&mut app.dom, "the host to have the reaction", |_| asked_count("react") == 1).await;
        until(&mut app.dom, "the pill to be pressable again", |dom| !disabled(dom, "thumbs up, 1")).await;
        assert_eq!(pressed(&app.dom, "thumbs up, 1"), Some(true), "what the host now says");
        assert_eq!((app.asked("react"), app.asked("unreact")), (1, 0), "one press, one request");
        assert_eq!(sent("react")[0]["content"], "THUMBS_UP");
        // Pressing it again takes it back.
        click_control(&mut app.dom, "thumbs up, 1");
        until(&mut app.dom, "the reaction to be gone", |dom| control(dom, "thumbs up, 1").is_none() && asked_count("unreact") == 1).await;
        assert_eq!(error(app.state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_reaction_the_host_refuses_goes_back_and_says_so() {
    with_app(31, &WRITER, async |app| {
        app.github.set(|data| data.write_refusal = Some("Resource not accessible".into()));
        app.open_panel().await;
        wait_for_rendered_text(&mut app.dom, "first remark").await;
        click_nth_control(&mut app.dom, "Add a reaction", 1);
        click_control(&mut app.dom, "heart");
        assert_eq!(pressed(&app.dom, "heart, 1"), Some(true));
        wait_for_rendered_text(&mut app.dom, "The reaction could not be saved").await;
        assert!(control(&app.dom, "heart, 1").is_none(), "the refused reaction is taken back");
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn only_your_own_remark_can_be_rewritten_and_a_failed_save_keeps_the_words() {
    with_app(32, &WRITER, async |app| {
        app.open_panel().await;
        wait_for_rendered_text(&mut app.dom, "first remark").await;
        assert!(control(&app.dom, "Edit comment").is_none(), "bob's remark is not yours to rewrite");
        click_control(&mut app.dom, "Comment on pull request");
        input_control(&mut app.dom, "Comment on this pull request", "Ship it please");
        click_control(&mut app.dom, "Comment");
        wait_for_rendered_text(&mut app.dom, "Ship it please").await;
        assert_eq!(all_controls(&app.dom, "Edit comment").len(), 1);

        // Escape leaves the editor and sends nothing.
        click_control(&mut app.dom, "Edit comment");
        input_control(&mut app.dom, "Edit comment", "Ship it never");
        key_on(&mut app.dom, "Edit comment", Key::Escape);
        assert!(control(&app.dom, "Save Edit comment").is_none() && rendered_text(&app.dom).contains("Ship it please"));
        assert_eq!(app.asked("editComment"), 0);

        // A refused save keeps the editor open with what was typed.
        app.github.set(|data| data.write_refusal = Some("Resource not accessible".into()));
        click_control(&mut app.dom, "Edit comment");
        input_control(&mut app.dom, "Edit comment", "Ship it now");
        click_control(&mut app.dom, "Save Edit comment");
        assert!(disabled(&app.dom, "Save Edit comment"), "one save in flight");
        wait_for_rendered_text(&mut app.dom, "Could not save the comment").await;
        assert_eq!(control(&app.dom, "Edit comment").unwrap().1.as_deref(), Some("Ship it now"));
        assert!(!disabled(&app.dom, "Save Edit comment"));

        // Once the host takes it, the words are the host's.
        app.github.set(|data| data.write_refusal = None);
        click_control(&mut app.dom, "Save Edit comment");
        wait_for_rendered_text(&mut app.dom, "Ship it now").await;
        assert_eq!(sent("editComment").len(), 2);
        assert_eq!(sent("editComment")[1]["body"], "Ship it now");
        assert!(control(&app.dom, "Save Edit comment").is_none());
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_title_and_description_are_rewritten_once_and_an_unchanged_title_sends_nothing() {
    with_app(33, &WRITER, async |app| {
        app.open_panel().await;
        // Unchanged, or empty, closes without a request.
        click_control(&mut app.dom, "Edit title");
        key_on(&mut app.dom, "Pull request title", Key::Enter);
        assert!(control(&app.dom, "Pull request title").is_none() && app.asked("update") == 0);
        click_control(&mut app.dom, "Edit title");
        input_control(&mut app.dom, "Pull request title", "   ");
        assert!(disabled(&app.dom, "Save title"));
        key_on(&mut app.dom, "Pull request title", Key::Escape);
        assert!(control(&app.dom, "Pull request title").is_none());

        click_control(&mut app.dom, "Edit title");
        input_control(&mut app.dom, "Pull request title", "  A better title ");
        click_control(&mut app.dom, "Save title");
        assert!(disabled(&app.dom, "Save title"), "one save in flight");
        wait_for_rendered_text(&mut app.dom, "A better title").await;
        assert_eq!(sent("update")[0]["title"], "A better title", "the title is trimmed");

        click_control(&mut app.dom, "Edit description");
        input_control(&mut app.dom, "Edit description", "Now **bold** words");
        click_control(&mut app.dom, "Save Edit description");
        wait_for_rendered_text(&mut app.dom, "Now").await;
        assert_eq!(app.asked("update"), 2);
        assert_eq!(sent("update")[1]["body"], "Now **bold** words");
        assert_eq!(error(app.state), None);
    })
    .await;
}

// ---- the thread's own links -------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_pull_request_is_filed_under_the_thread_and_taken_out_from_its_panel() {
    with_app(40, &WRITER, async |app| {
        app.open_panel().await;
        let thread_links = |state: Store<UiModel>| -> usize {
            let snapshot = state.peek().shell.snapshot.clone().unwrap();
            let thread = snapshot.threads.iter().find(|thread| thread.id.as_str() == "thread-a").unwrap();
            t3_client::thread_pull_requests::links(&serde_json::to_value(thread).unwrap()).len()
        };
        assert_eq!(thread_links(app.state), 0);
        assert!(control(&app.dom, "Unlink from this thread").is_none());
        click_control(&mut app.dom, "Link to this thread");
        assert!(disabled(&app.dom, "Link to this thread"), "one request while it is in flight");
        until(&mut app.dom, "the link", |dom| control(dom, "Unlink from this thread").is_some()).await;
        assert_eq!(thread_links(app.state), 1);
        let link = &app.state.peek().shell.snapshot.clone().unwrap().threads.iter().find(|thread| thread.id.as_str() == "thread-a").map(|thread| serde_json::to_value(thread).unwrap()).unwrap()["pullRequests"][0];
        assert_eq!((link["host"].as_str(), link["repository"].as_str(), link["number"].as_u64(), link["source"].as_str()), (Some("github.com"), Some("acme/web"), Some(40), Some("manual")));
        // The panel's pull request is also the branch's: the badge already offers it, not a second button.
        assert!(control(&app.dom, "Linked pull request #40").is_none());
        click_when_enabled(&mut app.dom, "Unlink from this thread").await;
        until(&mut app.dom, "the link gone", |dom| control(dom, "Link to this thread").is_some()).await;
        assert_eq!(thread_links(app.state), 0);
        assert_eq!(error(app.state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_thread_row_carries_its_pull_requests_state_and_opens_the_panel_on_it() {
    with_app(43, &WRITER, async |app| {
        app.open_panel().await;
        assert!(control(&app.dom, "PR #43*").is_none(), "nothing is filed under the thread, so its row has no badge");
        click_control(&mut app.dom, "Link to this thread");
        until(&mut app.dom, "the badge on the row", |dom| control(dom, "PR #43*").is_some()).await;
        // The badge says what the request is in its name and wears the state's class.
        let name = control_attribute(&app.dom, "PR #43*", "aria-label").unwrap().1.unwrap();
        assert!(name.starts_with("PR #43"), "{name}");
        let class = control_attribute(&app.dom, "PR #43*", "class").unwrap().1.unwrap();
        assert!(class.contains("thread-pr") && class.contains("pr-open"), "{class}");
        // Clicking it opens the panel on that request, in its thread, as it opens from the header.
        click_control(&mut app.dom, "Close pull request panel");
        assert_eq!(crate::right_panel::shown_pull_request(app.state), None);
        click_control(&mut app.dom, "PR #43*");
        until(&mut app.dom, "the panel", |_| crate::right_panel::shown_pull_request(app.state) == Some(43)).await;
        // The way out: unlinked, the row carries nothing.
        wait_for_rendered_text(&mut app.dom, "Please").await;
        click_when_enabled(&mut app.dom, "Unlink from this thread").await;
        until(&mut app.dom, "the badge gone", |dom| control(dom, "PR #43*").is_none()).await;
        assert_eq!(error(app.state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_line_comment_is_collected_under_its_line_and_sent_with_the_review() {
    with_app(44, &WRITER, async |app| {
        app.github.passing();
        app.open_panel().await;
        click_control(&mut app.dom, "Code");
        wait_for_rendered_text(&mut app.dom, "nit about a").await;
        // The thread sits under its line (a.ts line 1, on the new side), not below the file.
        assert!(control(&app.dom, "Reply to thread on a.ts").is_some());
        // Start a comment on the added line. The deleted line is another line with the same number.
        click_nth_control(&mut app.dom, "Comment on a.ts line 1", 1);
        assert!(control(&app.dom, "Line comment on a.ts:1").is_some());
        assert!(disabled(&app.dom, "Add to review"), "a comment needs words");
        input_control(&mut app.dom, "Line comment on a.ts:1", "  Rename this please  ");
        click_control(&mut app.dom, "Add to review");
        // Nothing is sent: a review is one request with the verdict. The row holds the pending comment and the composer counts it.
        assert!(control(&app.dom, "Line comment on a.ts:1").is_none() && control(&app.dom, "Discard pending comment on a.ts:1").is_some());
        assert!(control(&app.dom, "Review pull request, 1 comment pending").is_some());
        assert!(rendered_text(&app.dom).contains("Rename this please"));
        assert_eq!(world().asked().iter().filter(|asked| asked.method == "POST" && asked.path.ends_with("/pulls/44/reviews")).count(), 0);
        // Cancelled, a draft is dropped and nothing is kept.
        click_nth_control(&mut app.dom, "Comment on b.ts line 1", 1);
        click_control(&mut app.dom, "Cancel line comment");
        assert!(control(&app.dom, "Line comment on b.ts:1").is_none());
        // The review carries it: a comment-only review with no summary is sendable because it holds a line comment.
        click_control(&mut app.dom, "Review pull request, 1 comment pending");
        assert_eq!(pressed(&app.dom, "Review mode"), Some(true), "opening picks the mode with work waiting in it");
        click_control(&mut app.dom, "Submit review");
        wait_for_rendered_text(&mut app.dom, "Review submitted").await;
        let reviews: Vec<_> = world().asked().into_iter().filter(|asked| asked.method == "POST" && asked.path.ends_with("/pulls/44/reviews")).collect();
        assert_eq!(reviews.len(), 1);
        let body = reviews[0].body.clone().unwrap();
        assert_eq!(body["event"], "COMMENT");
        assert_eq!(body["comments"].as_array().map(Vec::len), Some(1), "{body}");
        assert_eq!((body["comments"][0]["path"].as_str(), body["comments"][0]["body"].as_str()), (Some("a.ts"), Some("Rename this please")), "{body}");
        // Sent, the pending comment is gone from the row and from the count.
        until(&mut app.dom, "the pending comment gone", |dom| control(dom, "Discard pending comment on a.ts:1").is_none()).await;
        assert!(control(&app.dom, "Comment on pull request").is_some() || control(&app.dom, "Review pull request").is_some());
        assert_eq!(error(app.state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_pull_request_filed_under_the_thread_by_anything_else_shows_in_its_header() {
    with_app(41, &WRITER, async |app| {
        app.open_panel().await;
        let transport = app.props.transport.clone();
        let link = json!({"threadId":"thread-a","host":"github.com","repository":"acme/web","number":99,"url":"https://github.com/acme/web/pull/99","source":"agent"});
        app.dom.in_scope(ScopeId::APP, || command(&transport, app.state, "thread.pull-request.link", link)).unwrap();
        until(&mut app.dom, "the header link", |dom| control(dom, "Linked pull request #99").is_some()).await;
        // Choosing it opens the panel on that pull request, in this thread.
        click_control(&mut app.dom, "Linked pull request #99");
        until(&mut app.dom, "the panel", |_| crate::right_panel::shown_pull_request(app.state) == Some(99)).await;
    })
    .await;
}

// ---- the list, and starting a thread from a pull request --------------------------------------

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_list_comes_from_the_hosts_shows_their_sizes_and_the_threads_filed_under_one() {
    with_app(50, &WRITER, async |app| {
        app.open_panel().await;
        click_control(&mut app.dom, "Link to this thread");
        until(&mut app.dom, "the link", |dom| control(dom, "Unlink from this thread").is_some()).await;
        app.state.view().set(View::PullRequests);
        wait_for_rendered_text(&mut app.dom, "#50").await;
        // The size is the host's own count for the page, read after the page.
        wait_for_rendered_text(&mut app.dom, "+5 \u{2212}1").await;
        assert_eq!((app.asked("search"), app.asked("stats")), (1, 1));
        assert!(rendered_text(&app.dom).contains("Fix it") && rendered_text(&app.dom).contains("bug"));
        // Opening the row shows the pull request beside the list, belonging to no thread; leaving the page takes it away.
        app.state.pr_panel().set(None);
        click_control(&mut app.dom, "Open pull request #50");
        until(&mut app.dom, "the panel beside the list", |dom| control(dom, "Pull request panel").is_some() && rendered_text(dom).contains("Please")).await;
        assert!(app.state.peek().pr_panel.as_ref().is_some_and(|target| target.reference.number == 50 && target.thread_id.is_empty()));
        assert_eq!(control_attribute(&app.dom, "Open pull request #50", "aria-pressed").and_then(|(_, value)| value).as_deref(), Some("true"), "the row says which pull request is open");
        app.state.view().set(View::Chat);
        until(&mut app.dom, "the panel gone with the page", |_| app.state.peek().pr_panel.is_none()).await;
        app.state.view().set(View::PullRequests);
        wait_for_rendered_text(&mut app.dom, "#50").await;
        // Who it is filed under is read when asked for, and opens that thread.
        assert_eq!(world().asked().iter().filter(|asked| asked.path.contains("linkedThreads")).count(), 0);
        click_when_enabled(&mut app.dom, "Linked threads of pull request #50").await;
        until(&mut app.dom, "the threads", |dom| control(dom, "Open thread thread-a").is_some()).await;
        click_control(&mut app.dom, "Open thread thread-a");
        until(&mut app.dom, "the thread", |_| app.state.peek().active_thread.as_deref() == Some("thread-a") && app.state.peek().view == View::Chat).await;
        assert_eq!(error(app.state), None);
    })
    .await;
}

/// The page opened at its address is mounted before the connection can read anything. It reads when the
/// connection becomes ready, and again when a lost connection returns: the rows stay, marked old, meanwhile.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_list_opened_by_its_address_reads_once_the_connection_is_ready_and_again_when_it_returns() {
    let world = world();
    let _alone = world.exclusive().lock().await;
    world.reset_everything_signed_in();
    let _github = Github::install(world, 71);
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["remote", "add", "origin", "https://github.com/acme/web.git"]);
    git(&fixture.workspace, &["checkout", "-q", "-b", "feature"]);
    // The project is the server's, from an earlier visit.
    let (mut first, props) = fixture.client(Gate::default(), &WRITER);
    seed(&mut first, &props, &fixture.workspace).await;
    drop(first);

    let (mut dom, _props, state, _browser) = super::routing_tests::cold(&fixture, "/pull-requests", &WRITER, |_| {});
    until(&mut dom, "the page named by the address", |_| state.peek().view == View::PullRequests).await;
    assert!(!state.peek().shell.synchronized, "the page was mounted before the server had said anything");
    // The server keeps a list by state across the servers of this test binary, so whichever pull request another test
    // listed first may be the one that answers; the number is read from the page.
    wait_for_rendered_text(&mut dom, "Fix it #").await;
    assert!(!rendered_text(&dom).contains("Loading pull requests"), "{}", rendered_text(&dom));
    let text = rendered_text(&dom);
    let number: String = text[text.find("Fix it #").unwrap() + "Fix it #".len()..].chars().take_while(char::is_ascii_digit).collect();
    let (start, refresh) = (format!("Start a thread from pull request #{number}"), "Refresh pull requests");

    // The connection is lost: what was read stays, marked old, and nothing can be started from it.
    state.status().set(ConnectionStatus::Interrupted("test".into()));
    wait_for_rendered_text(&mut dom, "Offline. This is the last list read.").await;
    assert!(rendered_text(&dom).contains("Fix it #") && disabled(&dom, &start) && disabled(&dom, refresh));
    // It returns: the list is read again, which is what takes the mark away.
    state.status().set(ConnectionStatus::Connected);
    until(&mut dom, "a fresh list", |dom| !rendered_text(dom).contains("Offline. This is the last list read.") && !disabled(dom, refresh)).await;
    assert!(rendered_text(&dom).contains("Fix it #") && !disabled(&dom, &start));
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

/// The files of a pull request are read again at each notice, and once the connection is back: the revision
/// and the grant are the tab's inputs, and a copy of them taken at mount would never read again.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_files_tab_reads_the_patch_again_at_a_refresh_notice() {
    with_app(72, &WRITER, async |app| {
        app.open_panel().await;
        click_control(&mut app.dom, "Code");
        wait_for_rendered_text(&mut app.dom, "+b").await;
        app.github.set(|data| data.patch = "diff --git a/c.ts b/c.ts\n--- a/c.ts\n+++ b/c.ts\n@@ -1 +1 @@\n-x\n+changed in review\n".into());
        let payload = json!({"reference":reference(app)});
        app.dom.in_scope(ScopeId::APP, || request(&app.props.transport, app.state, "pullRequests.invalidate", payload, RequestKind::Unary)).unwrap();
        wait_for_rendered_text(&mut app.dom, "changed in review").await;
        assert_eq!(error(app.state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_thread_starts_from_a_pull_request_in_a_dialog_that_checks_what_is_typed_and_leaves_with_escape() {
    with_app(51, &WRITER, async |app| {
        app.open_panel().await;
        app.state.view().set(View::PullRequests);
        until(&mut app.dom, "the row", |dom| control(dom, "Start a thread from pull request #51").is_some()).await;
        click_control(&mut app.dom, "Start a thread from pull request #51");
        until(&mut app.dom, "the dialog", |dom| control(dom, "Checkout pull request").is_some()).await;
        assert_eq!(control(&app.dom, "Pull request").unwrap().1.as_deref(), Some("https://github.com/acme/web/pull/51"));
        // Nothing can be prepared before the host has resolved it.
        assert!(disabled(&app.dom, "Local") && disabled(&app.dom, "Worktree"));
        input_control(&mut app.dom, "Pull request", "not a pull request");
        assert!(disabled(&app.dom, "Resolve pull request"));
        assert!(rendered_text(&app.dom).contains("Use a pull request URL, checkout command, 123, or #123."));
        input_control(&mut app.dom, "Pull request", "");
        assert!(rendered_text(&app.dom).contains("Paste a pull request URL, checkout command, or enter 123 / #123."));
        key_on(&mut app.dom, "Checkout pull request", Key::Escape);
        assert!(control(&app.dom, "Checkout pull request").is_none(), "Escape closes it");
        assert!(app.state.peek().pr_checkout.is_none());
    })
    .await;
}

// ---- the tests the panel's earlier batches listed as missing ------------------------------------

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn unchanged_lines_between_hunks_expand_from_the_hosts_file_and_only_when_asked() {
    with_app(60, &WRITER, async |app| {
        app.github.set(|data| data.patch = "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -10,3 +10,3 @@\n line 10\n-line 11\n+LINE 11\n line 12\n".into());
        app.open_panel().await;
        click_control(&mut app.dom, "Code");
        until(&mut app.dom, "the gap", |dom| control(dom, "Expand 9 unchanged lines in a.ts").is_some()).await;
        let contents = || world().asked().iter().filter(|asked| asked.path.contains("/contents/")).count();
        assert_eq!(contents(), 0, "the file is not read until a gap is opened");
        assert!(!rendered_text(&app.dom).contains("line 3"));
        click_control(&mut app.dom, "Expand 9 unchanged lines in a.ts");
        wait_for_rendered_text(&mut app.dom, "line 3").await;
        wait_for_rendered_text(&mut app.dom, "line 9").await;
        assert!(contents() >= 1, "the host's file was read");
        assert!(control(&app.dom, "Expand 9 unchanged lines in a.ts").is_none(), "the gap is gone once it is all shown");
        assert_eq!(error(app.state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_review_thread_with_more_comments_pages_the_next_one_in_from_its_cursor() {
    with_app(61, &WRITER, async |app| {
        app.github.set(|data| {
            data.threads[0]["comments"]["pageInfo"] = json!({"hasNextPage":true,"endCursor":"c1"});
            data.threads[0]["comments"]["totalCount"] = json!(2);
        });
        app.open_panel().await;
        click_control(&mut app.dom, "Code");
        wait_for_rendered_text(&mut app.dom, "nit about a").await;
        assert!(control(&app.dom, "Show more comments in the thread on a.ts").is_some());
        // The next page is the host's, once per press, and the control goes when there is no more.
        app.github.set(|data| {
            let nodes = data.threads[0]["comments"]["nodes"].as_array_mut().unwrap();
            nodes.push(json!({"id":"RC_2","author":super::fake_github::actor("carol"),"body":"second page remark","createdAt":"2026-01-02T01:00:00Z","url":"https://t/9","reactionGroups":[]}));
        });
        click_control(&mut app.dom, "Show more comments in the thread on a.ts");
        wait_for_rendered_text(&mut app.dom, "second page remark").await;
        assert_eq!(app.asked("threadComments"), 1);
        let asked = world().asked().into_iter().find(|asked| kind_of(asked) == "threadComments").unwrap();
        assert_eq!(asked.body.unwrap()["variables"]["cursor"], "c1", "the page after the one the thread carried");
        assert!(control(&app.dom, "Show more comments in the thread on a.ts").is_none());
        assert_eq!(error(app.state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn toggling_a_label_sends_exactly_that_label_to_the_host() {
    with_app(62, &WRITER, async |app| {
        app.github.passing();
        app.open_panel().await;
        click_control(&mut app.dom, "Change labels");
        until(&mut app.dom, "the labels", |dom| control(dom, "docs").is_some()).await;
        click_control(&mut app.dom, "docs");
        until(&mut app.dom, "the label request", |_| world().asked().iter().any(|asked| asked.method == "POST" && asked.path.contains("/labels"))).await;
        let posted: Vec<_> = world().asked().into_iter().filter(|asked| asked.method == "POST" && asked.path.contains("/labels")).collect();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].body.clone().unwrap(), json!({"labels":["docs"]}), "adding applies the one label and no other");
        assert!(posted[0].path.ends_with("/issues/62/labels"), "{}", posted[0].path);
        // A label the pull request already has is taken off by name, not by replacing the set. The rows take no click while the
        // first change is under way, so it is waited out.
        until(&mut app.dom, "the first change to finish", |dom| control_attribute(dom, "bug", "aria-disabled").and_then(|(_, value)| value).as_deref() == Some("false")).await;
        click_control(&mut app.dom, "bug");
        until(&mut app.dom, "the removal", |_| world().asked().iter().any(|asked| asked.method == "DELETE" && asked.path.contains("/labels/"))).await;
        let removed: Vec<_> = world().asked().into_iter().filter(|asked| asked.method == "DELETE" && asked.path.contains("/labels/")).collect();
        assert_eq!(removed.len(), 1);
        assert!(removed[0].path.ends_with("/labels/bug"), "{}", removed[0].path);
        assert_eq!(error(app.state), None);
    })
    .await;
}

// ---- The `#` menu of the message box. ----

fn draft(app: &Mounted) -> String {
    app.state.draft().peek().clone()
}

/// `#` lists the project's pull requests from the host, a number or words narrow them, and picking
/// one names it in the text and attaches it as a chip that is sent with the message.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_hash_menu_lists_the_projects_pull_requests_and_attaches_the_one_picked() {
    with_app(31, &READER, async |app| {
        wait_for_rendered_text(&mut app.dom, &format!("PR #{} \u{b7} Open", app.number)).await;
        input_control(&mut app.dom, "Message", "see #");
        wait_for_rendered_text(&mut app.dom, "Fix it").await;
        let text = rendered_text(&app.dom);
        assert!(text.contains("#31") && text.contains("Open"), "{text}");
        // A number that no pull request has: the menu says so rather than closing.
        input_control(&mut app.dom, "Message", "see #99999");
        wait_for_rendered_text(&mut app.dom, "No pull request matches 99999.").await;
        // Words go to the host once typing pauses, and what a searching host answers is offered.
        input_control(&mut app.dom, "Message", "see #fix");
        until(&mut app.dom, "the search", |dom| rendered_text(dom).contains("Fix it") && !rendered_text(dom).contains("No pull request matches")).await;
        input_control(&mut app.dom, "Message", "see #31");
        until(&mut app.dom, "the number", |dom| control(dom, "Remove #31").is_none() && rendered_text(dom).contains("Fix it")).await;
        // Picking replaces the `#31` with a reference and attaches the chip; Escape would only have closed the menu.
        key_on(&mut app.dom, "Message", Key::Enter);
        until(&mut app.dom, "the chip", |dom| control(dom, "Remove #31").is_some()).await;
        let text = draft(app);
        assert!(text.starts_with("see [#31](t3-context://v1/review-comment/"), "{text}");
        assert!(text.ends_with(") "), "{text}");
        let chips = crate::composer_chips::contexts_of(app.state, &crate::composer_chips::open_target(app.state).unwrap());
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0]["pullRequest"]["number"], 31);
        assert!(chips[0]["text"].as_str().unwrap().contains("untrusted data"));
    })
    .await;
}

/// A project whose host this server cannot read pull requests from says so in the menu.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_hash_menu_says_when_pull_requests_are_not_available_for_the_project() {
    // The hosts the server asks, and what it remembers of them per project, are shared by the test
    // binary: this project has an id of its own, and no other test may be asking meanwhile.
    let world = world();
    let _alone = world.exclusive().lock().await;
    world.reset_everything_signed_in();
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.client(Gate::default(), &READER);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "shell", |_| state.peek().shell.synchronized).await;
    dom.in_scope(ScopeId::APP, || request(&props.transport, state, "projects.mutate", json!({"type":"project.create","commandId":"no-host","projectId":"no-host","title":"No host","workspaceRoot":fixture.workspace}), RequestKind::Unary)).unwrap();
    until(&mut dom, "project", |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| !s.projects.is_empty())).await;
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.create", json!({"createdBy":"user","creationSource":"web","threadId":"lonely","projectId":"no-host","title":"Lonely","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null}))).unwrap();
    until(&mut dom, "thread", |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| !s.threads.is_empty())).await;
    open_thread(&mut dom, &props, state, "lonely").await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    input_control(&mut dom, "Message", "see #");
    wait_for_rendered_text(&mut dom, "Pull requests are not available for this project.").await;
    fixture.finish(dom).await;
}

/// Reading the host and typing in the menu repaint the message box and nothing else.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_hash_menu_repaints_neither_the_application_nor_the_conversation() {
    with_probed_app(32, &READER, async |app| {
        let probe = app.probe.clone().unwrap();
        // One round trip first, so the tail of what opened the thread has been delivered.
        wait_for_rendered_text(&mut app.dom, &format!("PR #{} \u{b7} Open", app.number)).await;
        input_control(&mut app.dom, "Message", "x");
        let before = repaints(app.state, &probe);
        input_control(&mut app.dom, "Message", "see #");
        wait_for_rendered_text(&mut app.dom, "Fix it").await;
        input_control(&mut app.dom, "Message", "see #fi");
        wait_for_rendered_text(&mut app.dom, "Fix it").await;
        let after = repaints(app.state, &probe);
        // Typing is the composer's; the host's answer and the menu are inside it. The application (and so the
        // sidebar) and the conversation are not built again, and no shell or thread event explains it if they were.
        assert_eq!((after.0.0, after.0.1), (before.0.0, before.0.1), "the application and the conversation");
        assert_eq!(after.1, before.1, "no event of the server was involved");
        assert!(after.0.2 > before.0.2, "the composer shows what is typed");
    })
    .await;
}

/// Where the host's answers about a stack are counted: reading it, and the one write that merges it.
fn stack_reads() -> usize {
    world().asked().iter().filter(|asked| kind_of(asked) == "GET repos/acme/web/stacks").count()
}
fn stack_merges(number: u64) -> Vec<Value> {
    world().asked().iter().filter(|asked| kind_of(asked) == format!("PUT repos/acme/web/pulls/{number}/merge-async")).filter_map(|asked| asked.body.clone()).collect()
}
/// The host's stack was scripted after the panel's thread was open, by which time the server may have asked about the pull request
/// and been told there is no stack; the panel's refresh goes around what it kept.
async fn read_the_stack_afresh(app: &mut Mounted) {
    click_when_enabled(&mut app.dom, "Refresh pull request").await;
}

/// A stack of three open layers, the middle one being pull request `n`, numbered by it: each test has its own, since the server
/// keeps what the host said of a stack for a while and tests share the host's address.
fn three_layers(n: u64, shas: [&str; 3]) -> Value {
    super::fake_github::stack_of(n, &[(n - 1, "open", false, shas[0]), (n, "open", false, shas[1]), (n + 1, "open", false, shas[2])])
}
fn query_has(asked: &super::source_control_tests::Asked, text: &str) -> bool {
    asked.body.as_ref().and_then(|body| body["query"].as_str()).is_some_and(|query| query.contains(text))
}

/// A pull request in the host's stack is merged with the layers under it, after the question, naming the heads that were read;
/// the stack is read again whatever the answer, and a stack that moved is refused in the server's words.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_stack_is_merged_through_the_layer_in_view_after_asking_and_is_read_again_whatever_the_answer() {
    with_app(30, &WRITER, async |app| {
        app.github.passing();
        app.github.set(|data| data.stack = Some(three_layers(30, ["aaa1111", "bbb2222", "ccc3333"])));
        app.open_panel().await;
        read_the_stack_afresh(app).await;
        until(&mut app.dom, "the stack in the header", |dom| control(dom, "Stack 30, layer 2 of 3").is_some() && control(dom, "Merge stack").is_some()).await;
        assert!(control(&app.dom, "Merge").is_none(), "a pull request in a stack is not merged on its own");

        // Asking sends nothing; the question says how many layers and with what.
        click_when_enabled(&mut app.dom, "Merge stack").await;
        wait_for_rendered_text(&mut app.dom, "Merge 2 pull requests?").await;
        assert!(rendered_text(&app.dom).contains("Merge #30 and its unmerged layers below into main using merge."));
        assert!(rendered_text(&app.dom).contains("Layer 29") && rendered_text(&app.dom).contains("Layer 30") && !rendered_text(&app.dom).contains("Layer 31"), "the layers it reaches, and not the one above");
        click_control(&mut app.dom, "Cancel");
        assert!(stack_merges(30).is_empty());

        // Confirmed: one request, to the layer in view, naming its head; the answer is said and the stack read again.
        let reads = stack_reads();
        click_when_enabled(&mut app.dom, "Merge stack").await;
        click_control(&mut app.dom, "Confirm Merge stack");
        wait_for_rendered_text(&mut app.dom, "GitHub merged the stack or added it to its merge queue.").await;
        assert!(rendered_text(&app.dom).contains("Stack merge request completed"));
        assert_eq!(stack_merges(30), [json!({"merge_method":"merge","merge_action":"default","sha":"bbb2222"})]);
        until(&mut app.dom, "the stack read again", |_| stack_reads() >= reads + 2).await;
        assert!(control(&app.dom, "Confirm Merge stack").is_none(), "the question is closed");

        // The host refuses: its reason is said, the stack is read again, and nothing is left half done on screen.
        app.github.set(|data| data.stack_merge = "failed");
        let reads = stack_reads();
        click_when_enabled(&mut app.dom, "Merge stack").await;
        click_control(&mut app.dom, "Confirm Merge stack");
        wait_for_rendered_text(&mut app.dom, "GitHub refused the stack merge.").await;
        assert!(rendered_text(&app.dom).contains("Stack operation did not complete"));
        until(&mut app.dom, "the stack read again after the refusal", |_| stack_reads() >= reads + 2).await;
        assert_eq!(stack_merges(30).len(), 2);

        // A layer moved since the panel read the stack: the server reads it again before sending anything, finds another head and
        // refuses; the panel then reads it as it is, and the next try names the head it now has.
        app.github.set(|data| {
            data.stack_merge = "merged";
            data.stack_list = Some(three_layers(30, ["aaa1111", "bbb9999", "ccc3333"]));
        });
        click_when_enabled(&mut app.dom, "Merge stack").await;
        click_control(&mut app.dom, "Confirm Merge stack");
        wait_for_rendered_text(&mut app.dom, "The stack changed. Refresh it before trying again.").await;
        assert_eq!(stack_merges(30).len(), 2, "nothing was sent for a stack that moved");
        app.github.set(|data| {
            data.stack = Some(three_layers(30, ["aaa1111", "bbb9999", "ccc3333"]));
            data.stack_list = None;
        });
        let details = || world().asked().iter().filter(|asked| kind_of(asked) == "GET repos/acme/web/stacks/30").count();
        let before = details();
        click_control(&mut app.dom, "Refresh pull request");
        until(&mut app.dom, "the stack read as it is", |_| details() > before).await;
        click_when_enabled(&mut app.dom, "Merge stack").await;
        click_control(&mut app.dom, "Confirm Merge stack");
        until(&mut app.dom, "the third merge", |_| stack_merges(30).len() == 3).await;
        assert_eq!(stack_merges(30)[2]["sha"], "bbb9999");
        assert_eq!(error(app.state), None);
    })
    .await;
}

/// Rebasing the stack names every open layer and the top one; the account's right to push to every branch is checked by the server
/// before a layer is touched, and its refusal is said in its words. A rebase of a stack that moved is refused too.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_stack_rebase_asks_first_names_every_open_layer_and_says_why_it_was_refused() {
    with_app(40, &WRITER, async |app| {
        app.github.passing();
        app.github.set(|data| {
            data.stack = Some(three_layers(40, ["aaa1111", "bbb2222", "ccc3333"]));
            data.stack_branches_writable = false;
        });
        app.open_panel().await;
        read_the_stack_afresh(app).await;
        until(&mut app.dom, "the stack in the header", |dom| control(dom, "Stack 40, layer 2 of 3").is_some()).await;
        click_when_enabled(&mut app.dom, "Stack 40, layer 2 of 3").await;
        until(&mut app.dom, "the menu", |dom| control(dom, "Rebase stack").is_some() && control(dom, "Merge stack (2)").is_some()).await;
        click_when_enabled(&mut app.dom, "Rebase stack").await;
        wait_for_rendered_text(&mut app.dom, "Rebase 3 pull requests?").await;
        assert!(rendered_text(&app.dom).contains("Rebase the remote branches from bottom to top onto main."));
        click_control(&mut app.dom, "Cancel");
        assert!(world().asked().iter().all(|asked| !query_has(asked, "maintainerCanModify")), "asking reads nothing about the branches");

        // Confirmed: the branches of every open layer are looked at, and the account may not push to them.
        click_when_enabled(&mut app.dom, "Stack 40, layer 2 of 3").await;
        click_when_enabled(&mut app.dom, "Rebase stack").await;
        click_control(&mut app.dom, "Confirm Rebase stack");
        wait_for_rendered_text(&mut app.dom, "You cannot update every branch in this stack.").await;
        assert!(rendered_text(&app.dom).contains("Stack operation did not complete"));
        let looked_at = world().asked().into_iter().find(|asked| query_has(asked, "maintainerCanModify")).and_then(|asked| asked.body).unwrap()["query"].as_str().unwrap().to_owned();
        assert!(looked_at.contains("pr39:") && looked_at.contains("pr40:") && looked_at.contains("pr41:"), "every open layer: {looked_at}");

        // A stack that moved is refused before any branch is read.
        app.github.set(|data| data.stack_list = Some(three_layers(40, ["aaa1111", "bbb2222", "ccc7777"])));
        let looks = world().asked().iter().filter(|asked| query_has(asked, "maintainerCanModify")).count();
        click_when_enabled(&mut app.dom, "Stack 40, layer 2 of 3").await;
        click_when_enabled(&mut app.dom, "Rebase stack").await;
        click_control(&mut app.dom, "Confirm Rebase stack");
        wait_for_rendered_text(&mut app.dom, "The stack changed. Refresh it before trying again.").await;
        assert_eq!(world().asked().iter().filter(|asked| query_has(asked, "maintainerCanModify")).count(), looks);
        assert_eq!(error(app.state), None);
    })
    .await;
}

/// What the stack allows follows the layers and the account: a draft under the layer in view stops the merge and says why; an account
/// that may not write sees the layers and no way to act on them; a pull request in no stack keeps its own merge once the lookup is done.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_stack_offers_only_what_the_layers_and_the_account_allow() {
    with_app(50, &WRITER, async |app| {
        app.github.passing();
        app.github.set(|data| data.stack = Some(super::fake_github::stack_of(50, &[(49, "open", true, "aaa1111"), (50, "open", false, "bbb2222")])));
        app.open_panel().await;
        read_the_stack_afresh(app).await;
        until(&mut app.dom, "the stack in the header", |dom| control(dom, "Stack 50, layer 2 of 2").is_some()).await;
        assert!(disabled(&app.dom, "Merge stack"), "a draft under the layer stops the merge");
        click_control(&mut app.dom, "Stack 50, layer 2 of 2");
        until(&mut app.dom, "the menu", |dom| control(dom, "Merge stack (2)").is_some()).await;
        assert!(rendered_text(&app.dom).contains("Every layer being merged must be open and ready for review."));
        assert!(disabled(&app.dom, "Merge stack (2)") && !disabled(&app.dom, "Rebase stack"));
        // The layers open their pull requests as tabs of the thread's panel.
        click_control(&mut app.dom, "Layer 49, #49");
        until(&mut app.dom, "the other layer", |_| crate::right_panel::shown_pull_request(app.state) == Some(49)).await;
    })
    .await;

    // No stack at the host: the pull request is merged on its own, once the lookup has said so.
    with_app(61, &WRITER, async |app| {
        app.github.passing();
        app.open_panel().await;
        read_the_stack_afresh(app).await;
        until(&mut app.dom, "its own merge", |dom| control(dom, "Merge").is_some()).await;
        assert!(control(&app.dom, "Merge stack").is_none() && control(&app.dom, "Stack 61, layer 1 of 1").is_none());
    })
    .await;

    // An account that cannot write: the stack is there to read and move through, with nothing to do to it.
    with_app(70, &READER, async |app| {
        app.github.set(|data| data.stack = Some(three_layers(70, ["aaa1111", "bbb2222", "ccc3333"])));
        app.open_panel().await;
        read_the_stack_afresh(app).await;
        until(&mut app.dom, "the stack in the header", |dom| control(dom, "Stack 70, layer 2 of 3").is_some()).await;
        click_control(&mut app.dom, "Stack 70, layer 2 of 3");
        until(&mut app.dom, "the menu", |dom| control(dom, "Stack #70").is_some()).await;
        assert!(control(&app.dom, "Merge stack").is_none() && control(&app.dom, "Merge stack (2)").is_none() && control(&app.dom, "Rebase stack").is_none());
    })
    .await;
}

/// A pull request's tab in the panel's bar wears the state it was last read in, and follows it.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_pull_requests_tab_wears_its_state_and_follows_it() {
    with_app(81, &WRITER, async |app| {
        app.open_panel().await;
        until(&mut app.dom, "the open glyph", |dom| control(dom, "Pull request state: Open").is_some()).await;
        app.github.set(|data| data.draft = true);
        click_when_enabled(&mut app.dom, "Refresh pull request").await;
        until(&mut app.dom, "the draft glyph", |dom| control(dom, "Pull request state: Draft").is_some()).await;
        assert!(control(&app.dom, "Pull request state: Open").is_none());
        app.github.set(|data| {
            data.draft = false;
            data.state = "MERGED";
        });
        click_when_enabled(&mut app.dom, "Refresh pull request").await;
        until(&mut app.dom, "the merged glyph", |dom| control(dom, "Pull request state: Merged").is_some()).await;
    })
    .await;
}

/// While the host's own read of the stack is on its way, the layers the threads saved for the pull request can be opened (an old
/// link keeps them), marked as refreshing and with nothing to do to them; the host's answer replaces them and offers the actions.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_stack_the_threads_saved_keeps_its_layers_open_to_the_eye_until_the_hosts_own_read_arrives() {
    with_app(80, &WRITER, async |app| {
        app.github.passing();
        app.github.set(|data| data.stack = Some(three_layers(80, ["aaa1111", "bbb2222", "ccc3333"])));
        let gate = app.github.hold_stack_reads();
        wait_for_rendered_text(&mut app.dom, "PR #80 \u{b7} Open").await;
        click_control(&mut app.dom, "pull request #80");
        wait_for_rendered_text(&mut app.dom, "Please").await;
        assert!(control(&app.dom, "Stack 80, layer 2 of 3").is_none(), "nothing is known of a stack yet");
        assert!(control(&app.dom, crate::pull_request_summary::CHECKING_STACK).is_some(), "the lookup is on its way");

        // The thread's link saved a stack at an earlier sync. Nothing is awaited between this and the look at the screen.
        let link: t3_contracts::ThreadPullRequestLink = serde_json::from_value(json!({
            "host":"github.com","repository":"acme/web","number":80,"url":"https://github.com/acme/web/pull/80","source":"manual","linkedAt":"2026-09-09T10:00:00Z",
            "snapshot":{"title":"Fix it","headBranch":"feature","baseBranch":"saved-79","state":"open","isDraft":false,"updatedAt":null,"syncedAt":"2026-09-09T10:00:00Z"},
            "stack":{"kind":"native","id":"stack","number":80,"url":"https://github.com/acme/web/stacks/80","base":"main","layers":[
                {"number":79,"headBranch":"saved-79","state":"open"},{"number":80,"headBranch":"feature","state":"open"},{"number":81,"headBranch":"saved-81","state":"open"}]}
        }))
        .unwrap();
        app.dom.in_scope(ScopeId::APP, || {
            let mut shell = app.state.shell();
            let mut shell = shell.write();
            let thread = shell.snapshot.as_mut().unwrap().threads.iter_mut().find(|thread| thread.id.as_str() == "thread-a").unwrap();
            thread.pull_requests = Some(Some(vec![link]));
        });
        app.dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert!(control(&app.dom, "Stack 80, layer 2 of 3").is_some(), "the saved layers are in the header");
        assert!(control(&app.dom, "Merge stack").is_none(), "nothing can be done to a stack the host has not just said");
        click_control(&mut app.dom, "Stack 80, layer 2 of 3");
        assert!(rendered_text(&app.dom).contains("Refreshing\u{2026}"), "{}", rendered_text(&app.dom));
        assert!(control(&app.dom, "saved-79, #79").is_some() && control(&app.dom, "Merge stack (2)").is_none() && control(&app.dom, "Rebase stack").is_none());

        // The host answers: its layers replace the saved ones, and the actions are offered.
        gate.release();
        until(&mut app.dom, "the host's stack", |dom| control(dom, "Merge stack").is_some()).await;
        // The menu was left open: it now lists the host's layers.
        assert!(control(&app.dom, "Layer 79, #79").is_some() && control(&app.dom, "saved-79, #79").is_none());
        assert!(!rendered_text(&app.dom).contains("Refreshing\u{2026}"));
        assert_eq!(error(app.state), None);
    })
    .await;
}
