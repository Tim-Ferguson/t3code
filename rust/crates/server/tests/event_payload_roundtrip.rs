//! Every payload field of every event the original can write survives this server's codecs (`event_payload_roundtrip`).
//!
//! `fixtures/event-payloads.jsonl.gz` (`rust/tools/generate_event_payload_fixtures.mjs`) holds payloads the original's own
//! schemas generate for each event type, encoded as its event store writes them. A typed codec that does not model a field drops
//! it silently, and an event this server wrote would then differ from one the original wrote. Each payload is decoded and encoded
//! again; the JSON must be what the original's own decode and encode make of it (which fills a decoding default such as a provider
//! thread's `contextUsage: null`).
use flate2::read::GzDecoder;
use serde_json::Value;
use std::{collections::BTreeMap, io::Read};

/// `(aggregate, event type, payload as written, payload as the original reads it and writes it back)`.
fn payloads() -> Vec<(String, String, Value, Value)> {
    let mut text = String::new();
    GzDecoder::new(&include_bytes!("fixtures/event-payloads.jsonl.gz")[..]).read_to_string(&mut text).unwrap();
    text.lines()
        .map(|line| {
            let row: Value = serde_json::from_str(line).unwrap();
            (row["aggregate"].as_str().unwrap().to_owned(), row["type"].as_str().unwrap().to_owned(), row["payload"].clone(), row["canonical"].clone())
        })
        .collect()
}

/// Paths where `kept` is not `wrote`: a field dropped, one added, or a value changed. Arrays are walked by position.
fn differences(wrote: &Value, kept: &Value, at: &str, out: &mut Vec<String>) {
    match (wrote, kept) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys().filter(|key| !a.contains_key(*key))) {
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => differences(x, y, &format!("{at}.{key}"), out),
                    (Some(_), None) => out.push(format!("{at}.{key}: dropped")),
                    (None, Some(_)) => out.push(format!("{at}.{key}: added")),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                out.push(format!("{at}: {} items became {}", a.len(), b.len()));
            }
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                differences(x, y, &format!("{at}[]"), out);
                let _ = index;
            }
        }
        _ if wrote == kept => {}
        _ => out.push(format!("{at}: changed")),
    }
}

#[test]
fn every_field_of_every_thread_event_payload_survives_the_codecs() {
    let mut problems: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    for (aggregate, event_type, payload, canonical) in payloads() {
        if aggregate != "thread" {
            continue;
        }
        seen.insert(event_type.clone());
        let mut found = Vec::new();
        match t3_contracts::normalize_event_payload(&event_type, payload.clone()) {
            Ok(kept) => differences(&canonical, &kept, "", &mut found),
            Err(error) => found.push(format!(": does not decode ({error})")),
        }
        for path in found {
            let entry = problems.entry(format!("{event_type}{path}")).or_insert((0, payload.to_string().chars().take(160).collect()));
            entry.0 += 1;
        }
    }
    assert_eq!(seen.len(), 43, "{seen:?}");
    let shown: Vec<String> = problems.iter().map(|(path, (count, example))| format!("{count:>3}  {path}   e.g. {example}")).collect();
    assert!(shown.is_empty(), "{} paths are not carried:\n{}", shown.len(), shown.join("\n"));
}

/// The samples are not vacuous: the fields that were the cases for concern are in them, and the comparison sees a field that is lost.
#[test]
fn the_samples_reach_the_fields_a_codec_could_drop() {
    let mut present: BTreeMap<String, usize> = BTreeMap::new();
    for (aggregate, event_type, payload, _) in payloads() {
        if aggregate == "thread" {
            for key in payload.as_object().unwrap().keys() {
                *present.entry(format!("{event_type}.{key}")).or_default() += 1;
            }
        }
    }
    for path in [
        "provider-turn.updated.tokenUsage", "provider-turn.updated.turnTokenUsage", "run.updated.restartCancelledBackgroundWork", "run.updated.delegatedCompletion",
        "run.created.queueHeld", "provider-thread.updated.goal", "provider-thread.updated.contextUsage", "provider-thread.updated.pendingBackgroundTasks",
        "thread.created.pullRequests", "thread.created.limitRecovery", "thread.metadata-updated.rollbackFailure", "message.updated.notification", "message.updated.delegatedCompletion",
        "subagent.updated.completionDelivery", "turn-item.updated.progress", "run.background-work-cancelled.restartCancelledBackgroundWork",
    ] {
        assert!(present.get(path).copied().unwrap_or(0) >= 3, "{path} is in too few samples: {present:?}");
    }
    let wrote = serde_json::json!({"a":{"b":[{"c":1,"d":2}]}});
    let kept = serde_json::json!({"a":{"b":[{"c":1}]}});
    let mut found = Vec::new();
    differences(&wrote, &kept, "", &mut found);
    assert_eq!(found, [".a.b[].d: dropped"]);
}

/// The project events the original writes are kept whole by the project reducer, and the project row is the one the original keeps.
#[test]
fn every_field_of_every_project_event_payload_is_kept() {
    use t3_server::persistence::{Event, Store, StoredEvent};
    let store = Store::memory().unwrap();
    let mut sequence = 0;
    let mut reduce = |event_type: &str, payload: &Value| {
        sequence += 1;
        let event = Event {
            event_id: format!("event-{sequence}"), aggregate_kind: "project".into(), aggregate_id: payload["projectId"].as_str().unwrap().into(), occurred_at: "2026-01-01T00:00:00.000Z".into(),
            command_id: None, causation_event_id: None, correlation_id: None, event_type: event_type.into(), payload: payload.clone(), metadata: serde_json::json!({}),
        };
        store.transaction(|transaction| t3_server::project::reduce_project_event(transaction, &StoredEvent { sequence, event })).unwrap();
        store.projection("project", payload["projectId"].as_str().unwrap()).unwrap().unwrap()
    };
    let (mut created_count, mut meta_count) = (0, 0);
    for (aggregate, event_type, _, canonical) in payloads() {
        if aggregate != "project" {
            continue;
        }
        match event_type.as_str() {
            "project.created" => {
                let row = reduce(&event_type, &canonical);
                for (key, value) in canonical.as_object().unwrap() {
                    assert_eq!(&row[key], value, "project.created {key}");
                }
                // Where the event says nothing the row says null, and the project starts with automatic pulls off.
                for key in ["defaultThreadEnvMode", "faviconPath", "projectIcon"] {
                    assert!(canonical.get(key).is_some() || row[key].is_null(), "{key}");
                }
                assert_eq!(row["autoPull"], false);
                created_count += 1;
            }
            "project.meta-updated" => {
                let id = format!("project-for-{meta_count}");
                let created = serde_json::json!({"projectId":id,"title":"T","workspaceRoot":"/w","defaultModelSelection":null,"scripts":[],"createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-01T00:00:00.000Z"});
                reduce("project.created", &created);
                let mut update = canonical.clone();
                update["projectId"] = serde_json::json!(id);
                let row = reduce(&event_type, &update);
                for (key, value) in update.as_object().unwrap() {
                    assert_eq!(&row[key], value, "project.meta-updated {key}");
                }
                meta_count += 1;
            }
            "project.deleted" => {
                let id = canonical["projectId"].as_str().unwrap().to_owned();
                let created = serde_json::json!({"projectId":id,"title":"T","workspaceRoot":"/w","defaultModelSelection":null,"scripts":[],"createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-01T00:00:00.000Z"});
                reduce("project.created", &created);
                let row = reduce(&event_type, &canonical);
                assert_eq!((row["deletedAt"].clone(), row["updatedAt"].clone()), (canonical["deletedAt"].clone(), canonical["deletedAt"].clone()));
            }
            other => panic!("{other}"),
        }
    }
    assert!(created_count >= 10 && meta_count >= 10, "{created_count} {meta_count}");
}
