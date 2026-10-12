//! What a thread row and the composer's goal strip say of the shell the server now sends, mounted on their own over shells the
//! test writes (the fake provider has no goal and no pull request host to ask): the label a working thread wears, the earlier
//! providers, when a person last wrote, and the glyph and number of the thread's pull requests with their states.
use super::*;
use crate::runtime::transport_tests::{control, control_attribute, rendered_text};
use crate::sidebar_rows::{self, Caps};

fn thread(extra: Value) -> t3_contracts::ThreadShell {
    let mut base = json!({"createdBy":"user","creationSource":"web","id":"thread","projectId":"p","title":"Work","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"m"},
        "runtimeMode":"approval-required","interactionMode":"default","branch":"feature/x","worktreePath":null,"lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":"thread"},"forkedFrom":null,
        "activeProviderThreadId":"pt","latestRunId":"r","activeRunId":"r","status":"running","pendingRuntimeRequest":null,"latestVisibleMessage":null,"latestUserMessageAt":"2026-01-01T00:00:00Z",
        "hasActionableProposedPlan":false,"itemCount":4,"visibleItemCount":4,"pendingBackgroundTasks":[],"providerInstanceHistory":["claude","codex"],"pullRequests":[],
        "activityRunStatus":"running","activityRunStartedAt":"2026-01-01T00:00:00Z","latestRunStartedAt":"2026-01-01T00:00:00Z","latestRunRequestedAt":"2026-01-01T00:00:00Z",
        "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","archivedAt":null,"settledOverride":null,"settledAt":null,"deletedAt":null});
    for (key, value) in extra.as_object().unwrap() {
        base[key] = value.clone();
    }
    serde_json::from_value(base).unwrap()
}
fn link(number: u64, snapshot: Value, extra: Value) -> Value {
    let mut link = json!({"host":"github.com","repository":"acme/web","number":number,"url":format!("https://github.com/acme/web/pull/{number}"),"source":"manual","linkedAt":"2026-01-02T00:00:00Z","snapshot":snapshot,"stack":null});
    for (key, value) in extra.as_object().into_iter().flatten() {
        link[key] = value.clone();
    }
    link
}
fn snapshot(state: &str, draft: bool, head: &str, base: &str) -> Value {
    json!({"state":state,"isDraft":draft,"title":format!("Title of {head}"),"headBranch":head,"baseBranch":base,"updatedAt":"2026-01-03T00:00:00Z","syncedAt":"2026-01-03T00:00:00Z"})
}

#[derive(Clone)]
struct Props {
    threads: Vec<t3_contracts::ThreadShell>,
    working: bool,
}
impl PartialEq for Props {
    fn eq(&self, other: &Self) -> bool {
        self.threads == other.threads && self.working == other.working
    }
}
fn harness(props: Props) -> Element {
    let state = use_store(UiModel::default);
    let providers: std::collections::BTreeMap<String, (String, String)> = [("claude".to_owned(), ("Claude".to_owned(), "claudeAgent".to_owned())), ("codex".to_owned(), ("Codex".to_owned(), "codex".to_owned()))].into();
    let _ = props.working;
    rsx! {
        for thread in props.threads.iter() {
            sidebar_rows::ThreadRow { key: "{thread.id}", view: sidebar_rows::view(&EnvironmentId::new("environment-a").unwrap(), true, true, thread, false, None, Caps::default(), true, chrono::DateTime::parse_from_rfc3339("2026-01-01T03:00:00Z").unwrap().with_timezone(&chrono::Utc), &providers), state, transport: TransportHandle::default() }
        }
    }
}
fn mount(threads: Vec<t3_contracts::ThreadShell>) -> VirtualDom {
    let mut dom = VirtualDom::new_with_props(harness, Props { threads, working: true });
    dom.rebuild_in_place();
    dom
}

#[test]
fn a_working_thread_says_goal_when_a_native_goal_keeps_it_going_and_when_a_person_last_wrote() {
    let plain = mount(vec![thread(json!({}))]);
    let text = rendered_text(&plain);
    assert!(text.contains("Working") && !text.contains("Goal"), "{text}");
    // The row is dated by the last message a person wrote (three hours before the clock the test gives it), not by `updatedAt`.
    assert!(text.contains("3h"), "{text}");
    let goal = mount(vec![thread(json!({"goal":{"objective":"Ship it","status":"active"}}))]);
    let text = rendered_text(&goal);
    assert!(text.contains("Goal") && !text.contains("Working"), "{text}");
    // A paused goal does not keep the agent going: the thread is just working.
    let paused = mount(vec![thread(json!({"goal":{"objective":"Ship it","status":"paused"}}))]);
    assert!(rendered_text(&paused).contains("Working"));
}

#[test]
fn the_providers_that_had_the_thread_before_are_named_and_the_current_one_is_not() {
    let handed_off = mount(vec![thread(json!({}))]);
    // The earlier providers are drawn as their marks, named for a screen reader and the tooltip.
    assert!(control(&handed_off, "Claude").is_some() && control(&handed_off, "Codex").is_none(), "{}", rendered_text(&handed_off));
    assert!(!rendered_text(&handed_off).contains("after "), "icons, not words");
    let never = mount(vec![thread(json!({"providerInstanceHistory":["codex"]}))]);
    assert!(control(&never, "Codex").is_none() && control(&never, "Claude").is_none());
    // Before the thread has run, the model it will use is the current one.
    let idle = mount(vec![thread(json!({"latestRunId":null,"activeRunId":null,"activeProviderThreadId":null,"status":"idle","activityRunStatus":null,"modelSelection":{"instanceId":"claude","model":"m"},"providerInstanceHistory":["claude","codex"]}))]);
    assert!(control(&idle, "Codex").is_some() && control(&idle, "Claude").is_none());
}

#[test]
fn a_rows_pull_request_wears_its_states_glyph_and_a_stack_or_several_links_open_the_list() {
    let state_of = |dom: &VirtualDom, label: &str| control_attribute(dom, label, "class").and_then(|(_, class)| class).unwrap_or_default();
    let open = mount(vec![thread(json!({"pullRequests":[link(5, snapshot("open", false, "a", "main"), json!({}))]}))]);
    assert!(control(&open, "PR #5 - Open: Title of a").is_some());
    assert!(state_of(&open, "PR #5 - Open: Title of a").contains("pr-open"));
    let draft = mount(vec![thread(json!({"pullRequests":[link(5, snapshot("open", true, "a", "main"), json!({}))]}))]);
    assert!(state_of(&draft, "PR #5 - Draft: Title of a").contains("pr-draft"));
    let merged = mount(vec![thread(json!({"pullRequests":[link(5, snapshot("merged", false, "a", "main"), json!({}))]}))]);
    assert!(state_of(&merged, "PR #5 - Merged: Title of a").contains("pr-merged"));
    let closed = mount(vec![thread(json!({"pullRequests":[link(5, snapshot("closed", false, "a", "main"), json!({}))]}))]);
    assert!(state_of(&closed, "PR #5 - Closed: Title of a").contains("pr-closed"));
    // A merge request is named as the host names it.
    let mut request = link(7, snapshot("open", false, "a", "main"), json!({"host":"gitlab.com"}));
    request["url"] = json!("https://gitlab.com/acme/web/-/merge_requests/7");
    let gitlab = mount(vec![thread(json!({"pullRequests":[request]}))]);
    assert!(control(&gitlab, "MR #7 - Open: Title of a").is_some());
    // Before the first sync a link has no snapshot: it reads as pending, and as open.
    let unsynced = mount(vec![thread(json!({"pullRequests":[link(5, Value::Null, json!({}))]}))]);
    assert!(control(&unsynced, "PR #5, status pending").is_some());
    // Layers of a stack are one badge that counts them, in the state of the whole.
    let stack = mount(vec![thread(json!({"pullRequests":[link(1, snapshot("merged", false, "a", "main"), json!({})), link(2, snapshot("open", false, "b", "a"), json!({}))]}))]);
    let text = rendered_text(&stack);
    assert!(control(&stack, "Stack of 2 pull requests, open").is_some() && text.contains('2'), "{text}");
    // Several unrelated links: the newest open one, and how many more there are.
    let several = mount(vec![thread(json!({"pullRequests":[link(1, snapshot("open", false, "a", "main"), json!({})), link(2, snapshot("closed", false, "z", "main"), json!({}))]}))]);
    assert!(control(&several, "PR #1 - Open: Title of a, and 1 more linked; overall open").is_some() && rendered_text(&several).contains("+2"));
    // A thread with nothing filed has no badge.
    assert!(control(&mount(vec![thread(json!({}))]), "PR #1*").is_none());
}

#[derive(Clone)]
struct Strip(t3_contracts::ThreadShell);
impl PartialEq for Strip {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
fn strip_harness(props: Strip) -> Element {
    let state = use_store(|| {
        let project = serde_json::from_value(json!({"id":"p","title":"P","workspaceRoot":"/p","defaultModelSelection":null,"scripts":[],"createdAt":"now","updatedAt":"now"})).unwrap();
        let mut model = UiModel::default();
        model.shell.snapshot = Some(t3_contracts::ShellSnapshot { schema_version: 2, snapshot_sequence: 1, projects: vec![project], threads: vec![props.0.clone()], archived_threads: vec![] });
        model
    });
    rsx! { crate::composer_panels::GoalStrip { state, transport: TransportHandle::default(), thread_id: "thread".to_owned(), can_operate: true } }
}

#[test]
fn a_native_goal_is_shown_above_the_composer_with_its_follow_ups_once_the_thread_is_idle() {
    let idle = |goal: Value| {
        let mut dom = VirtualDom::new_with_props(strip_harness, Strip(thread(json!({"status":"idle","activeRunId":null,"activityRunStatus":null,"goal":goal}))));
        dom.rebuild_in_place();
        dom
    };
    let paused = idle(json!({"objective":"Ship the release","status":"paused","tokensUsed":12_400,"tokenBudget":50_000}));
    let text = rendered_text(&paused);
    assert!(text.contains("Goal paused \u{b7} 12k / 50k tokens") && text.contains("Ship the release"), "{text}");
    assert!(control(&paused, "Resume goal").is_some() && control(&paused, "Clear goal").is_some());
    // An active goal on an idle thread is set, not pursued; there is nothing to resume.
    let active = idle(json!({"objective":"Ship the release","status":"active"}));
    assert!(rendered_text(&active).contains("Goal set") && control(&active, "Resume goal").is_none() && control(&active, "Clear goal").is_some());
    // While the thread works the goal is pursued and the follow-ups wait.
    let mut working = VirtualDom::new_with_props(strip_harness, Strip(thread(json!({"goal":{"objective":"Ship the release","status":"active"}}))));
    working.rebuild_in_place();
    assert!(rendered_text(&working).contains("Pursuing goal") && control(&working, "Clear goal").is_none());
    // No goal, no strip.
    let mut none = VirtualDom::new_with_props(strip_harness, Strip(thread(json!({}))));
    none.rebuild_in_place();
    assert!(!rendered_text(&none).contains("Goal"));
}

#[test]
fn a_row_says_what_the_original_says_of_a_thread_that_needs_you_or_finished_while_you_were_away() {
    let idle = json!({"status":"idle","activeRunId":null,"activityRunStatus":null,"activeProviderThreadId":null,"latestRunId":null,"modelSelection":{"instanceId":"codex","model":"m"}});
    let with = |extra: Value| {
        let mut base = idle.clone();
        for (key, value) in extra.as_object().unwrap() {
            base[key] = value.clone();
        }
        rendered_text(&mount(vec![thread(base)]))
    };
    let approval = with(json!({"latestRunId":"r","activeProviderThreadId":"pt","status":"waiting","activityRunStatus":"waiting","pendingRuntimeRequest":{"id":"q","kind":"command","createdAt":"2026-01-01T00:00:00Z"}}));
    assert!(approval.contains("Approval") && !approval.contains("Pending Approval"), "{approval}");
    let input = with(json!({"latestRunId":"r","activeProviderThreadId":"pt","status":"waiting","activityRunStatus":"waiting","pendingRuntimeRequest":{"id":"q","kind":"user_input","createdAt":"2026-01-01T00:00:00Z"}}));
    assert!(input.contains("Input") && !input.contains("Awaiting Input"), "{input}");
    // Finished after the last visit: Done. A visit after it, and nothing says so.
    let finished = json!({"latestRunId":"r","activeProviderThreadId":"pt","status":"completed","latestRunCompletedAt":"2026-01-01T02:00:00Z","branch":null});
    let mut unread = finished.clone();
    unread["lastVisitedAt"] = json!("2026-01-01T01:00:00Z");
    assert!(with(unread).contains("Done"));
    let mut read = finished;
    read["lastVisitedAt"] = json!("2026-01-01T02:30:00Z");
    assert!(!with(read).contains("Done"));
}
