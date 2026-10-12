//! A project's repository identity: the resolver against what the original's resolver answers for the same git output
//! (`repository-identity.jsonl`, written by `rust/tools/generate_repository_identity_fixtures.mjs`), its caches, and the shell
//! that carries the result (an authoritative snapshot never held up for git, then a marked frame once a root resolves) over a
//! real socket with a real repository.
mod rpc_support;
use futures_util::future::BoxFuture;
use rpc_support::*;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use t3_server::{
    project_enrichment::ProjectEnrichment,
    repository_identity::{Probe, ProbeOutput, RepositoryIdentityResolver, identity_from_remotes, top_level_of},
    test_support::HANG_BOUND,
};
use t3_vcs::ManualClock;

#[test]
fn the_resolver_reads_identities_as_the_original_does() {
    let mut checked = 0;
    for line in include_str!("fixtures/repository-identity.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let output = |part: &Value| ProbeOutput { code: part["code"].as_i64().unwrap() as i32, stdout: part["stdout"].as_str().unwrap().to_owned() };
        let identity = top_level_of(&output(&row["topLevel"])).and_then(|root| identity_from_remotes(&output(&row["remotes"]), &root));
        let identity = identity.map_or(Value::Null, |identity| serde_json::to_value(identity).unwrap());
        assert_eq!(identity, row["identity"], "{}", row["name"]);
        checked += 1;
    }
    assert_eq!(checked, 35);
}

/// A probe that answers a fixed repository and counts how often it is asked.
fn counting_probe(remotes: &'static str) -> (Probe, Arc<AtomicUsize>) {
    let spawns = Arc::new(AtomicUsize::new(0));
    let counted = spawns.clone();
    let probe: Probe = Arc::new(move |args: Vec<String>| -> BoxFuture<'static, Option<ProbeOutput>> {
        counted.fetch_add(1, Ordering::SeqCst);
        let stdout = if args.contains(&"rev-parse".to_owned()) { "/work/repo\n".to_owned() } else { remotes.to_owned() };
        Box::pin(async move { Some(ProbeOutput { code: 0, stdout }) })
    });
    (probe, spawns)
}

#[tokio::test]
async fn an_identity_is_held_for_fifteen_minutes_and_an_absent_one_for_one() {
    let clock = Arc::new(ManualClock::default());
    let (probe, spawns) = counting_probe("origin\thttps://github.com/acme/widgets.git (fetch)\n");
    let resolver = RepositoryIdentityResolver::with_clock(probe, None, clock.clone());
    for _ in 0..1000 {
        assert!(resolver.resolve("/work/repo/sub", false).await.is_some());
    }
    // One probe for the directory's root, one for the root's remotes, however often it is asked.
    assert_eq!(spawns.load(Ordering::SeqCst), 2);
    clock.advance(Duration::from_secs(14 * 60));
    resolver.resolve("/work/repo/sub", false).await;
    assert_eq!(spawns.load(Ordering::SeqCst), 2);
    clock.advance(Duration::from_secs(2 * 60));
    resolver.resolve("/work/repo/sub", false).await;
    assert_eq!(spawns.load(Ordering::SeqCst), 4, "past fifteen minutes both are asked again");
    // A caller that has just changed the remotes asks for a refresh.
    resolver.resolve("/work/repo/sub", true).await;
    assert_eq!(spawns.load(Ordering::SeqCst), 6);

    let (probe, spawns) = counting_probe("");
    let resolver = RepositoryIdentityResolver::with_clock(probe, None, clock.clone());
    assert!(resolver.resolve("/work/repo", false).await.is_none());
    assert!(resolver.resolve("/work/repo", false).await.is_none());
    assert_eq!(spawns.load(Ordering::SeqCst), 2, "no remote is remembered too");
    clock.advance(Duration::from_secs(61));
    resolver.resolve("/work/repo", false).await;
    // The repository's root is held as long as an identity is, so only its remotes are asked again.
    assert_eq!(spawns.load(Ordering::SeqCst), 3, "but only for a minute: a folder can gain a remote");
}

#[tokio::test]
async fn shell_builds_ask_git_nothing_and_a_root_is_resolved_once_in_the_background() {
    let (probe, spawns) = counting_probe("origin\thttps://github.com/acme/widgets.git (fetch)\n");
    let enrichment = ProjectEnrichment::start(RepositoryIdentityResolver::new(probe, None), Arc::new(t3_vcs::SystemClock));
    let mut changes = enrichment.subscribe();
    let mut projects: Vec<Value> = (0..500).map(|index| json!({"id":format!("p{index}"),"workspaceRoot":format!("/work/root-{}", index % 50)})).collect();
    // Cold: every project is null and nothing waited on git.
    let started = std::time::Instant::now();
    let resolved = t3_server::shell_enrichment::enrich_projects(&enrichment, &mut projects);
    let cold = started.elapsed();
    assert!(resolved.is_empty());
    assert!(projects.iter().all(|project| project["repositoryIdentity"].is_null()));
    // Each of the fifty roots finishes once, however many projects share it.
    let mut finished = std::collections::HashSet::new();
    while finished.len() < 50 {
        let change = tokio::time::timeout(HANG_BOUND, changes.recv()).await.expect("roots resolve").unwrap();
        assert!(finished.insert(change.workspace_root), "a root resolved twice");
    }
    // Each directory is asked for its repository root; every one is in the same repository, whose remotes are asked once.
    assert_eq!(spawns.load(Ordering::SeqCst), 51);
    let started = std::time::Instant::now();
    let mut warm_resolved = Vec::new();
    for _ in 0..100 {
        warm_resolved = t3_server::shell_enrichment::enrich_projects(&enrichment, &mut projects);
    }
    let warm = started.elapsed() / 100;
    assert_eq!(warm_resolved.len(), 500);
    assert!(projects.iter().all(|project| project["repositoryIdentity"]["canonicalKey"] == "github.com/acme/widgets"));
    assert_eq!(spawns.load(Ordering::SeqCst), 51, "a warm shell build spawns no git");
    eprintln!("repository identity: building the identity of 500 projects over 50 roots takes {cold:?} cold (queueing) and {warm:?} warm (no process, no await)");
    // A moved project is looked at again.
    enrichment.invalidate(["/work/root-0"]);
    assert!(!enrichment.peek("/work/root-0").resolved);
}

/// The shell stream: the snapshot a client gets is never held up for git, and the identity follows as a marked frame.
#[test]
fn the_shell_stream_delivers_a_resolved_identity_after_the_snapshot() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(Box::pin(async {
        let h = harness().await;
        let repository = h.base.join("widgets");
        std::fs::create_dir(&repository).unwrap();
        for args in [&["init", "--quiet"][..], &["remote", "add", "origin", "https://github.com/Acme/Widgets.git"], &["remote", "add", "upstream", "git@github.com:Parent/Widgets.git"]] {
            assert!(std::process::Command::new("git").args(args).current_dir(&repository).status().unwrap().success());
        }
        let plain = h.base.join("plain");
        std::fs::create_dir(&plain).unwrap();
        let mut socket = h.connect(&h.operator).await;
        // Made in the store: `projects.mutate` hands back the project with what is known of its root, which is a look at the root, and
        // this flow is about the first look being the snapshot's.
        for (id, root) in [("widgets", &repository), ("plain", &plain)] {
            t3_server::project::ProjectService::new(h.store.clone()).mutate(json!({"type":"project.create","commandId":format!("create-{id}"),"projectId":id,"title":id,"workspaceRoot":root}), chrono::Utc::now()).unwrap();
        }
        // The first look at a root queues it; the snapshot says nothing yet.
        send(&mut socket, json!({"_tag":"Request","id":2,"tag":"orchestration.subscribeShell","payload":{"requestCompletionMarker":true},"headers":[]})).await;
        let mut frames = Vec::new();
        let mut roots_seen = std::collections::HashSet::new();
        while roots_seen.len() < 2 {
            let message = within(next(&mut socket)).await;
            if message["_tag"] == "Chunk" && message["requestId"] == 2 {
                let item = message["values"][0].clone();
                send(&mut socket, json!({"_tag":"Ack","requestId":2})).await;
                for root in item["resolvedRepositoryIdentityRoots"].as_array().into_iter().flatten() {
                    roots_seen.insert(root.as_str().unwrap().to_owned());
                }
                frames.push(item);
            }
        }
        let first = &frames[0];
        assert_eq!(first["kind"], "snapshot");
        assert!(first.get("resolvedRepositoryIdentityRoots").is_none(), "the snapshot is unmarked");
        let widgets_first = first["snapshot"]["projects"].as_array().unwrap().iter().find(|project| project["id"] == "widgets").unwrap();
        assert!(widgets_first["repositoryIdentity"].is_null(), "cold: not waited for");
        // Marked frames: projects alone, no threads, naming the roots that resolved.
        let marked: Vec<&Value> = frames.iter().filter(|frame| frame.get("resolvedRepositoryIdentityRoots").is_some()).collect();
        assert!(!marked.is_empty());
        for frame in &marked {
            assert_eq!(frame["snapshot"]["threads"], json!([]));
            assert_eq!(frame["snapshot"]["archivedThreads"], json!([]));
        }
        let widgets = marked.iter().flat_map(|frame| frame["snapshot"]["projects"].as_array().unwrap()).find(|project| project["id"] == "widgets").expect("the repository's project is in a marked frame");
        let canonical = std::fs::canonicalize(&repository).unwrap();
        assert_eq!(widgets["repositoryIdentity"]["canonicalKey"], "github.com/parent/widgets");
        assert_eq!(widgets["repositoryIdentity"]["locator"]["remoteName"], "upstream");
        assert_eq!(widgets["repositoryIdentity"]["origin"]["canonicalKey"], "github.com/acme/widgets");
        assert_eq!(widgets["repositoryIdentity"]["rootPath"], canonical.to_str().unwrap());
        let plain_marked = marked.iter().flat_map(|frame| frame["snapshot"]["projects"].as_array().unwrap()).find(|project| project["id"] == "plain").expect("a folder with no repository resolves to none");
        assert!(plain_marked["repositoryIdentity"].is_null());

        // The remote goes; the answer stays, from the cache: no git runs for a shell.
        assert!(std::process::Command::new("git").args(["remote", "remove", "upstream"]).current_dir(&repository).status().unwrap().success());
        let response = reqwest::Client::builder().no_proxy().build().unwrap().get(format!("http://{}/api/orchestration/shell", h.address)).bearer_auth(&h.operator).header("x-t3-orchestration-protocol", "2").send().await.unwrap();
        let shell: Value = response.json().await.unwrap();
        let held = shell["projects"].as_array().unwrap().iter().find(|project| project["id"] == "widgets").unwrap();
        assert_eq!(held["repositoryIdentity"]["canonicalKey"], "github.com/parent/widgets");
        h.finish().await;
    }));
}

async fn within<T>(wait: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, wait).await.expect("nothing arrived before the hang bound")
}
