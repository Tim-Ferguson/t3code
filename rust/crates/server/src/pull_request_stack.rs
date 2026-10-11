//! The native stack scope of pull request actions on GitHub: merge the stack up to a layer, or rebase it
//! (`githubStackActions.ts` and `githubStackRebase.ts`, which `runPullRequestAction` hands any action that carries a
//! `stackNumber`). The reader names the heads it reviewed; every open layer must still be at exactly that head or nothing is
//! sent. A merge is one asynchronous request GitHub finishes by itself and this polls; a rebase has no endpoint, so it works in a
//! scratch clone under the temporary directory (never the environment's checkout) and pushes each layer with a lease on the head
//! that was reviewed.
use crate::{
    pull_request_actions::RunAction,
    pull_request_decode as decode,
    pull_request_github::{GitHub, PrFailure, Read, Target},
    source_control_github_api::{self as api, GraphQlInput, Method, RestInput},
};
use base64::Engine;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use t3_contracts::{PullRequestAction, PullRequestMergeMethod, PullRequestStackHead, PullRequestState};
use t3_vcs::process::VcsProcessInput;

/// What a stack action adds to the run: the stack it was read from and the heads the reader reviewed.
#[derive(Debug, Clone)]
pub struct StackRun {
    pub stack_number: u64,
    pub expected_heads: Option<Vec<PullRequestStackHead>>,
}

/// One open layer of a stack, bottom to top, with the head the reader reviewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadeLayer {
    pub number: u64,
    pub head_branch: String,
    pub head_sha: String,
}

/// How a stack action ends without having done what was asked (`GitHubStack*Error`); the text is the original's `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackError {
    Changed { number: u64, completed: u64 },
    Unsupported,
    ResponseInvalid,
    MergeRejected,
    MergePending,
    Permission,
    /// A layer's rebase hit a conflict; the layers below it are already rebased on GitHub.
    RebaseConflict { number: u64, completed: u64 },
    /// Git failed for a reason other than a conflict: the fetch, or a push GitHub refused.
    RebaseGit { step: String, number: u64, completed: u64 },
    /// The cascade could not even begin (no credential, no scratch directory).
    Other(PrFailure),
}

impl StackError {
    pub fn message(&self) -> String {
        match self {
            Self::Changed { number, completed } if *completed > 0 => format!("The stack changed at PR #{number} after {completed} layers. Earlier updates remain on GitHub. Refresh it before trying again."),
            Self::Changed { .. } => "The stack changed. Refresh it before trying again.".into(),
            Self::Unsupported => "This operation is not supported for this stack.".into(),
            Self::ResponseInvalid => "GitHub returned an unreadable stack operation response.".into(),
            Self::MergeRejected => "GitHub refused the stack merge. Check the stack's branch rules and merge requirements.".into(),
            Self::MergePending => "The merge is still running on GitHub. Check its status there before submitting another request.".into(),
            Self::Permission => "You cannot update every branch in this stack. Check write access and fork maintainer permissions before retrying.".into(),
            // `GitHubStackRebaseFailedError` shows the cause's own sentence: it already says which layer and what to do.
            Self::RebaseConflict { number, completed } => format!("PR #{number} conflicts with the layer below it. {completed} layers were rebased; resolve #{number} with `gh stack rebase` and push."),
            Self::RebaseGit { step, number, completed } => format!("Stack rebase stopped at PR #{number} while {step}. {completed} layers were rebased."),
            Self::Other(failure) => failure.detail.clone(),
        }
    }

    fn failure(self) -> PrFailure {
        match self {
            Self::Other(failure) => failure,
            other => PrFailure::failed(other.message()),
        }
    }
}

/// A merge request's answer: `status` and the identity of the job when it is still running.
struct MergeAnswer {
    status: String,
    uuid: Option<String>,
}

fn decode_merge_answer(raw: &str) -> Result<MergeAnswer, StackError> {
    let value: Value = serde_json::from_str(raw.trim()).map_err(|_| StackError::ResponseInvalid)?;
    let status = value["status"].as_str().filter(|status| matches!(*status, "pending" | "merged" | "enqueued" | "failed")).ok_or(StackError::ResponseInvalid)?;
    if !value["details"].is_object() {
        return Err(StackError::ResponseInvalid);
    }
    Ok(MergeAnswer { status: status.into(), uuid: value["details"]["uuid"].as_str().map(str::to_owned) })
}

/// The access the viewer has to each open layer's head, from the aliased GraphQL read, or none when it is unreadable.
fn branch_access_allows(raw: &str, layers: &[&t3_contracts::PullRequestStackLayer]) -> Result<bool, StackError> {
    let value: Value = serde_json::from_str(raw.trim()).map_err(|_| StackError::ResponseInvalid)?;
    let repository = value["data"].get("repository").ok_or(StackError::ResponseInvalid)?;
    for layer in layers {
        let pr = if repository.is_object() { &repository[format!("pr{}", layer.number)] } else { &Value::Null };
        if !pr.is_object() || !pr["headRepository"].is_object() {
            return Ok(false);
        }
        let can_modify = pr["maintainerCanModify"].as_bool().ok_or(StackError::ResponseInvalid)?;
        let permission = pr["headRepository"]["viewerPermission"].as_str().unwrap_or("");
        if !can_modify && !matches!(permission, "ADMIN" | "MAINTAIN" | "WRITE") {
            return Ok(false);
        }
    }
    Ok(true)
}

impl GitHub {
    /// `runGitHubStackAction`.
    pub async fn run_stack_action(&self, target: &Target, run: &RunAction, stack_run: &StackRun) -> Read<()> {
        self.stack_action(target, run, stack_run).await.map_err(StackError::failure)
    }

    async fn stack_action(&self, target: &Target, run: &RunAction, stack_run: &StackRun) -> Result<(), StackError> {
        let other = |failure: crate::source_control_github_api::GitHubApiError| StackError::Other(failure.into());
        if run.action != PullRequestAction::Merge && run.action != PullRequestAction::UpdateBranch {
            return Err(StackError::Unsupported);
        }
        let endpoint = format!("repos/{}", target.repository);
        let read = api::rest(self.host(), RestInput { host: target.host.clone(), operation: "runGitHubStackAction".into(), path: format!("{endpoint}/stacks?pull_request={}", target.number), ..Default::default() }).await.map_err(other)?;
        let stack = decode::decode_stacks(read.body.trim()).map_err(|_| StackError::ResponseInvalid)?;
        let changed = || StackError::Changed { number: target.number, completed: 0 };
        let Some(stack) = stack.filter(|stack| stack.number == stack_run.stack_number) else { return Err(changed()) };
        let Some(index) = stack.layers.iter().position(|layer| layer.number == target.number) else { return Err(changed()) };
        let target_layer = &stack.layers[index];
        if run.action == PullRequestAction::UpdateBranch && index != stack.layers.len() - 1 {
            return Err(changed());
        }
        let affected: &[t3_contracts::PullRequestStackLayer] = if run.action == PullRequestAction::Merge { &stack.layers[..=index] } else { &stack.layers };
        let open: Vec<&t3_contracts::PullRequestStackLayer> = affected.iter().filter(|layer| layer.state != PullRequestState::Merged).collect();
        if run.action == PullRequestAction::Merge && target_layer.state != PullRequestState::Open {
            return Err(StackError::Unsupported);
        }
        let reviewed = stack_run.expected_heads.as_deref();
        let same_heads = reviewed.is_some_and(|reviewed| {
            reviewed.len() == open.len()
                && reviewed.iter().map(|layer| layer.number).collect::<std::collections::HashSet<_>>().len() == open.len()
                && open.iter().all(|layer| layer.head_sha.as_deref().is_some_and(|sha| !sha.is_empty() && reviewed.iter().any(|expected| expected.number == layer.number && expected.head_sha == sha)))
        });
        if !same_heads {
            return Err(changed());
        }
        if open.is_empty() || open.iter().any(|layer| layer.state != PullRequestState::Open) {
            return Err(StackError::Unsupported);
        }
        if run.action == PullRequestAction::UpdateBranch {
            let (owner, name) = target.repository.split_once('/').unwrap_or((&target.repository, ""));
            let selection: Vec<String> = open.iter().map(|layer| format!("pr{0}:pullRequest(number:{0}){{headRepository{{viewerPermission}} maintainerCanModify}}", layer.number)).collect();
            let query = format!("query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{{}}}}}", selection.join(" "));
            let permissions = api::graphql(self.host(), GraphQlInput { host: target.host.clone(), operation: "runGitHubStackAction".into(), query, variables: Some(json!({"owner":owner,"name":name})), allow_reserve: Some(true), ..Default::default() }).await.map_err(other)?;
            // `viewerCanUpdateBranch` is false for an already-current layer, even if rebasing its parent will make it stale, so
            // branch write access is checked separately before any layer is touched.
            if !branch_access_allows(&permissions, &open)? {
                return Err(StackError::Permission);
            }
            // GitHub's own "Rebase stack" has no API, and its per-PR "update branch" replays the old copy of every lower layer
            // into the one above it. The cascade moves each layer's own commits.
            let layers: Vec<CascadeLayer> = open.iter().map(|layer| CascadeLayer { number: layer.number, head_branch: layer.head_branch.clone(), head_sha: layer.head_sha.clone().unwrap_or_default() }).collect();
            return cascade_rebase_stack(self, &target.host, &target.repository, &stack.base, &layers).await.map(|_| ());
        }
        if open.iter().any(|layer| layer.is_draft == Some(true)) {
            return Err(StackError::Unsupported);
        }
        let method = match run.merge_method {
            Some(PullRequestMergeMethod::Squash) => "squash",
            Some(PullRequestMergeMethod::Rebase) => "rebase",
            _ => "merge",
        };
        let request = api::rest(
            self.host(),
            RestInput {
                host: target.host.clone(),
                operation: "runGitHubStackAction".into(),
                method: Method::Put,
                path: format!("{endpoint}/pulls/{}/merge-async", target.number),
                body: Some(json!({"merge_method":method,"merge_action":"default","sha":target_layer.head_sha})),
                ..Default::default()
            },
        )
        .await
        .map_err(other)?;
        let mut answer = decode_merge_answer(&request.body)?;
        let unit = self.host().shared.stack_poll_unit;
        let deadline = tokio::time::Instant::now() + unit * 300;
        let mut attempt = 0u32;
        while answer.status == "pending" && tokio::time::Instant::now() < deadline {
            let uuid = answer.uuid.clone().ok_or(StackError::ResponseInvalid)?;
            tokio::time::sleep(unit * (1u32 << attempt.min(4)).min(10)).await;
            let poll = api::rest(self.host(), RestInput { host: target.host.clone(), operation: "runGitHubStackAction".into(), path: format!("{endpoint}/pulls/{}/merge-async/{}", target.number, encode_uri_component(&uuid)), ..Default::default() }).await.map_err(other)?;
            answer = decode_merge_answer(&poll.body)?;
            attempt += 1;
        }
        match answer.status.as_str() {
            "pending" => Err(StackError::MergePending),
            "failed" => Err(StackError::MergeRejected),
            _ => Ok(()),
        }
    }
}

fn encode_uri_component(value: &str) -> String {
    crate::provider_maintenance::encode_uri_component(value)
}

/// Removes the scratch clone when the rebase ends, however it ends.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// GitHub's own "Rebase stack": every open layer, bottom to top, onto the new head of the layer below it (the bottom one onto the
/// stack's base). Each layer moves only its own commits (`git rebase --onto <new parent> <old parent>`), so a rebased lower layer is
/// never replayed into the one above it. Works in a throwaway clone, so the environment's checkout never moves, and pushes each
/// layer with a lease on the head that was reviewed, so a push that landed meanwhile is refused rather than overwritten.
pub(crate) async fn cascade_rebase_stack(github: &GitHub, host: &str, repository: &str, base: &str, layers: &[CascadeLayer]) -> Result<usize, StackError> {
    let Some(first) = layers.first() else { return Ok(0) };
    let failed = |step: &str, number: u64, completed: usize| StackError::RebaseGit { step: step.into(), number, completed: completed as u64 };
    let (token, _) = api::credential(github.host(), host).await.map_err(|error| StackError::Other(error.into()))?;
    let directory = std::env::temp_dir().join(format!("t3-stack-rebase-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).map_err(|_| failed("preparing", first.number, 0))?;
    let scratch = Scratch(directory);
    let remote = match &github.host().shared.git_remote {
        Some(resolver) => resolver(host, repository),
        None => format!("https://{}/{repository}.git", host.trim().to_lowercase()),
    };
    // The token rides in a header for this process only: never in the URL, argv, or git config.
    let authorization = format!("AUTHORIZATION: basic {}", base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{}", token.expose())));
    let env: Vec<(String, Option<String>)> = [
        ("GIT_TERMINAL_PROMPT", "0".to_owned()),
        ("GIT_CONFIG_COUNT", "3".to_owned()),
        ("GIT_CONFIG_KEY_0", format!("http.{remote}.extraheader")),
        ("GIT_CONFIG_VALUE_0", authorization),
        ("GIT_CONFIG_KEY_1", "user.name".to_owned()),
        ("GIT_CONFIG_VALUE_1", "T3 Code".to_owned()),
        ("GIT_CONFIG_KEY_2", "user.email".to_owned()),
        ("GIT_CONFIG_VALUE_2", "noreply@t3.codes".to_owned()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), Some(value)))
    // The configuration files the host was told to read are the ones the scratch clone reads.
    .chain(["GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM", "GIT_CONFIG_NOSYSTEM"].into_iter().filter_map(|name| github.host().env().get(name).map(|value| (name.to_owned(), Some(value.to_owned())))))
    .collect();
    let git = |args: Vec<String>, allow_non_zero_exit: bool| {
        let mut input = VcsProcessInput::new("cascadeRebaseStack", "git", args, scratch.0.clone());
        input.env = env.clone();
        input.timeout = Some(Duration::from_secs(120));
        input.allow_non_zero_exit = allow_non_zero_exit;
        let process = github.host().process().clone();
        async move { process.run(input).await }
    };
    let strings = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    git(strings(&["init", "--quiet"]), false).await.map_err(|_| failed("preparing", first.number, 0))?;
    git(strings(&["remote", "add", "origin", &remote]), false).await.map_err(|_| failed("preparing", first.number, 0))?;
    let mut fetch = strings(&["fetch", "--quiet", "--no-tags", "origin", &format!("+refs/heads/{base}:refs/remotes/origin/{base}")]);
    fetch.extend(layers.iter().map(|layer| format!("+refs/heads/{0}:refs/remotes/origin/{0}", layer.head_branch)));
    git(fetch, false).await.map_err(|_| failed("fetching", first.number, 0))?;

    let mut parent_old = format!("origin/{base}");
    let mut parent_new = parent_old.clone();
    for (index, layer) in layers.iter().enumerate() {
        // The bottom layer's old parent is where it forked from the base; above that it is the reviewed head of the layer below,
        // which is exactly the commits this layer must not replay.
        let upstream = if index == 0 {
            git(strings(&["merge-base", &parent_old, &layer.head_sha]), false).await.map_err(|_| failed("reading the fork point", layer.number, index))?.stdout.trim().to_owned()
        } else {
            parent_old.clone()
        };
        git(strings(&["checkout", "--quiet", "--detach", &layer.head_sha]), false).await.map_err(|_| failed("checking out", layer.number, index))?;
        let rebase = git(strings(&["rebase", "--quiet", "--onto", &parent_new, &upstream]), true).await.map_err(|_| failed("rebasing", layer.number, index))?;
        if rebase.exit_code != 0 {
            let _ = git(strings(&["rebase", "--abort"]), true).await;
            return Err(StackError::RebaseConflict { number: layer.number, completed: index as u64 });
        }
        let rebased = git(strings(&["rev-parse", "HEAD"]), false).await.map_err(|_| failed("rebasing", layer.number, index))?.stdout.trim().to_owned();
        if rebased != layer.head_sha {
            git(
                strings(&["push", "--quiet", &format!("--force-with-lease=refs/heads/{}:{}", layer.head_branch, layer.head_sha), "origin", &format!("{rebased}:refs/heads/{}", layer.head_branch)]),
                false,
            )
            .await
            .map_err(|_| failed("pushing", layer.number, index))?;
        }
        parent_old = layer.head_sha.clone();
        parent_new = rebased;
    }
    drop(scratch);
    Ok(layers.len())
}
