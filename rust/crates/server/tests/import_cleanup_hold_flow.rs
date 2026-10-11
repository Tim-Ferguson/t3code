//! The import carries the original's cleanup rules over switched off (`cleanup_hold.rs`, a Rust-port extension): the first start
//! of a new implementation must not delete real worktrees on the strength of settings the user made for another server.
use flate2::read::GzDecoder;
use serde_json::json;
use std::{
    io::Read,
    path::{Path, PathBuf},
};
use t3_server::state_import::{Options, Outcome, import};

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The `full` fixture as a T3 home, its database unpacked and its settings replaced by `settings`.
fn home(base: &Path, settings: &str) -> PathBuf {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/state-import/full/userdata");
    let userdata = base.join("home/userdata");
    copy_tree(&fixtures, &userdata);
    for file in std::fs::read_dir(&userdata).unwrap().flatten().map(|entry| entry.path()).collect::<Vec<_>>() {
        if file.extension().is_some_and(|extension| extension == "gz") {
            let mut bytes = Vec::new();
            GzDecoder::new(std::fs::File::open(&file).unwrap()).read_to_end(&mut bytes).unwrap();
            std::fs::write(file.with_extension(""), bytes).unwrap();
            std::fs::remove_file(&file).unwrap();
        }
    }
    if !userdata.join("environment-id").exists() {
        std::fs::write(userdata.join("environment-id"), "environment-1\n").unwrap();
    }
    std::fs::write(userdata.join("settings.json"), settings).unwrap();
    base.join("home")
}

fn imported(base: &Path, settings: &str, in_place: bool) -> (PathBuf, t3_server::state_import::Report) {
    let from = home(base, settings);
    let state = if in_place { from.join("userdata") } else { base.join("state") };
    std::fs::create_dir_all(&state).unwrap();
    match import(Options::new(from, state.clone()), &mut |_| {}).unwrap() {
        Outcome::Imported(report) => (state, *report),
        other => panic!("{other:?}"),
    }
}

const ENABLED: &str = r#"{/* the original's own file */"storageCleanup":{"worktreeAfterDays":14,"worktreeOnMerge":false,"worktreeOnDelete":true,"worktreeUnchanged":false,"browserArtifactsAfterDays":null,"logsAfterDays":30},}"#;

#[test]
fn enabled_rules_are_held_when_settings_are_copied_or_used_in_place_and_the_report_says_so() {
    for in_place in [false, true] {
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-hold-").tempdir().unwrap();
        let (state, report) = imported(directory.path(), ENABLED, in_place);
        assert!(state.join("rust-cleanup-hold.json").exists(), "in place: {in_place}");
        let line = report.credentials_to_reenter.iter().find(|line| line.contains("cleanup rules")).unwrap_or_else(|| panic!("{:?}", report.credentials_to_reenter));
        assert!(line.contains("switched off") && line.contains("14") && line.contains("logs after days: 30"), "{line}");
        // The same line reaches the client's notice.
        let block = t3_server::import_status::status(&state).expect("a notice");
        assert!(block.reenter.iter().any(|text| text.contains("cleanup rules")), "{:?}", block.reenter);
        // The settings the user made are not rewritten.
        assert_eq!(std::fs::read_to_string(state.join("settings.json")).unwrap(), ENABLED);
    }
}

#[test]
fn settings_that_delete_nothing_are_not_held() {
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-hold-").tempdir().unwrap();
    let off = json!({"storageCleanup": {"worktreeAfterDays": null, "worktreeOnDelete": false}, "worktreeCleanup": {"mode": "off"}}).to_string();
    let (state, report) = imported(directory.path(), &off, false);
    assert!(!state.join("rust-cleanup-hold.json").exists());
    assert!(report.credentials_to_reenter.iter().all(|line| !line.contains("cleanup rules")));
}
