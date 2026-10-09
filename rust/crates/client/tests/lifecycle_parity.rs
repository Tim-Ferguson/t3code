//! The words and rules of ending threads and choosing a workspace, replayed against what the
//! original's helpers answered (`rust/tools/generate_client_lifecycle_fixtures.mjs`).
use serde_json::Value;
use t3_client::{lifecycle, text_settings};

fn rows(kind: &str) -> Vec<Value> {
    let all: Vec<Value> = serde_json::from_str(include_str!("fixtures/lifecycle_logic.json")).unwrap();
    all.into_iter().filter(|row| row["type"] == kind).collect()
}

#[test]
fn a_confirmation_message_becomes_the_title_and_description_the_original_shows() {
    let rows = rows("confirm");
    assert!(rows.len() >= 8);
    for row in rows {
        let (title, description) = lifecycle::confirm_copy(row["message"].as_str().unwrap());
        assert_eq!(title, row["copy"]["title"].as_str().unwrap(), "{row}");
        assert_eq!(description.as_deref(), row["copy"]["description"].as_str(), "{row}");
    }
}

#[test]
fn the_messages_this_app_asks_split_into_the_originals_title_and_description() {
    let all = rows("confirm");
    let find = |message: &str| all.iter().find(|row| row["message"] == message).map(|row| row["copy"].clone()).unwrap_or_else(|| panic!("no row for {message}"));
    for message in [
        lifecycle::delete_thread_message("Fix it"),
        lifecycle::archive_thread_message("a"),
        lifecycle::remove_project_message("Web", "/work/web", None, 2),
        lifecycle::remove_project_message("Web", "/work/web", Some("Laptop"), 0),
    ] {
        let copy = find(&message);
        let (title, description) = lifecycle::confirm_copy(&message);
        assert_eq!((title.as_str(), description.as_deref()), (copy["title"].as_str().unwrap(), copy["description"].as_str()), "{message}");
    }
}

#[test]
fn the_previous_worktree_is_the_one_the_original_picks() {
    let rows = rows("previous");
    assert!(rows.len() >= 6);
    for row in rows {
        let owned: Vec<(Option<String>, Option<String>, i64, bool)> = row["input"]["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|thread| {
                (
                    thread["branch"].as_str().map(str::to_owned),
                    thread["worktreePath"].as_str().map(str::to_owned),
                    chrono::DateTime::parse_from_rfc3339(thread["updatedAt"].as_str().unwrap()).unwrap().timestamp_millis(),
                    !thread["archivedAt"].is_null() && thread.get("archivedAt").is_some(),
                )
            })
            .collect();
        let threads: Vec<lifecycle::WorktreeThread<'_>> = owned.iter().map(|(branch, path, updated_at, archived)| lifecycle::WorktreeThread { branch: branch.as_deref(), worktree_path: path.as_deref(), updated_at: *updated_at, archived: *archived }).collect();
        let found = lifecycle::previous_worktree(&threads, row["input"]["current"].as_str());
        assert_eq!(found.as_ref().map(|(_, path)| path.as_str()), row["seed"]["worktreePath"].as_str(), "{row}");
        assert_eq!(found.as_ref().and_then(|(branch, _)| branch.as_deref()), row["seed"]["branch"].as_str(), "{row}");
        assert_eq!(found.map(|(branch, _)| lifecycle::previous_worktree_label(branch.as_deref())), row["label"].as_str().map(str::to_owned), "{row}");
    }
}

#[test]
fn the_fetch_interval_is_what_the_original_resolves_from_the_policy() {
    let rows = rows("fetch");
    assert!(rows.len() >= 5);
    for row in rows {
        assert_eq!(text_settings::fetch_seconds(&row["policy"]), row["seconds"].as_u64().unwrap(), "{row}");
        assert_eq!(text_settings::preset_seconds(&row["policy"]), row["preset"].as_u64().unwrap(), "{row}");
    }
}
