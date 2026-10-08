use crate::scroll_state::*;
fn input(
    owner: &str,
    hydrated: bool,
    run: Option<&str>,
    message: Option<&str>,
    queued: bool,
) -> RunInput {
    RunInput {
        observation: RunObservation {
            thread_key: Some(owner.into()),
            hydrated,
            run_id: run.map(str::to_owned),
        },
        queued,
        message_id: message.map(str::to_owned),
    }
}
#[test]
fn open_hydrate_new_turn_and_queued_input_follow_source_baseline() {
    let mut scroll = ScrollState::new("a:thread".into(), None);
    assert_eq!(
        scroll.initial_position(None, false).unwrap().target,
        ScrollTarget::End
    );
    assert!(
        scroll
            .observe(&input("a:thread", false, None, None, false))
            .is_none()
    );
    assert!(
        scroll
            .observe(&input(
                "a:thread",
                true,
                Some("old"),
                Some("old-message"),
                false
            ))
            .is_none()
    );
    assert!(
        scroll
            .observe(&input("a:thread", true, Some("new"), None, false))
            .is_none()
    );
    assert_eq!(
        scroll
            .observe(&input(
                "a:thread",
                true,
                Some("new"),
                Some("new-message"),
                false
            ))
            .unwrap()
            .target,
        ScrollTarget::AuthoredMessage("new-message".into())
    );
    assert!(
        scroll
            .observe(&input(
                "a:thread",
                true,
                Some("new"),
                Some("new-message"),
                false
            ))
            .is_none()
    );
    scroll.anchor_settled("new-message");
    assert!(
        scroll
            .observe(&input(
                "a:thread",
                true,
                Some("queued"),
                Some("queue-message"),
                true
            ))
            .is_none()
    );
    assert!(scroll.own_send("queued-own".into(), true).is_none());
    assert_eq!(scroll.mode, ScrollMode::FollowingEnd);
    assert!(
        scroll
            .observe(&input(
                "a:thread",
                true,
                Some("queued"),
                Some("queue-message"),
                false
            ))
            .is_some()
    );
}
#[test]
fn reading_cancels_delayed_restore_and_stream_follow_until_explicit_own_send() {
    let reading = RememberedPosition {
        anchor: RowAnchor {
            row_id: "old-output".into(),
            offset_within_row: 75.0,
            scroll_offset: 350.0,
        },
        at_end: false,
        disclosures: None,
    };
    let mut scroll = ScrollState::new("a:thread".into(), Some(&reading));
    let restore = scroll.initial_position(Some(&reading), false).unwrap();
    assert!(scroll.owns(&restore));
    scroll.observe(&input(
        "a:thread",
        true,
        Some("old"),
        Some("old-message"),
        false,
    ));
    assert!(scroll.wheel(-1.0, false, true, true));
    assert!(!scroll.owns(&restore));
    assert!(
        scroll
            .observe(&input(
                "a:thread",
                true,
                Some("new"),
                Some("new-message"),
                false
            ))
            .is_none()
    );
    let history = scroll.preserve_history(reading.anchor.clone());
    scroll.manual_navigation();
    assert!(!scroll.owns(&history));
    let sent = scroll.own_send("my-message".into(), false).unwrap();
    assert!(scroll.owns(&sent));
    assert_eq!(
        scroll.mode,
        ScrollMode::AnchoringNewTurn("my-message".into())
    );
    scroll.manual_navigation();
    scroll.anchor_settled("my-message");
    assert_eq!(
        scroll.mode,
        ScrollMode::FreeScrolling,
        "settling an old anchor cannot override manual reading"
    );
    let latest = scroll.follow_end();
    assert!(scroll.owns(&latest));
    assert!(!scroll.owns(&sent));
}
#[test]
fn gesture_near_end_nested_scroll_and_underflow_never_silently_break_follow() {
    let mut scroll = ScrollState::new("a:thread".into(), None);
    assert!(!scroll.wheel(-1.0, false, true, false));
    assert!(!scroll.wheel(-1.0, false, false, true));
    assert!(!scroll.wheel(-1.0, true, true, true));
    assert!(!scroll.wheel(1.0, false, true, true));
    assert!(!scroll.touch_move(false));
    assert!(!scroll.pointer_down(false, true, false));
    assert!(!scroll.pointer_down(true, false, false));
    assert_eq!(scroll.mode, ScrollMode::FollowingEnd);
    assert!(scroll.pointer_down(true, true, false));
    assert_eq!(scroll.mode, ScrollMode::FreeScrolling);
    scroll.at_end_changed(true);
    assert_eq!(scroll.mode, ScrollMode::FollowingEnd);
    assert!(scroll.pointer_down(false, true, true));
    scroll.follow_end();
    assert!(scroll.touch_move(true));
}
#[test]
fn same_local_ids_in_another_destination_reject_old_async_position_requests() {
    let mut a = ScrollState::new("a:same-thread".into(), None);
    let request = a.own_send("same-message".into(), false).unwrap();
    let b = ScrollState::new("b:same-thread".into(), None);
    assert!(!b.owns(&request));
    assert!(
        a.initial_position(None, true).is_none(),
        "citation owns positioning"
    );
    let newer = a.follow_end();
    assert!(!a.owns(&request));
    assert!(a.owns(&newer));
}
