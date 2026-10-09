//! Forks, merge-back and lineage replayed against the original
//! (`rust/tools/generate_client_thread_relations_fixtures.mjs`).
use serde_json::{Value, json};
use t3_client::thread_relations::{self as relations, Edge, Kind, Shell};

fn rows(kind: &str) -> Vec<Value> {
    let rows: Vec<Value> = include_str!("fixtures/thread-relations.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).filter(|row: &Value| row["type"] == kind).collect();
    assert!(!rows.is_empty(), "no {kind} rows");
    rows
}
fn edge_json(edge: &Edge) -> Value {
    let kind = match edge.kind {
        Kind::Fork => "fork",
        Kind::Subagent => "subagent",
        Kind::Transfer => "transfer",
    };
    json!({"sourceThreadId":edge.source,"targetThreadId":edge.target,"kind":kind,"status":edge.status})
}

#[test]
fn status_words_are_the_originals() {
    for row in rows("status") {
        assert_eq!(relations::status_label(row["status"].as_str()), row["output"].as_str().unwrap(), "{row}");
    }
}

#[test]
fn the_graph_its_rows_and_their_order_are_the_originals() {
    for row in rows("graph") {
        let shells: Vec<Shell> = row["input"]["shells"].as_array().unwrap().iter().map(Shell::from_json).collect();
        let projection = (!row["input"]["projection"].is_null()).then(|| row["input"]["projection"].clone());
        let graph = relations::graph(&shells, projection.as_ref());
        let current = row["current"].as_str().unwrap();
        assert_eq!(json!(graph.edges.iter().map(edge_json).collect::<Vec<_>>()), row["edges"], "edges: {row}");
        let immediate = graph.immediate(current);
        assert_eq!(json!(immediate.iter().map(|r| json!({"thread":r.thread,"edge":edge_json(&r.edge)})).collect::<Vec<_>>()), row["immediate"], "immediate: {row}");
        let ordered = relations::order_rows(&graph, immediate, current, row["mergeTarget"].as_str());
        assert_eq!(json!(ordered.iter().map(|r| json!({"thread":r.thread,"status":graph.row_status(r)})).collect::<Vec<_>>()), row["ordered"], "ordered: {row}");
    }
}

#[test]
fn a_fork_merges_back_into_the_thread_it_was_taken_from() {
    for row in rows("merge-target") {
        assert_eq!(relations::merge_back_target(&row["thread"]).as_deref(), row["output"].as_str(), "{row}");
    }
    for row in rows("graph") {
        // The graph rows also carry the target of the thread they were built for.
        let shell = row["input"]["shells"].as_array().unwrap().iter().find(|shell| shell["id"] == row["current"]).cloned();
        if let Some(shell) = shell {
            assert_eq!(relations::merge_back_target(&shell).as_deref(), row["merge"].as_str(), "{row}");
        }
    }
}

#[test]
fn the_newest_finished_run_is_the_one_to_merge_unless_a_newer_one_is_working() {
    for row in rows("merge-run") {
        let projection = json!({"runs":row["runs"]});
        let id = relations::latest_merge_back_run(&projection).and_then(|run| run["id"].as_str());
        assert_eq!(id, row["output"].as_str(), "{row}");
    }
}

#[test]
fn which_answers_may_be_forked_is_the_originals() {
    for row in rows("can-fork") {
        let capabilities = (!row["capabilities"].is_null()).then(|| row["capabilities"].clone());
        assert_eq!(relations::can_fork_assistant_item(&row["item"], capabilities.as_ref()), row["output"].as_bool().unwrap(), "{row}");
    }
}

#[test]
fn handoff_endpoints_recover_the_models_the_original_recovers() {
    for row in rows("handoff") {
        let runs = row["runs"].as_array().unwrap();
        let (from, to) = relations::handoff_endpoints(&row["item"], runs);
        let endpoint = |e: &relations::Endpoint| match &e.model {
            Some(model) => json!({"instanceId":e.instance_id,"model":model}),
            None => json!({"instanceId":e.instance_id}),
        };
        assert_eq!(json!({"from":from.iter().map(endpoint).collect::<Vec<_>>(),"to":endpoint(&to)}), row["output"], "{row}");
    }
}

#[test]
fn the_commands_carry_what_the_original_sends() {
    assert_eq!(
        relations::fork_command("source", "target", "run-1", &relations::fork_title("Thread"), "web"),
        json!({"type":"thread.fork","createdBy":"user","creationSource":"web","sourceThreadId":"source","targetThreadId":"target","sourcePoint":{"type":"run","runId":"run-1"},"title":"Thread fork"})
    );
    assert_eq!(
        relations::merge_back_command("fork", "source", "run-2", "web"),
        json!({"type":"thread.merge_back","createdBy":"user","creationSource":"web","sourceThreadId":"fork","targetThreadId":"source","sourcePoint":{"type":"run","runId":"run-2"}})
    );
    assert_eq!(relations::merge_back_label(Some("Thread")), "Merge back to Thread");
    assert_eq!(relations::merge_back_label(None), "Merge back to source conversation");
    assert_eq!(relations::merge_back_tooltip(false, Some("Thread")), "Complete a run in this fork before merging it back");
    assert_eq!(relations::merge_back_tooltip(true, Some("Thread")), "Merge this conversation back into Thread");
    assert_eq!(relations::merge_back_tooltip(true, None), "Merge this conversation back into its source");
    assert_eq!(relations::fork_row(&json!({"source":{"type":"run","threadId":"s"},"targetThreadId":"t"})), ("Forked from conversation", "Open source conversation", "s".into()));
    assert_eq!(relations::fork_row(&json!({"source":{"type":"node"},"targetThreadId":"t"})), ("Conversation fork", "Open fork", "t".into()));
}
