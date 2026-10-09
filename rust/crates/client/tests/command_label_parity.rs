//! The shell-command reading replayed against the original's on a corpus of commands
//! (`rust/tools/generate_client_command_label_fixtures.mjs`).
use serde_json::Value;
use t3_client::command_label as label;

#[test]
fn programs_and_displayed_text_match_the_original_on_the_corpus() {
    let mut checked = 0;
    let mut named = 0;
    for line in include_str!("fixtures/command-label.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let command = row["command"].as_str().unwrap();
        assert_eq!(label::program_name(command).map(Value::from).unwrap_or(Value::Null), row["program"], "program of {command:?}");
        assert_eq!(label::display_text(command), row["display"].as_str().unwrap(), "display of {command:?}");
        checked += 1;
        named += usize::from(!row["program"].is_null());
    }
    assert!(checked > 2000, "{checked}");
    // The corpus must exercise both answers, or it proves little.
    assert!(named > 1000 && named < checked, "{named} of {checked}");
}

#[test]
fn a_plain_wrapper_is_removed_and_a_program_is_named() {
    assert_eq!(label::display_text("bash -lc 'npm test'"), "npm test");
    assert_eq!(label::display_text("bash -lc 'a' extra"), "bash -lc 'a' extra");
    assert_eq!(label::display_text("echo \u{1b}x"), "echo \u{241b}x");
    assert_eq!(label::program_name("cd src && /usr/bin/git status").as_deref(), Some("git"));
    assert_eq!(label::program_name("if true; then x; fi"), None);
}
