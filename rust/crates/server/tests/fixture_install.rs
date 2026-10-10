//! What `--fixture` installs, in a process of its own (the process's host is set once).
use t3_server::{
    external_launcher::{Probe, available_editors},
    fixture,
    source_control_host::Host,
};

#[test]
fn the_fixture_sends_github_to_the_origin_and_offers_no_editors() {
    fixture::install(Some("http://127.0.0.1:47851")).unwrap();
    let resolve = Host::process_default().shared.github_api.clone().expect("the fixture host redirects GitHub");
    assert_eq!(resolve("github.example"), ("http://127.0.0.1:47851/rest/github.example".to_owned(), "http://127.0.0.1:47851/graphql/github.example".to_owned()));
    let probe = Probe::host();
    assert!(probe.applications.is_empty() && probe.linux_dirs.is_empty() && probe.env.is_empty());
    assert!(available_editors(&probe).is_empty());
    // The host is set once: a second install is refused, not silently ignored.
    assert!(fixture::install(Some("http://127.0.0.1:47852")).is_err());
}
