//! The sidebar's sections, their order and the drop of a row, replayed against the original
//! (`rust/tools/generate_client_sidebar_sections_fixtures.mjs`).
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use t3_client::sidebar::{self, DropInput, DropPlan, Section, Sections, Support};
use t3_contracts::ThreadShell;

fn rows(kind: &str) -> Vec<Value> {
    let rows: Vec<Value> = include_str!("fixtures/sidebar-sections.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).filter(|row: &Value| row["type"] == kind).collect();
    assert!(!rows.is_empty(), "no rows for {kind}");
    rows
}
fn time(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
}
fn shells(row: &Value) -> Vec<ThreadShell> {
    row["shells"].as_array().unwrap().iter().map(|shell| serde_json::from_value(shell.clone()).unwrap_or_else(|error| panic!("{error}: {shell}"))).collect()
}
fn support(row: &Value) -> Support {
    Support { settlement: row["support"]["settlement"] == true, snooze: row["support"]["snooze"] == true }
}
fn ids(list: &[&ThreadShell]) -> Value {
    json!(list.iter().map(|thread| thread.id.to_string()).collect::<Vec<_>>())
}

#[test]
fn threads_land_in_the_sections_and_order_the_original_gives_them() {
    for row in rows("sections") {
        let threads = shells(&row);
        let sections = Sections::of(threads.iter(), time(row["now"].as_str().unwrap()), support(&row));
        let got = json!({"pinned":ids(&sections.pinned),"active":ids(&sections.active),"snoozed":ids(&sections.snoozed),"settled":ids(&sections.settled)});
        assert_eq!(got, row["output"], "{row}");
    }
}

#[test]
fn a_snooze_ends_and_wakes_as_the_original_says() {
    let mut woke = 0;
    for row in rows("sections") {
        let now = time(row["now"].as_str().unwrap());
        for (thread, expected) in shells(&row).iter().zip(row["woke"].as_array().unwrap()) {
            assert_eq!(sidebar::snoozed_until(thread, now).is_some(), expected["snoozed"] == true, "snoozed: {thread:?}");
            let got = sidebar::woke_time(thread, now).map(|at| at.timestamp_millis());
            assert_eq!(got, expected["wokeAt"].as_str().map(|at| time(at).timestamp_millis()), "woke: {thread:?}");
            woke += got.is_some() as usize;
        }
    }
    assert!(woke > 20, "the fixtures wake threads: {woke}");
}

fn keys(threads: &[ThreadShell], pinned: bool) -> BTreeMap<String, Option<String>> {
    threads.iter().map(|thread| (thread.id.to_string(), if pinned { thread.pin_order_key.clone().flatten() } else { thread.active_order_key.clone().flatten() }.map(|key| key.to_string()))).collect()
}

#[test]
fn a_drop_resolves_its_target_and_asks_the_server_for_what_the_original_asks() {
    let mut kinds = BTreeSet::new();
    for row in rows("drop") {
        let threads = shells(&row);
        let now = time(row["now"].as_str().unwrap());
        let sections = Sections::of(threads.iter(), now, support(&row));
        let moved = row["moved"].as_str().unwrap();
        let from = Section::parse(row["from"].as_str().unwrap()).unwrap();
        assert_eq!(sidebar::section_of(threads.iter().find(|thread| thread.id.as_str() == moved).unwrap(), now, support(&row)), from);
        if row["target"].is_null() {
            continue;
        }
        let expected = &row["target"];
        let section = Section::parse(expected["section"].as_str().unwrap()).unwrap();
        // The row lands in front of whatever follows it in the section it joins.
        let order: Vec<&str> = expected[if section == Section::Pinned { "pinnedOrder" } else { "activeOrder" }].as_array().unwrap().iter().map(|id| id.as_str().unwrap()).collect();
        let before = order.iter().position(|id| *id == moved).and_then(|at| order.get(at + 1).copied());
        let target = sidebar::drop_target(&sections, moved, section, before).unwrap_or_else(|| panic!("{row}"));
        if matches!(section, Section::Pinned | Section::Active) {
            assert_eq!(json!(target.pinned_order), expected["pinnedOrder"], "{row}");
            assert_eq!(json!(target.active_order), expected["activeOrder"], "{row}");
        }
        assert_eq!(sidebar::drop_verb(from, Some(section)).map(|verb| verb.label().to_lowercase()), row["verb"].as_str().map(str::to_owned), "{row}");
        let moved_shell = threads.iter().find(|thread| thread.id.as_str() == moved).unwrap();
        let all: BTreeSet<String> = threads.iter().map(|thread| thread.id.to_string()).collect();
        let (pinned_keys, active_keys) = (keys(&threads, true), keys(&threads, false));
        let (pinned_order, active_order): (Vec<String>, Vec<String>) = (sections.pinned.iter().map(|t| t.id.to_string()).collect(), sections.active.iter().map(|t| t.id.to_string()).collect());
        // The fixture's own target, so a divergence in the plan is told apart from one in the target.
        let from_original = sidebar::DropTarget { section, pinned_order: serde_json::from_value(expected["pinnedOrder"].clone()).unwrap(), active_order: serde_json::from_value(expected["activeOrder"].clone()).unwrap() };
        let plan = sidebar::plan_drop(&DropInput {
            moved,
            from,
            moved_pinned: sidebar::is_pinned(moved_shell),
            moved_settled: moved_shell.settled_override == Some(t3_contracts::SettledOverride::Settled),
            supports_settlement: support(&row).settlement,
            target: &from_original,
            pinned_order: &pinned_order,
            pinned_keys: &pinned_keys,
            pin_writable: Some(&if row["pinWritable"] == true { all.clone() } else { BTreeSet::new() }),
            active_order: &active_order,
            active_keys: &active_keys,
            active_writable: Some(&if row["activeWritable"] == true { all.clone() } else { BTreeSet::new() }),
        });
        let assignments = |list: &[sidebar::Assignment]| json!(list.iter().map(|a| json!({"id":a.id,"orderKey":a.order_key})).collect::<Vec<_>>());
        let got = match &plan {
            DropPlan::None => json!({"kind":"none"}),
            DropPlan::Settle => json!({"kind":"settle"}),
            DropPlan::ReorderPinned { order, assignments: list } => json!({"kind":"reorder-pinned","order":order,"assignments":assignments(list)}),
            DropPlan::Pin { order, order_key, extra } => json!({"kind":"pin","order":order,"orderKey":order_key,"extraAssignments":assignments(extra)}),
            DropPlan::MoveActive { order, assignments: list, unpin, unsettle, unsnooze } => json!({"kind":"move-active","order":order,"assignments":assignments(list),"unpin":unpin,"unsettle":unsettle,"unsnooze":unsnooze}),
        };
        let mut want = row["plan"].clone();
        // JSON drops an undefined order key; `None` is that.
        if want["kind"] == "pin" && want.get("orderKey").is_none() {
            want["orderKey"] = Value::Null;
        }
        kinds.insert(want["kind"].as_str().unwrap().to_owned());
        assert_eq!(got, want, "{row}");
    }
    assert_eq!(kinds.len(), 5, "every kind of drop is replayed: {kinds:?}");
}

#[test]
fn a_folded_shelf_still_shows_the_open_thread_and_the_settled_shelf_pages() {
    let shells: Vec<ThreadShell> = (0..40).map(|n| serde_json::from_value(json!({"createdBy":"user","creationSource":"web","id":format!("t{n}"),"projectId":"p","title":"T","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,"lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":format!("t{n}")},"forkedFrom":null,"activeProviderThreadId":null,"latestRunId":null,"activeRunId":null,"status":"idle","pendingRuntimeRequest":null,"latestVisibleMessage":null,"latestUserMessageAt":null,"hasActionableProposedPlan":false,"itemCount":1,"visibleItemCount":1,"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","archivedAt":null,"settledOverride":"settled","settledAt":format!("2026-02-01T00:{:02}:00Z", n),"deletedAt":null})).unwrap()).collect();
    let sections = Sections::of(shells.iter(), time("2026-03-01T00:00:00Z"), Support { settlement: true, snooze: true });
    assert_eq!(sections.settled.len(), 40);
    // Newest settle first; folded away it lists nothing, unless the thread is the one open.
    assert_eq!(sections.settled[0].id.as_str(), "t39");
    assert!(sidebar::shelf_rows(&sections.settled, false, sidebar::SETTLED_INITIAL, None).is_empty());
    let open = sidebar::shelf_rows(&sections.settled, false, sidebar::SETTLED_INITIAL, Some("t3"));
    assert_eq!(open.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["t3"]);
    // Open: a page of ten, and the thread open beyond it is appended rather than hidden.
    assert_eq!(sidebar::shelf_rows(&sections.settled, true, sidebar::SETTLED_INITIAL, None).len(), 10);
    let paged = sidebar::shelf_rows(&sections.settled, true, sidebar::SETTLED_INITIAL, Some("t3"));
    assert_eq!((paged.len(), paged[10].id.as_str()), (11, "t3"));
    assert_eq!(sidebar::shelf_rows(&sections.settled, true, sidebar::SETTLED_INITIAL + sidebar::SETTLED_PAGE, None).len(), 35);
    // Without the capability nothing is settled.
    let none = Sections::of(shells.iter(), time("2026-03-01T00:00:00Z"), Support::default());
    assert!(none.settled.is_empty() && none.active.len() == 40);
}
