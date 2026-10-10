//! `--fixture` is a testing flag: a server started without it is unchanged, and the origin variable alone is
//! an error before anything starts. The refusals are exercised on the real binary, which exits while it parses
//! its arguments, so no server is started and nothing is bound.
use std::process::Command;
use t3_server::{external_launcher::Probe, source_control_host::Host};

fn run(arguments: &[&str], origin: Option<&str>) -> std::process::Output {
    let home = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_t3-server"));
    command.env_clear().env("PATH", "/usr/bin:/bin").env("HOME", home.path()).arg("serve").arg("--state-dir").arg(home.path().join("state")).args(["--host", "127.0.0.1", "--port", "47859"]);
    command.args(arguments);
    if let Some(origin) = origin {
        command.env("T3CODE_FIXTURE_GITHUB_API_ORIGIN", origin);
    }
    let output = command.output().unwrap();
    // Refused at parse time: no state directory was even created.
    assert!(!home.path().join("state").exists());
    output
}

fn refusal(arguments: &[&str], origin: &str, mentions: &str) {
    let output = run(arguments, Some(origin));
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{origin}: {errors}");
    assert!(errors.contains(mentions), "{origin}: {errors}");
}

#[test]
fn the_origin_variable_without_the_flag_refuses_to_start() {
    refusal(&[], "http://127.0.0.1:47851", "--fixture");
    // Not even a valid loopback origin is read for its effect.
    refusal(&[], "https://api.github.com", "--fixture");
}

#[test]
fn the_flag_accepts_only_a_bare_loopback_origin() {
    for (origin, why) in [
        ("https://127.0.0.1:47851", "must start with http://"),
        ("http://api.github.com:80", "loopback"),
        ("http://127.0.0.1", "port"),
        ("http://127.0.0.1:47851/api", "path"),
        ("http://127.0.0.1:47851?x=1", "path"),
        ("http://user:secret@127.0.0.1:47851", "credentials"),
        ("http://127.0.0.1:47851@evil.example:80", "credentials"),
    ] {
        refusal(&["--fixture"], origin, why);
    }
}

#[test]
fn help_says_the_flag_is_for_testing_only() {
    let output = run(&["--help"], None);
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(help.contains("--fixture is for testing only"), "{help}");
}

/// Nothing in this process installs the fixture, so this is what a normal server has: the real GitHub endpoints and the
/// machine's applications.
#[test]
fn without_the_flag_the_host_and_editor_probe_are_unchanged() {
    assert!(Host::process_default().shared.github_api.is_none());
    assert_eq!(Probe::host().applications, vec![std::path::PathBuf::from("/Applications")]);
}
