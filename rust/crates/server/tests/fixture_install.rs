//! What `--fixture` installs, in a process of its own (the process's host is set once).
use t3_server::{
    external_launcher::{Probe, available_editors},
    fixture,
    source_control_host::Host,
};

#[test]
fn the_fixture_sends_github_to_the_origin_and_offers_no_editors() {
    fixture::install(Some("http://127.0.0.1:47851"), None).unwrap();
    let resolve = Host::process_default().shared.github_api.clone().expect("the fixture host redirects GitHub");
    assert_eq!(resolve("github.example"), ("http://127.0.0.1:47851/rest/github.example".to_owned(), "http://127.0.0.1:47851/graphql/github.example".to_owned()));
    let probe = Probe::host();
    assert!(probe.applications.is_empty() && probe.linux_dirs.is_empty() && probe.env.is_empty());
    assert!(available_editors(&probe).is_empty());
    // The host is set once: a second install is refused, not silently ignored.
    assert!(fixture::install(Some("http://127.0.0.1:47852"), None).is_err());
}

/// The catalog reads the registry index the fixture names and never fetches. The index is the original's shape; the host's target
/// is among those it lists, and the archive URL is never visited (a search downloads nothing).
#[tokio::test]
async fn the_registry_catalog_reads_the_fixture_file() {
    let directory = tempfile::tempdir().unwrap();
    let target = |name: &str| (name.to_owned(), serde_json::json!({"archive":"https://fixture.invalid/agent.tar.gz","cmd":"bin/agent"}));
    let targets: serde_json::Map<_, _> = ["darwin-aarch64", "darwin-x86_64", "linux-aarch64", "linux-x86_64", "windows-aarch64", "windows-x86_64"].iter().map(|name| target(name)).collect();
    let index = serde_json::json!({"version":"1","agents":[{"id":"offline-agent","name":"Offline Fixture Agent","version":"1.0.0","description":"Served from a file","distribution":{"binary":targets}}]});
    let file = directory.path().join("registry.json");
    std::fs::write(&file, index.to_string()).unwrap();
    assert!(fixture::install(None, Some(&directory.path().join("missing.json"))).is_err(), "a file that is not there is refused");
    assert!(fixture::acp_registry_file().is_none());
    fixture::install(None, Some(&file)).unwrap();
    assert_eq!(fixture::acp_registry_file(), Some(file.as_path()));
    let catalog = t3_server::acp_registry_support::Catalog::new(directory.path().join("caches"), directory.path().join("tools")).unwrap();
    let found = catalog.search(&serde_json::from_value(serde_json::json!({"query":""})).unwrap()).await.unwrap();
    assert_eq!(serde_json::to_value(found).unwrap()["agents"][0]["name"], "Offline Fixture Agent");
}
