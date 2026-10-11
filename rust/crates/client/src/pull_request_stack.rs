//! The rules behind merging and rebasing a pull request's stack: which layers an action reaches, when it may be asked for,
//! what the request carries, and the words for asking and for the answer. Port of `PullRequestStackMenu.tsx`,
//! `pullRequestStackSnapshot.ts` (`pullRequestStackView`) and `allowsSinglePullRequestMerge`
//! (`pullRequestDetail.logic.ts`). The stack is the host's own, read as `pullRequests.stack` and kept as the wire gives it.
use crate::pull_request::Reference;
use serde_json::{Value, json};

/// One layer of a stack, bottom to top.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub number: u64,
    pub title: String,
    pub head_branch: String,
    /// What the layer's branch pointed at when the stack was read; an action names it, so a branch that moved is refused.
    pub head_sha: Option<String>,
    pub is_draft: bool,
    /// `open`, `closed` or `merged`.
    pub state: String,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Stack {
    pub number: u64,
    pub base: String,
    pub layers: Vec<Layer>,
}
impl Stack {
    pub fn parse(value: &Value) -> Option<Self> {
        let layers = value["layers"]
            .as_array()?
            .iter()
            .map(|layer| {
                Some(Layer {
                    number: layer["number"].as_u64()?,
                    title: layer["title"].as_str().unwrap_or_default().to_owned(),
                    head_branch: layer["headBranch"].as_str().unwrap_or_default().to_owned(),
                    head_sha: layer["headSha"].as_str().filter(|sha| !sha.is_empty()).map(str::to_owned),
                    is_draft: layer["isDraft"].as_bool().unwrap_or(false),
                    state: layer["state"].as_str().unwrap_or("open").to_owned(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self { number: value["number"].as_u64()?, base: value["base"].as_str().unwrap_or_default().to_owned(), layers })
    }
    /// The layer's title as the list shows it: the title, else its branch.
    pub fn label(layer: &Layer) -> &str {
        if layer.title.is_empty() { &layer.head_branch } else { &layer.title }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Merge,
    Rebase,
}

/// What a read of the stack has told the panel (`usePullRequestStack`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// Settled by a read that succeeded: an action may only be asked for from what the host just said.
    pub fresh: bool,
    pub notice: Option<&'static str>,
}
/// `pullRequestStackView`: whether what is shown is fresh, and the one line to say when it is not. `has_data` is whether there
/// is a stack to show at all (the last good read counts: a failed refresh keeps it).
pub fn view(has_data: bool, succeeded: bool, pending: bool, error: Option<&str>) -> View {
    View {
        fresh: succeeded && !pending,
        notice: match (has_data, error, succeeded && !pending) {
            (false, ..) => None,
            (true, Some(_), _) => Some("Stack data may be stale. We couldn\u{2019}t refresh it."),
            (true, None, false) => Some("Refreshing stack\u{2026} Showing saved data."),
            (true, None, true) => None,
        },
    }
}

/// `allowsSinglePullRequestMerge`: where the host can act on stacks, a single pull request is merged only once the stack lookup
/// has settled and found none; environments without stack actions keep their existing actions.
pub fn single_merge_allowed(supports_stack_actions: bool, has_stack: bool, stack_pending: bool, stack_error: bool) -> bool {
    !supports_stack_actions || (!has_stack && !stack_pending && !stack_error)
}

/// What the stack allows for the pull request in view.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// 1-based; 0 when the pull request is not in the stack.
    pub position: usize,
    pub unmerged: Vec<Layer>,
    /// The layers a merge reaches: the ones from the bottom up to and including this one, merged ones left out.
    pub merge_layers: Vec<Layer>,
    pub selected: Option<Layer>,
    pub top: Option<Layer>,
    pub merge_disabled: bool,
    pub rebase_disabled: bool,
    /// A layer to merge is closed or a draft: said beside the buttons.
    pub merge_blocked: bool,
}
pub fn plan(stack: &Stack, number: u64, pending: bool) -> Plan {
    let position = stack.layers.iter().position(|layer| layer.number == number).map_or(0, |at| at + 1);
    let unmerged: Vec<Layer> = stack.layers.iter().filter(|layer| layer.state != "merged").cloned().collect();
    let merge_layers: Vec<Layer> = stack.layers.iter().take(position).filter(|layer| layer.state != "merged").cloned().collect();
    let selected = position.checked_sub(1).and_then(|at| stack.layers.get(at)).cloned();
    let has_closed = unmerged.iter().any(|layer| layer.state != "open");
    let merge_has_closed = merge_layers.iter().any(|layer| layer.state != "open");
    let has_unknown_head = unmerged.iter().any(|layer| layer.head_sha.is_none());
    let merge_blocked = merge_has_closed || merge_layers.iter().any(|layer| layer.is_draft);
    Plan {
        position,
        merge_disabled: pending
            || selected.as_ref().is_none_or(|layer| layer.state != "open")
            || merge_layers.iter().any(|layer| layer.head_sha.is_none())
            || merge_has_closed
            || merge_layers.is_empty()
            || merge_layers.iter().any(|layer| layer.is_draft),
        rebase_disabled: pending || has_unknown_head || has_closed || unmerged.is_empty(),
        merge_blocked,
        top: stack.layers.last().cloned(),
        selected,
        unmerged,
        merge_layers,
    }
}
impl Plan {
    pub fn layers(&self, action: Action) -> &[Layer] {
        match action {
            Action::Merge => &self.merge_layers,
            Action::Rebase => &self.unmerged,
        }
    }
    pub fn disabled(&self, action: Action) -> bool {
        match action {
            Action::Merge => self.merge_disabled,
            Action::Rebase => self.rebase_disabled,
        }
    }
}

/// The request for `pullRequests.runAction` (`run` in the menu): a merge names the pull request in view and the layers up to it;
/// a rebase names the top layer and every open one. Each layer is named with the head that was read. None when the action
/// cannot be asked for from this stack.
pub fn run_input(stack: &Stack, plan: &Plan, reference: &Reference, action: Action, merge_method: &str) -> Option<Value> {
    if plan.disabled(action) {
        return None;
    }
    let target = match action {
        Action::Merge => plan.selected.as_ref(),
        Action::Rebase => plan.top.as_ref(),
    }?;
    target.head_sha.as_ref()?;
    let heads: Vec<Value> = plan.layers(action).iter().filter_map(|layer| layer.head_sha.as_ref().map(|sha| json!({"number":layer.number,"headSha":sha}))).collect();
    let mut extra = json!({"stackNumber":stack.number,"expectedStackHeads":heads});
    match action {
        Action::Merge => {
            extra["action"] = json!("merge");
            extra["mergeMethod"] = json!(merge_method);
        }
        Action::Rebase => {
            extra["action"] = json!("update-branch");
            extra["updateMethod"] = json!("rebase");
        }
    }
    let mut input = reference.payload(extra);
    input["number"] = json!(target.number);
    Some(input)
}

/// The question asked before the action: its title and its words.
// TODO(original PullRequestStackMenu.tsx): the title says "Merge 1 pull requests?" for a stack of one layer; kept as it is.
pub fn confirmation(action: Action, reference_number: u64, base: &str, merge_method: &str, layers: usize) -> (String, String) {
    match action {
        Action::Merge => (
            format!("Merge {layers} pull requests?"),
            format!("Merge #{reference_number} and its unmerged layers below into {base} using {merge_method}. GitHub checks their rules before merging or queueing them and rebases the remaining stack after merging."),
        ),
        Action::Rebase => (
            format!("Rebase {layers} pull requests?"),
            format!("Rebase the remote branches from bottom to top onto {base}. This rewrites branch history and may restart checks. If a layer fails, earlier updates remain."),
        ),
    }
}
pub fn confirm_label(action: Action) -> &'static str {
    match action {
        Action::Merge => "Merge stack",
        Action::Rebase => "Rebase stack",
    }
}
/// The tooltip of the button that merges the stack through the pull request in view.
pub fn merge_tooltip(number: u64, base: &str, layers: usize) -> String {
    format!("Merge stack through #{number} into {base} ({layers} {})", if layers == 1 { "pull request" } else { "pull requests" })
}
pub const FAILURE_TITLE: &str = "Stack operation did not complete";
/// The title and the line under it for an action that went through.
pub fn success(action: Action) -> (&'static str, Option<&'static str>) {
    match action {
        Action::Merge => ("Stack merge request completed", Some("GitHub merged the stack or added it to its merge queue.")),
        Action::Rebase => ("Stack rebased", None),
    }
}
/// What to say of a failed action: the server's own sentence (`PullRequestOperationError.detail`), without the words that name the
/// operation.
pub fn failure_text(raw: &str) -> String {
    let detail = crate::pull_request::operation_detail(raw).trim();
    if detail.is_empty() { "The stack operation did not complete.".to_owned() } else { detail.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(number: u64, state: &str, draft: bool, sha: Option<&str>) -> Value {
        json!({"number":number,"title":format!("Layer {number}"),"headBranch":format!("branch-{number}"),"headSha":sha,"isDraft":draft,"state":state})
    }
    fn stack_of(layers: Vec<Value>) -> Stack {
        Stack::parse(&json!({"number":9,"base":"main","layers":layers})).unwrap()
    }
    fn open_stack() -> Stack {
        stack_of(vec![layer(1, "open", false, Some("aaa")), layer(2, "open", false, Some("bbb")), layer(3, "open", false, Some("ccc"))])
    }
    fn reference(number: u64) -> Reference {
        Reference { project_id: "project".into(), host: Some("github.com".into()), repository: "acme/web".into(), number }
    }

    #[test]
    fn a_stack_is_read_from_the_wire_and_a_broken_one_is_not() {
        let stack = open_stack();
        assert_eq!((stack.number, stack.base.as_str(), stack.layers.len()), (9, "main", 3));
        assert_eq!(Stack::label(&stack.layers[0]), "Layer 1");
        let untitled = Stack::parse(&json!({"number":1,"base":"main","layers":[{"number":5,"headBranch":"feature","state":"open"}]})).unwrap();
        assert_eq!(Stack::label(&untitled.layers[0]), "feature", "a layer with no title is its branch");
        assert!(untitled.layers[0].head_sha.is_none());
        assert!(Stack::parse(&json!({"number":1,"layers":[{"title":"no number"}]})).is_none());
        assert!(Stack::parse(&json!({"layers":[]})).is_none());
    }

    #[test]
    fn a_merge_reaches_the_layers_up_to_the_one_in_view_and_a_rebase_every_open_one() {
        let stack = stack_of(vec![layer(1, "merged", false, Some("aaa")), layer(2, "open", false, Some("bbb")), layer(3, "open", false, Some("ccc")), layer(4, "open", false, Some("ddd"))]);
        let plan = plan(&stack, 3, false);
        assert_eq!(plan.position, 3);
        assert_eq!(plan.merge_layers.iter().map(|layer| layer.number).collect::<Vec<_>>(), [2, 3], "the merged layer is left out and the one above stays");
        assert_eq!(plan.unmerged.iter().map(|layer| layer.number).collect::<Vec<_>>(), [2, 3, 4]);
        assert!(!plan.merge_disabled && !plan.rebase_disabled && !plan.merge_blocked);
        let merge = run_input(&stack, &plan, &reference(3), Action::Merge, "squash").unwrap();
        assert_eq!(
            merge,
            json!({"projectId":"project","host":"github.com","repository":"acme/web","number":3,"stackNumber":9,"expectedStackHeads":[{"number":2,"headSha":"bbb"},{"number":3,"headSha":"ccc"}],"action":"merge","mergeMethod":"squash"})
        );
        // A rebase asks for the top layer, whatever layer is in view, and names every open layer.
        let rebase = run_input(&stack, &plan, &reference(3), Action::Rebase, "squash").unwrap();
        assert_eq!(
            rebase,
            json!({"projectId":"project","host":"github.com","repository":"acme/web","number":4,"stackNumber":9,"expectedStackHeads":[{"number":2,"headSha":"bbb"},{"number":3,"headSha":"ccc"},{"number":4,"headSha":"ddd"}],"action":"update-branch","updateMethod":"rebase"})
        );
    }

    #[test]
    fn what_cannot_be_asked_for_is_not_offered() {
        // A draft or a closed layer under the one in view stops the merge and says why; a rebase stops at a closed one only.
        let draft = stack_of(vec![layer(1, "open", true, Some("aaa")), layer(2, "open", false, Some("bbb"))]);
        let plan_draft = plan(&draft, 2, false);
        assert!(plan_draft.merge_disabled && plan_draft.merge_blocked && !plan_draft.rebase_disabled);
        assert!(run_input(&draft, &plan_draft, &reference(2), Action::Merge, "merge").is_none());
        assert!(run_input(&draft, &plan_draft, &reference(2), Action::Rebase, "merge").is_some());
        let closed = stack_of(vec![layer(1, "closed", false, Some("aaa")), layer(2, "open", false, Some("bbb"))]);
        let plan_closed = plan(&closed, 2, false);
        assert!(plan_closed.merge_disabled && plan_closed.rebase_disabled && plan_closed.merge_blocked);
        // A layer whose head is not known cannot be named, so nothing that reaches it can be asked for.
        let unknown = stack_of(vec![layer(1, "open", false, Some("aaa")), layer(2, "open", false, None)]);
        let plan_unknown = plan(&unknown, 1, false);
        assert!(!plan_unknown.merge_disabled, "a merge to layer 1 does not reach layer 2");
        assert!(plan_unknown.rebase_disabled);
        assert!(plan(&unknown, 2, false).merge_disabled);
        // Merging a layer that is itself merged or closed, one that is not in the stack, and a request in flight.
        let merged = stack_of(vec![layer(1, "merged", false, Some("aaa")), layer(2, "open", false, Some("bbb"))]);
        assert!(plan(&merged, 1, false).merge_disabled);
        assert!(plan(&merged, 99, false).merge_disabled && plan(&merged, 99, false).position == 0);
        assert!(plan(&open_stack(), 2, true).merge_disabled && plan(&open_stack(), 2, true).rebase_disabled);
        // Everything merged: nothing to rebase.
        let done = stack_of(vec![layer(1, "merged", false, Some("aaa"))]);
        assert!(plan(&done, 1, false).rebase_disabled);
    }

    #[test]
    fn the_stack_is_old_until_a_read_settles_and_says_so() {
        assert_eq!(view(true, true, false, None), View { fresh: true, notice: None });
        assert_eq!(view(true, false, true, None), View { fresh: false, notice: Some("Refreshing stack\u{2026} Showing saved data.") });
        assert_eq!(view(true, false, false, Some("offline")), View { fresh: false, notice: Some("Stack data may be stale. We couldn\u{2019}t refresh it.") });
        assert_eq!(view(true, true, true, None), View { fresh: false, notice: Some("Refreshing stack\u{2026} Showing saved data.") }, "a refresh in flight is not fresh");
        assert_eq!(view(false, true, false, None), View { fresh: true, notice: None }, "no stack, nothing to say");
        assert_eq!(view(false, false, false, Some("offline")).notice, None);
    }

    #[test]
    fn a_single_pull_request_waits_for_the_stack_lookup_where_the_host_has_stack_actions() {
        // (capability, stack, pending, error) -> permitted, the original's table.
        for (supports, has_stack, pending, error, allowed) in [
            (false, true, false, false, true),
            (false, false, true, false, true),
            (true, false, true, false, false),
            (true, false, false, true, false),
            (true, true, false, false, false),
            (true, false, false, false, true),
        ] {
            assert_eq!(single_merge_allowed(supports, has_stack, pending, error), allowed, "{supports} {has_stack} {pending} {error}");
        }
    }

    #[test]
    fn the_words_are_the_originals() {
        assert_eq!(confirmation(Action::Merge, 3, "main", "squash", 2), (
            "Merge 2 pull requests?".to_owned(),
            "Merge #3 and its unmerged layers below into main using squash. GitHub checks their rules before merging or queueing them and rebases the remaining stack after merging.".to_owned()
        ));
        assert_eq!(confirmation(Action::Rebase, 3, "main", "merge", 3).0, "Rebase 3 pull requests?");
        assert!(confirmation(Action::Rebase, 3, "main", "merge", 3).1.starts_with("Rebase the remote branches from bottom to top onto main."));
        assert_eq!(merge_tooltip(3, "main", 1), "Merge stack through #3 into main (1 pull request)");
        assert_eq!(merge_tooltip(3, "main", 2), "Merge stack through #3 into main (2 pull requests)");
        assert_eq!(success(Action::Merge), ("Stack merge request completed", Some("GitHub merged the stack or added it to its merge queue.")));
        assert_eq!(success(Action::Rebase), ("Stack rebased", None));
        assert_eq!(failure_text("Pull request operation runAction failed: The stack changed. Refresh it before trying again."), "The stack changed. Refresh it before trying again.");
        assert_eq!(failure_text("plain"), "plain");
        assert_eq!(failure_text("  "), "The stack operation did not complete.");
    }
}
