//! Work-log grouping and turn folds replayed against what the original's row derivation made for
//! the same threads (`rust/tools/generate_client_timeline_rows_fixtures.mjs`).
use serde_json::{Value, json};
use std::collections::BTreeSet;
use t3_client::timeline_rows::{self as rows, Row};

fn set(value: &Value) -> BTreeSet<String> {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
}
fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

/// The rows in the shape the generator writes the original's.
fn normalized(entries: &[rows::Entry], derived: &[Row]) -> Vec<Value> {
    let id = |index: &usize| entries[*index].id.clone();
    derived
        .iter()
        .map(|row| match row {
            Row::Entry { entry } => json!({"kind":"entry","id":entries[*entry].id}),
            Row::Single { entry, label, continues } => json!({"kind":"single","entry":entries[*entry].id,"label":label,"continues":continues}),
            Row::Toggle { id: row_id, group, entries: members, summary, kind, surface, has_failure, expanded, continues, .. } => {
                json!({"kind":"toggle","id":row_id,"group":group,"count":members.len(),"summary":summary,"summaryKind":kind.as_str(),"surface":surface,"hasFailure":has_failure,"expanded":expanded,"continues":continues})
            }
            Row::Live { id: row_id, group, entry, entries: members, label, expanded, active, thought, continues, .. } => {
                json!({"kind":"live","id":row_id,"group":group,"entry":entries[*entry].id,"label":label,"count":members.len(),"expanded":expanded,"active":active,"thought":thought.as_ref().map(id),"surface":null,"continues":continues})
            }
            Row::Members { group, entries: members } => json!({"kind":"members","group":group,"entries":members.iter().map(id).collect::<Vec<_>>()}),
            Row::Working { since } => json!({"kind":"working","since":since}),
            Row::Thinking { group, expanded, continues, .. } => json!({"kind":"thinking","group":group,"expanded":expanded,"continues":continues}),
            Row::TurnFold { run_id, label, expanded, .. } => json!({"kind":"turn-fold","run":run_id,"label":label,"expanded":expanded}),
            Row::AttemptFold { run_id, attempt_id, label, expanded, .. } => json!({"kind":"attempt-fold","run":run_id,"attempt":attempt_id,"label":label,"expanded":expanded}),
            Row::Compaction { entry, label, active } => json!({"kind":"compaction","id":entries[*entry].id,"label":label,"active":active}),
            Row::Subagents { entries: members } => json!({"kind":"subagents","ids":members.iter().map(id).collect::<Vec<_>>()}),
            Row::Created { entry } => json!({"kind":"entry","id":format!("summary:{}", entries[*entry].id)}),
            Row::Footer { message } => json!({"kind":"footer","message":entries[*message].id}),
            Row::Item { .. } => panic!("left-out rows are placed by the caller"),
        })
        .collect()
}

#[test]
fn threads_group_fold_and_go_live_as_the_original_does() {
    let mut checked = 0;
    for line in include_str!("fixtures/timeline-rows.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        if row["type"] != "thread" {
            continue;
        }
        let input = &row["input"];
        let items = input["items"].as_array().unwrap();
        let entries = rows::entries(items, input["attempts"].as_array().unwrap(), input["nodes"].as_array().unwrap());
        let latest = input["latestRun"].as_object().map(|run| rows::LatestRun { id: run["runId"].as_str().unwrap().into(), status: run["status"].as_str().unwrap().into(), started_at: text(&run["startedAt"]), completed_at: text(&run["completedAt"]) });
        let (expanded_runs, expanded_attempts, expanded_groups) = (set(&input["expandedRuns"]), set(&input["expandedAttempts"]), set(&input["expandedGroups"]));
        let derived = rows::derive(&rows::Input {
            entries: &entries,
            latest_run: latest.as_ref(),
            running_run_id: input["runningRunId"].as_str(),
            expanded_runs: &expanded_runs,
            expanded_attempts: &expanded_attempts,
            expanded_groups: &expanded_groups,
            is_working: input["isWorking"] == true,
            runless_work_active: input["runlessWorkActive"] == true,
            active_turn_started_at: input["activeTurnStartedAt"].as_str(),
        });
        let expected_ids: Vec<&str> = row["entries"].as_array().unwrap().iter().map(|entry| entry["id"].as_str().unwrap()).collect();
        let ids: Vec<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, expected_ids, "{}", row["name"]);
        let got: Vec<Value> = normalized(&entries, &derived);
        let want = row["rows"].as_array().unwrap();
        for (position, (got, want)) in got.iter().zip(want).enumerate() {
            assert_eq!(got, want, "{} row {position}\n got {got:#?}\n want {want:#?}", row["name"]);
        }
        assert_eq!(got.len(), want.len(), "{}: row count\n got {got:#?}\nwant {want:#?}", row["name"]);
        checked += 1;
    }
    assert!(checked > 400, "{checked}");
}

#[test]
fn each_call_is_labelled_classified_and_judged_as_the_original_does() {
    let mut checked = 0;
    for line in include_str!("fixtures/timeline-rows.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        if row["type"] != "entry" {
            continue;
        }
        let work = rows::Work::from_item(&row["item"]);
        let context = row["item"].to_string();
        assert_eq!(rows::display_label(&work), row["label"].as_str().unwrap(), "label of {context}");
        assert_eq!(rows::live_label(&work, true), row["live"].as_str().unwrap(), "live label of {context}");
        assert_eq!(rows::live_label(&work, false), row["settled"].as_str().unwrap(), "past label of {context}");
        assert_eq!(rows::group_action(&work).as_str(), row["action"].as_str().unwrap(), "action of {context}");
        assert_eq!(rows::summarize_group(&[&work]).0, row["summary"].as_str().unwrap(), "summary of {context}");
        assert_eq!(work.display_failure(), row["failure"] == true, "failure of {context}");
        assert_eq!(rows::visible_in_group(&work, false), row["visible"] == true, "visibility of {context}");
        assert_eq!(rows::visible_in_group(&work, true), row["visibleInActive"] == true, "live visibility of {context}");
        checked += 1;
    }
    assert!(checked > 40, "{checked}");
}
