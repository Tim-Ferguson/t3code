use serde_json::{Value, json};
use t3_client::{terminal_output as output, terminal_session as session};

#[test]
fn original_output_cursor_and_session_reducers() {
    for (index, line) in include_str!("fixtures/terminal-client.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(line).unwrap();
        let actual = match row["kind"].as_str().unwrap() {
            "output" => {
                let mut state = output::OutputState {
                    generation: 7,
                    ..Default::default()
                };
                let mut cursor = output::INITIAL_CURSOR;
                let budget = row["budget"].as_i64().unwrap();
                row["operations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|operation| {
                        let data = operation["data"].as_str().unwrap();
                        state = if operation["type"] == "reset" {
                            output::reset(&state, data, budget)
                        } else {
                            output::append(&state, data, budget)
                        };
                        let update = output::read(&state, cursor);
                        cursor = update.cursor();
                        json!({"state":state,"update":update,"text":output::text(&state)})
                    })
                    .collect::<Vec<_>>()
            }
            "read" => {
                let state = serde_json::from_value(row["state"].clone()).unwrap();
                row["cursors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|cursor| {
                        serde_json::to_value(output::read(
                            &state,
                            serde_json::from_value(cursor.clone()).unwrap(),
                        ))
                        .unwrap()
                    })
                    .collect()
            }
            "session" => {
                let mut buffer = session::BufferState::seed(9);
                row["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|event| {
                        buffer.apply_with_budget(
                            &serde_json::from_value(event.clone()).unwrap(),
                            row["budget"].as_i64().unwrap(),
                        );
                        serde_json::to_value(&buffer).unwrap()
                    })
                    .collect()
            }
            "combine" => {
                let summary: Option<t3_contracts::TerminalSummary> =
                    serde_json::from_value(row["summary"].clone()).unwrap();
                let buffer = serde_json::from_value(row["buffer"].clone()).unwrap();
                let actual =
                    serde_json::to_value(session::combine(summary.as_ref(), &buffer)).unwrap();
                assert_eq!(actual, row["expected"], "witness {index}");
                continue;
            }
            "metadata" => {
                let mut metadata = vec![];
                row["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|event| {
                        session::apply_metadata(
                            &mut metadata,
                            &serde_json::from_value(event.clone()).unwrap(),
                        );
                        serde_json::to_value(&metadata).unwrap()
                    })
                    .collect()
            }
            kind => panic!("unknown {kind}"),
        };
        assert_eq!(json!(actual), row["expected"], "witness {index}");
    }
}

#[test]
fn live_append_shares_retained_chunk_and_reinstalled_stream_resets_old_cursor() {
    let state = output::reset(
        &output::OutputState::default(),
        &"x".repeat(512 * 1024),
        512 * 1024,
    );
    let state = output::append(&state, "new", 512 * 1024);
    let next = output::append(&state, "next", 512 * 1024);
    assert!(std::sync::Arc::ptr_eq(
        &state.chunks[1].data,
        &next.chunks[1].data
    ));
    let cursor = output::read(&next, output::INITIAL_CURSOR).cursor();
    let reinstalled = output::OutputState {
        generation: 1,
        ..next
    };
    assert!(matches!(
        output::read(&reinstalled, cursor),
        output::OutputUpdate::Reset { .. }
    ));
}
