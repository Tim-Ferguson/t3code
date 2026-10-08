use serde_json::{Value, json};
use t3_contracts::*;
#[test]
fn history_rows_keep_validation_atomic_and_drop_only_unknown_item_types() {
    let row = json!({"position":1,"visibility":"local","sourceThreadId":"source","sourceItemId":"item","item":{"type":"future_item","future":true}});
    let page = json!({"snapshotSequence":7,"items":[row.clone()],"nextCursor":" cursor ","hasMoreHistory":true});
    let decoded: ThreadHistoryPage = serde_json::from_value(page.clone()).unwrap();
    assert!(decoded.items.is_empty());
    assert_eq!(decoded.next_cursor.unwrap().as_str(), "cursor");
    for broken in [
        json!({"position":-1}),
        json!({"visibility":"future"}),
        json!({"item":{"type":"assistant_message"}}),
        json!({"item":{"type":42}}),
    ] {
        let mut bad = page.clone();
        for (key, value) in broken.as_object().unwrap() {
            bad["items"][0][key] = value.clone();
        }
        assert!(serde_json::from_value::<ThreadHistoryPage>(bad).is_err());
    }
    assert!(serde_json::from_value::<ThreadHistoryPage>(json!({"snapshotSequence":9007199254740992u64,"items":[],"nextCursor":null,"hasMoreHistory":false})).is_err());
    assert!(
        serde_json::from_value::<ThreadHistoryPage>(
            json!({"snapshotSequence":1,"items":[],"hasMoreHistory":false})
        )
        .is_err()
    );
}
#[test]
fn subscription_optional_keys_and_detail_nullability_are_distinct() {
    assert!(
        serde_json::from_value::<SubscribeThreadInput>(
            json!({"threadId":"thread","afterSequence":null})
        )
        .is_err()
    );
    let input: GetTurnItemInput =
        serde_json::from_value(json!({"threadId":"thread","itemId":"item","revision":null}))
            .unwrap();
    assert_eq!(
        serde_json::to_value(input).unwrap(),
        json!({"threadId":"thread","itemId":"item","revision":null})
    );
    assert!(serde_json::from_value::<GetTurnItemResult>(json!({})).is_err());
    assert!(serde_json::from_value::<GetTurnItemResult>(json!([null])).is_err());
    assert!(
        serde_json::from_value::<GetTurnItemResult>(json!({"item":null}))
            .unwrap()
            .item
            .is_none()
    );
}
#[test]
fn project_queries_preserve_content_whitespace_and_bound_utf16_units() {
    let entry: ProjectSearchEntriesInput =
        serde_json::from_value(json!({"cwd":" /workspace ","query":"  ","limit":200.0})).unwrap();
    assert_eq!(entry.query.0.as_str(), "");
    assert_eq!(entry.limit.0, 200);
    let contents = json!({"cwd":"/workspace","query":" foo ","limit":500,"caseSensitive":false,"wholeWord":false,"useRegex":true});
    let result: ProjectSearchContentsInput = serde_json::from_value(contents.clone()).unwrap();
    assert_eq!(result.query.0, " foo ");
    let mut too_long = contents;
    too_long["query"] = json!("😀".repeat(129));
    assert!(serde_json::from_value::<ProjectSearchContentsInput>(too_long).is_err());
    assert!(
        serde_json::from_value::<ProjectWriteFileInput>(
            json!({"cwd":"/workspace","relativePath":"../outside","contents":""})
        )
        .is_ok(),
        "path containment belongs to the service, not this wire schema"
    );
    assert!(
        serde_json::from_value::<FilesystemBrowseInput>(json!({"partialPath":"x".repeat(513)}))
            .is_err()
    );
    let range: ProjectContentMatchRange =
        serde_json::from_value(json!({"start":5,"end":1})).unwrap();
    assert_eq!(range.end.0, 1, "wire source does not impose ordering");
}
#[test]
fn terminal_input_bounds_count_utf16_and_keep_raw_output() {
    let input = json!({"threadId":"thread","terminalId":" term-1 ","data":"\r\n\u{001b}[31m😀"});
    let write: TerminalWriteInput = serde_json::from_value(input.clone()).unwrap();
    assert_eq!(write.terminal_id.0.as_str(), "term-1");
    assert_eq!(write.data.0, input["data"].as_str().unwrap());
    assert!(
        serde_json::from_value::<TerminalWriteInput>(
            json!({"threadId":"thread","terminalId":"term-1","data":"😀".repeat(32768)})
        )
        .is_ok()
    );
    assert!(
        serde_json::from_value::<TerminalWriteInput>(
            json!({"threadId":"thread","terminalId":"term-1","data":"😀".repeat(32769)})
        )
        .is_err()
    );
    let resize = json!({"threadId":"thread","terminalId":"term-1","cols":1000,"rows":500});
    assert!(serde_json::from_value::<TerminalResizeInput>(resize.clone()).is_ok());
    for (field, value) in [("cols", 0), ("rows", 501)] {
        let mut bad = resize.clone();
        bad[field] = json!(value);
        assert!(serde_json::from_value::<TerminalResizeInput>(bad).is_err());
    }
    let snapshot = json!({"type":"output","threadId":" thread ","terminalId":" terminal ","sequence":3,"data":"\u{001b}[0m"});
    let event: TerminalEvent = serde_json::from_value(snapshot.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        snapshot,
        "snapshot/event IDs are not trimmed on the wire"
    );
}
#[test]
fn terminal_environment_enforces_identifier_value_and_count_boundaries() {
    assert!(
        serde_json::from_value::<TerminalEnv>(json!({"_T3_SESSION":"","LC_ALL":"en_US.UTF-8"}))
            .is_ok()
    );
    for value in [
        json!({"INVALID-NAME":42}),
        json!({" NAME":"x"}),
        json!({"9NAME":false}),
        json!({"N".repeat(129):"x"}),
    ] {
        assert!(
            serde_json::from_value::<TerminalEnv>(value)
                .unwrap()
                .0
                .is_empty()
        );
    }
    assert!(serde_json::from_value::<TerminalEnv>(json!({"NAME":"😀".repeat(4097)})).is_err());
    let mut map = serde_json::Map::new();
    for i in 0..128 {
        map.insert(format!("KEY_{i}"), json!(""));
    }
    assert!(serde_json::from_value::<TerminalEnv>(Value::Object(map.clone())).is_ok());
    map.insert("OVERFLOW".into(), json!(""));
    assert!(serde_json::from_value::<TerminalEnv>(Value::Object(map)).is_err());
}
