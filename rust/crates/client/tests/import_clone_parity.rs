//! Project import grouping and tracked-clone wording replayed against the original
//! (`rust/tools/generate_client_import_fixtures.mjs`).
use serde_json::{Value, json};
use t3_client::{project_clone, project_import as import};

fn rows(kind: &str) -> Vec<Value> {
    let rows: Vec<Value> = include_str!("fixtures/import-clone.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).filter(|row: &Value| row["type"] == kind).collect();
    assert!(!rows.is_empty());
    rows
}

#[test]
fn the_scan_is_grouped_and_selected_as_the_original_does() {
    for row in rows("import") {
        let scan = import::Scan::decode(&json!({"candidates":row["candidates"]})).unwrap();
        let name = row["name"].as_str().unwrap();
        let now = row["now"].as_f64().unwrap();
        let recent: Vec<&str> = import::recent(&scan.candidates, now).into_iter().map(|candidate| candidate.path.as_str()).collect();
        assert_eq!(json!(recent), row["recent"], "recent in {name}");
        let (repositories, other) = import::group(&scan.candidates);
        let described: Vec<Value> = repositories.iter().map(|group| json!({"key":group.key,"label":group.label,"repository":group.repository,"paths":group.candidates.iter().map(|candidate| candidate.path.as_str()).collect::<Vec<_>>(),"threadCount":group.thread_count as i64,"lastActiveAt":group.last_active_at})).collect();
        assert_eq!(json!(described), row["repositories"], "groups in {name}");
        let other: Vec<&str> = other.into_iter().map(|candidate| candidate.path.as_str()).collect();
        assert_eq!(json!(other), row["other"], "folders in {name}");
    }
}

#[test]
fn a_clone_is_named_and_summed_up_as_the_original_words_it() {
    for row in rows("clone") {
        let snapshot: project_clone::Snapshot = serde_json::from_value(json!({
            "projectId":"p","remoteUrl":"u","destinationPath":row["snapshot"]["destinationPath"],"repository":row["snapshot"]["repository"],
            "phase":"running","stage":row["snapshot"]["stage"],"percent":row["snapshot"]["percent"],"detail":row["snapshot"]["detail"],"error":null,"sequence":1
        })).unwrap();
        assert_eq!(project_clone::display_name(&snapshot), row["name"].as_str().unwrap());
        assert_eq!(project_clone::progress_summary(&snapshot), row["summary"].as_str().unwrap());
    }
}

#[test]
fn projects_resolve_by_the_servers_match_then_by_path_and_runs_are_worded() {
    let candidate = |path: &str, id: Option<&str>| import::Candidate { path: path.into(), title: "t".into(), project_id: id.map(str::to_owned), sources: vec![], thread_count: 1.0, last_active_at: None, already_imported: id.is_some(), git: Default::default() };
    let projects = vec![("p1".to_owned(), "/work/a/".to_owned()), ("p2".to_owned(), "C:\\Code\\B".to_owned())];
    assert_eq!(import::resolve_project_id(&projects, &candidate("/x", Some("server"))), Some("server".into()));
    assert_eq!(import::resolve_project_id(&projects, &candidate("/work/a", None)), Some("p1".into()));
    assert_eq!(import::resolve_project_id(&projects, &candidate("c:/code/b", None)), Some("p2".into()));
    assert_eq!(import::resolve_project_id(&projects, &candidate("/nowhere", None)), None);
    assert_eq!(import::warning(3.0, 1.0), "Imported 3 threads. 1 thread could not be imported.");
    assert_eq!(import::warning(0.0, 2.0), "2 threads could not be imported.");
    assert_eq!(import::warning(0.0, 1.0), "1 thread could not be imported.");
    assert_eq!(import::warning(1.0, 0.0), "Imported 1 thread. Some thread history could not be imported.");
    assert_eq!(import::warning(0.0, 0.0), "Could not import thread history.");
    assert_eq!(import::import_label(1, false), "Import 1 project");
    assert_eq!(import::import_label(2, true), "Importing\u{2026}");
}
