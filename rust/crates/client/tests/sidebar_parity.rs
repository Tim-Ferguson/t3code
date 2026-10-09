//! What a thread row says, replayed against the original (`rust/tools/generate_client_sidebar_fixtures.mjs`):
//! status and pill from the shell, unseen completions, the most urgent project indicator, working
//! durations, snooze presets in several zones and wake labels.
use chrono::{DateTime, Utc};
use serde_json::Value;
use t3_client::sidebar;
use t3_contracts::ThreadShell;

fn rows(kind: &str) -> Vec<Value> {
    let rows: Vec<Value> = include_str!("fixtures/sidebar.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).filter(|row: &Value| row["type"] == kind).collect();
    assert!(!rows.is_empty(), "no rows for {kind}");
    rows
}
fn time(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
}

#[test]
fn a_thread_row_reads_its_shell_as_the_original_does() {
    for row in rows("thread") {
        let shell: ThreadShell = serde_json::from_value(row["shell"].clone()).unwrap_or_else(|error| panic!("{error}: {}", row["shell"]));
        let context = row["shell"].to_string();
        assert_eq!(sidebar::status(&shell).as_str(), row["status"].as_str().unwrap(), "status of {context}");
        assert_eq!(sidebar::pill(&shell).map(|pill| pill.label), row["pill"].as_str(), "pill of {context}");
        assert_eq!(sidebar::has_unseen_completion(&shell), row["unseen"] == true, "unseen completion of {context}");
        assert_eq!(sidebar::runtime_status(&shell), row["runtime"].as_str(), "runtime of {context}");
    }
}

#[test]
fn the_most_urgent_pill_names_the_project_and_durations_and_wake_times_read_as_the_original_words_them() {
    for row in rows("highest") {
        let pills = row["labels"].as_array().unwrap().iter().map(|label| label.as_str().map(|label| match label {
            "Pending Approval" => sidebar::Pill { label: "Pending Approval", tone: sidebar::Tone::Amber, busy: false },
            "Awaiting Input" => sidebar::Pill { label: "Awaiting Input", tone: sidebar::Tone::Indigo, busy: false },
            "Working" => sidebar::Pill { label: "Working", tone: sidebar::Tone::Sky, busy: true },
            "Connecting" => sidebar::Pill { label: "Connecting", tone: sidebar::Tone::Sky, busy: true },
            "Waiting" => sidebar::Pill { label: "Waiting", tone: sidebar::Tone::Muted, busy: false },
            "Plan Ready" => sidebar::Pill { label: "Plan Ready", tone: sidebar::Tone::Violet, busy: false },
            _ => sidebar::Pill { label: "Completed", tone: sidebar::Tone::Emerald, busy: false },
        }));
        assert_eq!(sidebar::highest(pills).map(|pill| pill.label), row["output"].as_str(), "{}", row["labels"]);
    }
    for row in rows("working") {
        assert_eq!(sidebar::working_label(row["ms"].as_i64().unwrap()), row["output"].as_str().unwrap(), "{}", row["ms"]);
    }
    for row in rows("wake") {
        assert_eq!(sidebar::wake_label(row["until"].as_str().unwrap(), time(row["now"].as_str().unwrap())), row["output"].as_str().unwrap(), "{row}");
    }
}

#[test]
fn snooze_presets_land_on_the_same_local_times_in_every_zone() {
    for row in rows("presets") {
        let got: Vec<Value> = sidebar::snooze_presets(time(row["now"].as_str().unwrap()), row["offset"].as_i64().unwrap() as i32).into_iter().map(|preset| serde_json::json!({"id":preset.id,"label":preset.label,"until":preset.until})).collect();
        assert_eq!(Value::Array(got), row["output"], "{row}");
    }
}
