//! Contexts against scripted browsers (apps/server/src/preview/ServerBrowserContexts.test.ts): which launches happen, for whom, and what closes.
use super::*;
use crate::{server_browser_double::{Double, DoubleLauncher}, test_support::HANG_BOUND};

struct Fixed;
#[async_trait]
impl BrowserExecutable for Fixed {
    async fn executable(&self) -> Result<PathBuf, PreviewBrowserError> {
        Ok(PathBuf::from("/fake/chrome-headless-shell"))
    }
}

struct Fixture {
    contexts: ServerBrowserContexts,
    launcher: Arc<DoubleLauncher>,
    scratch: tempfile::TempDir,
}

fn fixture(launcher: DoubleLauncher, no_sandbox: bool) -> Fixture {
    let scratch = tempfile::tempdir().unwrap();
    let launcher = Arc::new(launcher);
    let contexts = ServerBrowserContexts::new(ContextsOptions {
        profiles_dir: scratch.path().join("profiles"),
        downloads_dir: scratch.path().join("downloads"),
        executable: Arc::new(Fixed),
        launcher: launcher.clone(),
        no_sandbox,
        setup_command: "sudo t3 browser setup".into(),
        scratch_root: scratch.path().to_owned(),
    });
    Fixture { contexts, launcher, scratch }
}

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, future).await.expect("the awaited event happened")
}

#[tokio::test]
async fn one_browser_is_shared_by_concurrent_isolated_sessions_and_each_context_is_reused() {
    let fixture = fixture(DoubleLauncher::new(Double::new()), false);
    let contexts = &fixture.contexts;
    let (first, second, again) = within(async { tokio::join!(contexts.context_for("default", Some("a")), contexts.context_for("default", Some("b")), contexts.context_for("default", Some("a"))) }).await;
    let (first, second, again) = (first.unwrap(), second.unwrap(), again.unwrap());
    assert_eq!(fixture.launcher.launches().len(), 1);
    assert!(Arc::ptr_eq(&first, &again) && !Arc::ptr_eq(&first, &second));
    assert_ne!(first.context_id(), second.context_id());
    let incognito = within(contexts.context_for(INCOGNITO_BROWSER_PROFILE_ID, None)).await.unwrap();
    assert!(incognito.context_id().is_some() && Arc::ptr_eq(incognito.browser(), first.browser()));
    // The shared browser's profile is a throwaway directory under the scratch root.
    assert!(fixture.launcher.launches()[0].profile_directory.starts_with(fixture.scratch.path()));
}

#[tokio::test]
async fn human_profiles_keep_their_storage_apart_from_agents_and_each_other() {
    let fixture = fixture(DoubleLauncher::new(Double::new()), false);
    let contexts = &fixture.contexts;
    let work = within(contexts.context_for("work", None)).await.unwrap();
    let personal = within(contexts.context_for("personal", None)).await.unwrap();
    let agent = within(contexts.context_for("work", Some("agent-tab"))).await.unwrap();
    let launches = fixture.launcher.launches();
    assert_eq!(launches.len(), 3);
    let profiles = fixture.scratch.path().join("profiles");
    assert_eq!(launches[0].profile_directory, profiles.join("work"));
    assert_eq!(launches[1].profile_directory, profiles.join("personal"));
    assert!(launches[0].profile_directory.is_dir());
    assert!(work.context_id().is_none() && personal.context_id().is_none());
    assert!(agent.context_id().is_some() && !Arc::ptr_eq(agent.browser(), work.browser()));
    assert!(Arc::ptr_eq(&work, &within(contexts.context_for("work", None)).await.unwrap()));
}

#[tokio::test]
async fn clearing_a_profile_closes_its_context_before_deleting_its_storage() {
    let fixture = fixture(DoubleLauncher::new(Double::new()), false);
    let contexts = &fixture.contexts;
    let (seen, mut closed) = mpsc::unbounded_channel();
    contexts.on_context_close(move |context| {
        let _ = seen.send(context.id());
    });
    let work = within(contexts.context_for("work", None)).await.unwrap();
    let personal = within(contexts.context_for("personal", None)).await.unwrap();
    std::fs::write(fixture.launcher.launches()[0].profile_directory.join("Cookies"), "x").unwrap();
    within(contexts.clear_profile("work")).await.unwrap();
    assert!(!fixture.launcher.launches()[0].profile_directory.exists());
    assert!(fixture.launcher.launches()[1].profile_directory.exists());
    assert!(work.is_closed() && !personal.is_closed());
    // The browser of the cleared profile was ended, and the service heard of the context.
    assert_eq!(fixture.launcher.shutdowns(), 1);
    assert_eq!(within(closed.recv()).await, Some(work.id()));
    // The next tab of that profile gets a fresh context and a fresh directory.
    let reopened = within(contexts.context_for("work", None)).await.unwrap();
    assert!(!Arc::ptr_eq(&work, &reopened));
    assert_eq!(fixture.launcher.launches().len(), 3);
}

#[tokio::test]
async fn a_tab_asked_for_during_a_clear_opens_after_the_storage_is_gone() {
    let (launcher, gate) = DoubleLauncher::new(Double::new()).gated();
    let fixture = fixture(launcher, false);
    let contexts = fixture.contexts.clone();
    let work = within(contexts.context_for("work", None)).await.unwrap();
    let directory = fixture.launcher.launches()[0].profile_directory.clone();
    std::fs::write(directory.join("Cookies"), "x").unwrap();
    // The clear holds the profile's place from the moment it ends the old browser until the storage is deleted.
    let clearing = contexts.clone();
    let clear = tokio::spawn(async move { clearing.clear_profile("work").await });
    within(gate.entered.notified()).await;
    let requested = contexts.clone();
    let request = tokio::spawn(async move { requested.context_for("work", None).await.map(|context| context.id()) });
    gate.release.notify_one();
    within(clear).await.unwrap().unwrap();
    let reopened = within(request).await.unwrap().unwrap();
    assert_ne!(reopened, work.id());
    // The directory the new browser was launched in exists again, and holds nothing of the old one.
    assert!(directory.is_dir() && !directory.join("Cookies").exists());
    assert_eq!(fixture.launcher.launches().len(), 2);
}

#[tokio::test]
async fn only_an_explicit_opt_out_turns_the_sandbox_off() {
    for (no_sandbox, expected) in [(false, false), (true, true)] {
        let fixture = fixture(DoubleLauncher::new(Double::new()), no_sandbox);
        within(fixture.contexts.context_for("default", Some("tab"))).await.unwrap();
        assert_eq!(fixture.launcher.launches()[0].no_sandbox, expected);
    }
}

#[tokio::test]
async fn a_failed_startup_is_retried_on_a_later_request_with_the_sandbox_still_on() {
    // Chrome's abort names the sandbox: the failure is the host setup, and the next request launches again.
    let fixture = fixture(DoubleLauncher::new(Double::new()).failing(1, "FATAL: No usable sandbox! Update your kernel"), false);
    let failure = within(fixture.contexts.context_for("default", Some("tab"))).await.unwrap_err();
    assert_eq!(failure, LaunchFailure::Host(PreviewBrowserHostError::Sandbox { setup_command: "sudo t3 browser setup".into() }));
    within(fixture.contexts.context_for("default", Some("tab"))).await.unwrap();
    let launches = fixture.launcher.launches();
    assert_eq!(launches.len(), 2);
    assert!(launches.iter().all(|launch| !launch.no_sandbox));
}

#[tokio::test]
async fn a_failure_with_no_known_cause_is_the_browsers_own() {
    let fixture = fixture(DoubleLauncher::new(Double::new()).failing(1, "something else"), false);
    match within(fixture.contexts.context_for("default", Some("tab"))).await.unwrap_err() {
        LaunchFailure::Browser(error) => assert_eq!(error.output.as_deref(), Some("something else")),
        // On Linux the loader check may also name libraries; the stand-in executable has none to miss.
        LaunchFailure::Host(PreviewBrowserHostError::Libraries { .. }) => {}
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_closed_context_is_forgotten_without_closing_other_agents_or_relaunching_the_browser() {
    let fixture = fixture(DoubleLauncher::new(Double::new()), false);
    let contexts = &fixture.contexts;
    let (first, second) = (within(contexts.context_for("default", Some("a"))).await.unwrap(), within(contexts.context_for("default", Some("b"))).await.unwrap());
    within(first.close()).await;
    assert!(first.is_closed() && !second.is_closed());
    let reopened = within(contexts.context_for("default", Some("a"))).await.unwrap();
    assert!(!Arc::ptr_eq(&first, &reopened) && Arc::ptr_eq(&second, &within(contexts.context_for("default", Some("b"))).await.unwrap()));
    assert_eq!(fixture.launcher.launches().len(), 1);
    // The browser was told to dispose the one context.
    assert_eq!(fixture.launcher.browser(0).commands_named("Target.disposeBrowserContext").len(), 1);
}

#[tokio::test]
async fn closing_ends_every_browser_and_refuses_later_work() {
    let fixture = fixture(DoubleLauncher::new(Double::new()), false);
    let contexts = &fixture.contexts;
    let shared = within(contexts.context_for("default", Some("a"))).await.unwrap();
    let persistent = within(contexts.context_for("work", None)).await.unwrap();
    within(contexts.close()).await;
    assert!(shared.is_closed() && persistent.is_closed());
    assert_eq!(fixture.launcher.shutdowns(), 2);
    assert_eq!(within(contexts.context_for("default", Some("b"))).await.unwrap_err(), LaunchFailure::Closed);
    assert!(contexts.scratch_page().await.is_err());
    within(contexts.close()).await;
}

#[tokio::test]
async fn the_browsers_own_events_reach_the_service_with_the_browser_they_came_from() {
    let fixture = fixture(DoubleLauncher::new(Double::new()), false);
    let mut events = fixture.contexts.take_events().expect("the stream is taken once");
    assert!(fixture.contexts.take_events().is_none());
    let context = within(fixture.contexts.context_for("default", Some("a"))).await.unwrap();
    fixture.launcher.browser(0).push(json!({"method": "Target.targetCreated", "params": {"targetInfo": {"targetId": "T-9", "type": "page"}}}));
    // A page's own events stay with the page.
    fixture.launcher.browser(0).push(json!({"method": "Page.loadEventFired", "sessionId": "S-1", "params": {}}));
    fixture.launcher.browser(0).push(json!({"method": "Browser.downloadProgress", "params": {"guid": "g", "state": "completed"}}));
    let created = within(events.recv()).await.unwrap();
    assert_eq!((created.browser, created.event.method.as_str()), (context.browser().id(), "Target.targetCreated"));
    assert_eq!(within(events.recv()).await.unwrap().event.method, "Browser.downloadProgress");
}
