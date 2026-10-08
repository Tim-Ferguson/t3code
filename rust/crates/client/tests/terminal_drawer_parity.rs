use serde_json::Value;
use t3_client::{
    terminal_drawer::{self, ExitState},
    terminal_session::SessionStatus,
    terminal_ui::{Direction, Group, PaneState},
};
#[test]
fn original_drawer_geometry_and_exit_witnesses() {
    for line in include_str!("fixtures/terminal-drawer.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        match row["kind"].as_str().unwrap() {
            "height" => assert_eq!(
                terminal_drawer::clamp_height(
                    row["height"].as_f64().unwrap(),
                    row["viewport"].as_f64()
                ),
                row["expected"].as_f64().unwrap(),
                "{line}"
            ),
            "exit" => assert_eq!(
                terminal_drawer::should_handle_exit(
                    serde_json::from_value(row["current"].clone()).unwrap(),
                    serde_json::from_value(row["synchronized"].clone()).unwrap(),
                    row["handled"].as_bool().unwrap(),
                    row["version"].as_u64().unwrap()
                ),
                row["expected"].as_bool().unwrap(),
                "{line}"
            ),
            _ => panic!("unknown witness"),
        }
    }
}
#[test]
fn exits_are_once_per_running_lifecycle_and_never_seed_version_zero() {
    let mut exit = ExitState::default();
    assert_eq!(exit.observe(SessionStatus::Exited, 0), None);
    assert_eq!(exit.observe(SessionStatus::Running, 1), None);
    assert_eq!(
        exit.observe(SessionStatus::Exited, 2),
        Some("Process exited")
    );
    assert_eq!(exit.observe(SessionStatus::Exited, 3), None);
    assert_eq!(exit.observe(SessionStatus::Closed, 4), None);
    assert_eq!(exit.observe(SessionStatus::Running, 5), None);
    assert_eq!(
        exit.observe(SessionStatus::Closed, 6),
        Some("Terminal closed")
    );
}
#[test]
fn displayed_group_order_follows_global_terminal_order_without_rewriting_storage() {
    let pane = PaneState {
        terminal_ids: vec!["a".into(), "b".into(), "c".into()],
        terminal_groups: vec![
            Group {
                id: "last".into(),
                terminal_ids: vec!["c".into()],
                split_direction: None,
            },
            Group {
                id: "first".into(),
                terminal_ids: vec!["b".into(), "a".into()],
                split_direction: Some(Direction::Vertical),
            },
        ],
        ..Default::default()
    };
    let groups = terminal_drawer::displayed_groups(&pane);
    assert_eq!(groups[0].id, "first");
    assert_eq!(terminal_drawer::group_label(&groups[0]), "Stacked");
    assert_eq!(terminal_drawer::group_label(&groups[1]), "Single");
    assert_eq!(pane.terminal_groups[0].id, "last");
}
