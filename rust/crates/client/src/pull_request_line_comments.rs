//! Commenting on a line of a pull request's diff: where a line is on the host, the comments a review holds before it is sent,
//! and which review threads sit on which line. Port of `resolveDiffReviewPosition` (`reviewCommentContext.ts`),
//! `pullRequestReviewStore.ts` and the annotation placement of `PullRequestCodeTab.tsx`.
//!
//! A comment is not posted as it is written. The review is one request carrying every line comment with the verdict, so a
//! half-written one is invisible to everyone else, on the hosts that keep no pending review as on the others. A range collapses
//! to its last line (only GitHub carries a multi-line comment, and one that silently lost its first line elsewhere would be
//! worse than one line), which here is the line whose button was pressed.
use crate::diff::{DiffLine, LineKind};
use serde_json::{Value, json};

/// `PullRequestReviewPosition` of a line. A context line exists on both sides, so the side it was chosen on is part of it:
/// `right_side` is the new side.
pub fn position(line: &DiffLine, right_side: bool) -> Option<Value> {
    match line.kind {
        LineKind::Addition => line.new_number.map(|new| json!({"kind":"added","newLine":new})),
        LineKind::Deletion => line.old_number.map(|old| json!({"kind":"deleted","oldLine":old})),
        LineKind::Context => Some(json!({"kind":"context","oldLine":line.old_number?,"newLine":line.new_number?,"side":if right_side { "right" } else { "left" }})),
    }
}

/// The side and number of the line a position names, as a review thread names it.
pub fn anchor(position: &Value) -> Option<(&'static str, u64)> {
    match position["kind"].as_str()? {
        "added" => Some(("right", position["newLine"].as_u64()?)),
        "deleted" => Some(("left", position["oldLine"].as_u64()?)),
        "context" => {
            if position["side"] == "left" {
                Some(("left", position["oldLine"].as_u64()?))
            } else {
                Some(("right", position["newLine"].as_u64()?))
            }
        }
        _ => None,
    }
}

/// Whether a thread (or a pending comment) anchored at `side`/`line` sits on this diff line. A thread names the side it was left
/// on: the old file for `left`, the new for `right`.
pub fn is_on(side: &str, number: u64, line: &DiffLine) -> bool {
    let here = if side == "left" { line.old_number } else { line.new_number };
    here.map(u64::from) == Some(number) && match line.kind {
        LineKind::Addition => side == "right",
        LineKind::Deletion => side == "left",
        LineKind::Context => true,
    }
}
/// The anchor of a review thread: its side and line, none when the host put it on no line of the diff, or when the line it was
/// written against is no longer in it (an outdated thread is listed apart, not pinned to the wrong line).
pub fn thread_anchor(thread: &Value) -> Option<(String, u64)> {
    if thread["isOutdated"] == true {
        return None;
    }
    Some((thread["side"].as_str().unwrap_or("right").to_owned(), thread["line"].as_u64()?))
}

/// One remark of a review not yet sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    /// Names the remark among its siblings; a counter, so two remarks of equal length on one line never share one.
    pub id: u64,
    pub path: String,
    /// What the file was called before the change, sent only when it differs: GitLab resolves a position against both.
    pub old_path: Option<String>,
    pub position: Value,
    pub body: String,
}
impl Pending {
    /// The `PullRequestReviewCommentDraft` the review carries.
    pub fn wire(&self) -> Value {
        let mut draft = json!({"path":self.path,"position":self.position,"body":self.body});
        if let Some(old) = &self.old_path {
            draft["oldPath"] = json!(old);
        }
        draft
    }
    /// "src/lib.rs:12".
    pub fn label(&self) -> String {
        match anchor(&self.position) {
            Some((_, line)) => format!("{}:{line}", self.path),
            None => self.path.clone(),
        }
    }
}

/// A review being written, per pull request, until it is sent: the summary and the line comments.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReviewDraft {
    pub summary: String,
    pub comments: Vec<Pending>,
    next_id: u64,
}
impl ReviewDraft {
    pub fn add(&mut self, path: &str, old_path: Option<&str>, position: Value, body: &str) -> u64 {
        self.next_id += 1;
        self.comments.push(Pending { id: self.next_id, path: path.to_owned(), old_path: old_path.map(str::to_owned), position, body: body.to_owned() });
        self.next_id
    }
    pub fn remove(&mut self, id: u64) {
        self.comments.retain(|comment| comment.id != id);
    }
    /// What a submitted review took off the draft: the remarks it carried, and the summary only if it was not revised meanwhile.
    pub fn sent(&mut self, ids: &[u64], summary: &str) {
        self.comments.retain(|comment| !ids.contains(&comment.id));
        if self.summary == summary {
            self.summary.clear();
        }
    }
    /// Whether any review work is waiting (a pending remark, or a summary typed and not sent).
    pub fn started(&self) -> bool {
        !self.comments.is_empty() || !self.summary.trim().is_empty()
    }
}

/// `canSubmit` of the review form: an approval may be empty, anything else needs words or a line comment, and Forgejo wants
/// a summary to request changes whatever else it carries.
pub fn can_submit(verdict: &str, summary: &str, comments: usize, summary_required_to_request_changes: bool) -> bool {
    if verdict == "request-changes" && summary_required_to_request_changes {
        return !summary.trim().is_empty();
    }
    verdict == "approve" || !summary.trim().is_empty() || comments > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>) -> DiffLine {
        DiffLine { kind, old_number: old, new_number: new, text: String::new(), no_newline: false }
    }

    #[test]
    fn a_line_is_named_to_the_host_by_what_kind_of_line_it_is_and_a_context_line_by_the_side_chosen() {
        assert_eq!(position(&line(LineKind::Addition, None, Some(7)), true), Some(json!({"kind":"added","newLine":7})));
        assert_eq!(position(&line(LineKind::Deletion, Some(4), None), true), Some(json!({"kind":"deleted","oldLine":4})));
        let context = line(LineKind::Context, Some(3), Some(5));
        assert_eq!(position(&context, true), Some(json!({"kind":"context","oldLine":3,"newLine":5,"side":"right"})));
        assert_eq!(position(&context, false).and_then(|p| anchor(&p)), Some(("left", 3)));
        assert_eq!(anchor(&json!({"kind":"added","newLine":7})), Some(("right", 7)));
    }

    #[test]
    fn a_thread_sits_on_the_line_of_its_side() {
        let context = line(LineKind::Context, Some(3), Some(5));
        assert!(is_on("right", 5, &context) && is_on("left", 3, &context) && !is_on("right", 3, &context));
        let added = line(LineKind::Addition, None, Some(5));
        assert!(is_on("right", 5, &added) && !is_on("left", 5, &added));
        let deleted = line(LineKind::Deletion, Some(5), None);
        assert!(is_on("left", 5, &deleted) && !is_on("right", 5, &deleted));
        assert_eq!(thread_anchor(&json!({"side":"left","line":9})), Some(("left".into(), 9)));
        assert_eq!(thread_anchor(&json!({"side":"right","line":null})), None);
        assert_eq!(thread_anchor(&json!({"side":"right","line":3,"isOutdated":true})), None, "outdated threads sit apart");
    }

    #[test]
    fn a_review_keeps_its_remarks_until_sent_and_a_summary_revised_meanwhile_is_new_work() {
        let mut draft = ReviewDraft::default();
        let first = draft.add("a.rs", None, json!({"kind":"added","newLine":1}), "same");
        let second = draft.add("a.rs", Some("b.rs"), json!({"kind":"added","newLine":1}), "same");
        assert_ne!(first, second, "equal remarks on one line are still two");
        assert_eq!(draft.comments[1].wire()["oldPath"], "b.rs");
        assert!(draft.comments[0].wire().get("oldPath").is_none());
        draft.summary = "Looks good".into();
        draft.remove(first);
        draft.sent(&[second], "Looks good, mostly");
        assert!(draft.comments.is_empty() && draft.summary == "Looks good", "a revised summary stays");
        draft.sent(&[], "Looks good");
        assert!(!draft.started());
    }

    #[test]
    fn what_can_be_sent_depends_on_the_verdict_and_on_what_the_review_holds() {
        assert!(can_submit("approve", "", 0, false));
        assert!(!can_submit("comment", " ", 0, false) && can_submit("comment", "", 1, false) && can_submit("comment", "x", 0, false));
        assert!(!can_submit("request-changes", "", 2, true), "Forgejo wants words to request changes");
        assert!(can_submit("request-changes", "", 2, false));
    }
}
