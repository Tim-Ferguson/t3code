//! Reactions and rewriting words on a pull request, replayed against what the original's helpers
//! answered (`rust/tools/generate_client_pull_request_edit_fixtures.mjs`).
use serde_json::Value;
use std::collections::BTreeMap;
use t3_client::pull_request_edit as edit;

fn rows(kind: &str) -> Vec<Value> {
    let all: Vec<Value> = serde_json::from_str(include_str!("fixtures/pull_request_edit.json")).unwrap();
    all.into_iter().filter(|row| row["type"] == kind).collect()
}

#[test]
fn a_reaction_pill_reads_as_the_original_says_it() {
    let rows = rows("tooltip");
    assert!(rows.len() >= 8);
    for row in rows {
        assert_eq!(edit::reaction_tooltip(&row["reaction"]), row["text"].as_str().unwrap(), "{row}");
    }
    for row in self::rows("name") {
        let content = row["content"].as_str().unwrap();
        assert_eq!((edit::reaction_emoji(content), edit::reaction_name(content)), (row["emoji"].as_str().unwrap(), row["name"].as_str().unwrap()), "{row}");
    }
}

#[test]
fn a_press_in_flight_moves_its_pill_at_once_and_in_githubs_order() {
    let rows = rows("pending");
    assert!(rows.len() >= 6);
    for row in rows {
        let pending: BTreeMap<String, bool> = row["pending"].as_array().unwrap().iter().map(|pair| (pair[0].as_str().unwrap().to_owned(), pair[1] == true)).collect();
        let reactions: Vec<Value> = row["reactions"].as_array().unwrap().clone();
        let shown: Vec<Value> = row["shown"].as_array().unwrap().clone();
        assert_eq!(edit::apply_pending(&reactions, &pending), shown, "{row}");
    }
}

#[test]
fn who_may_rewrite_the_title_or_a_remark_is_who_the_original_allows() {
    let (changes, comments) = (rows("change-request"), rows("comment"));
    assert!(changes.len() >= 8 && comments.len() >= 40);
    for row in changes {
        assert_eq!(edit::can_edit_change_request(&row["detail"]), row["can"] == true, "{row}");
    }
    for row in comments {
        assert_eq!(edit::can_edit_comment(&row["detail"], &row["comment"]), row["can"] == true, "{row}");
    }
}
