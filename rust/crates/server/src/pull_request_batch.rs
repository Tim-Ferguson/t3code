//! Summaries read together (port of `summaryResolver` and `buildPullRequestSummariesGraphQlQuery`).
//! The background sync asks for every linked pull request at once, and one request apiece is most
//! of what it spends. Reads that arrive within a short window on one host under one credential
//! share aliased GraphQL documents of twenty-five lookups. Whatever a batch cannot answer (a
//! selector it cannot write into a document, a pull request GitHub returned nothing for, a batch
//! that failed outright) is read on its own, unless the budget is paused: reading one at a time
//! would only spend what is being saved.
use crate::{
    pull_request_decode as decode,
    pull_request_github::{GitHub, PrFailure, Read, STAT_ALIASES_PER_REQUEST, Target},
    pull_request_queries as queries,
    source_control_github_api::{self as api, GitHubApiError, GraphQlInput, PinnedGitHubCredential},
};
use futures_util::{StreamExt, future::BoxFuture};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::sync::oneshot;

/// How long a read waits for company: each one reaches the batch after its own cache check, so
/// a batch needs a moment longer than one scheduler tick to gather them.
const BATCH_WINDOW: Duration = Duration::from_millis(10);
const SINGLE_READ_CONCURRENCY: usize = 4;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn is_repository_part(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// The document that reads each lookup under an alias of its own, or `None` when any of them
/// cannot be written into one safely (`buildPullRequestSummariesGraphQlQuery`).
pub fn build_summaries_query(lookups: &[(&str, u64)], include_stacks: bool) -> Option<String> {
    if lookups.is_empty() {
        return None;
    }
    let fields = if include_stacks { queries::SUMMARY_SELECTION_WITH_STACKS } else { queries::SUMMARY_SELECTION };
    let mut selections = vec![];
    for (index, (repository, number)) in lookups.iter().enumerate() {
        let parts: Vec<&str> = repository.trim().split('/').collect();
        let [owner, name] = parts[..] else { return None };
        if !is_repository_part(owner) || !is_repository_part(name) || *number == 0 || *number > MAX_SAFE_INTEGER {
            return None;
        }
        selections.push(format!("  s{index}: repository(owner: \"{owner}\", name: \"{name}\") {{ pullRequest(number: {number}) {{ {fields} }} }}"));
    }
    Some(format!("query PullRequestSummaries {{\n{}\n}}", selections.join("\n")))
}

struct Entry {
    target: Target,
    reply: oneshot::Sender<Read<decode::GitHubSummary>>,
}

/// Reads waiting for their window to close, and what they must run under: the credential and
/// rate-limit scope they were asked under (which is also what groups them), and whether the first
/// of them was asked for by a person.
struct Group {
    gh: GitHub,
    pinned: Option<PinnedGitHubCredential>,
    scope: String,
    interactive: bool,
    entries: Vec<Entry>,
}

fn pending() -> &'static Mutex<HashMap<String, Group>> {
    static PENDING: OnceLock<Mutex<HashMap<String, Group>>> = OnceLock::new();
    PENDING.get_or_init(Mutex::default)
}

/// One pull request's summary, shared with whatever else is asked for in the same moment.
pub async fn read(gh: &GitHub, target: &Target) -> Read<decode::GitHubSummary> {
    let pinned = api::current_pinned();
    let scope = api::current_credential_scope();
    let key = json!([target.host.to_lowercase(), pinned.as_ref().map(|pinned| pinned.credential_fingerprint.clone()), scope]).to_string();
    let (reply, answer) = oneshot::channel();
    let first = {
        let mut waiting = pending().lock().unwrap();
        let first = !waiting.contains_key(&key);
        waiting
            .entry(key.clone())
            .or_insert_with(|| Group { gh: gh.clone(), pinned, scope, interactive: api::allows_reserve(), entries: vec![] })
            .entries
            .push(Entry { target: target.clone(), reply });
        first
    };
    if first {
        tokio::spawn(async move {
            tokio::time::sleep(BATCH_WINDOW).await;
            let group = pending().lock().unwrap().remove(&key);
            if let Some(group) = group {
                run(group).await;
            }
        });
    }
    answer.await.unwrap_or_else(|_| Err(PrFailure::failed("The pull request summary read was abandoned.")))
}

/// Runs a closed window's reads under what they were asked under.
async fn run(group: Group) {
    let Group { gh, pinned, scope, interactive, entries } = group;
    let mut chunks: Vec<Vec<Entry>> = vec![];
    let mut entries = entries;
    while entries.len() > STAT_ALIASES_PER_REQUEST {
        let rest = entries.split_off(STAT_ALIASES_PER_REQUEST);
        chunks.push(std::mem::replace(&mut entries, rest));
    }
    chunks.push(entries);
    for chunk in chunks {
        let work: BoxFuture<'static, ()> = Box::pin(resolve(gh.clone(), chunk));
        let work: BoxFuture<'static, ()> = if interactive { Box::pin(api::with_allow_reserve(work)) } else { work };
        let work: BoxFuture<'static, ()> = Box::pin(api::with_credential_scope(scope.clone(), work));
        match pinned.clone() {
            Some(pinned) => api::with_pinned_credential(pinned, work).await,
            None => work.await,
        }
    }
}

async fn resolve(gh: GitHub, entries: Vec<Entry>) {
    let host = entries[0].target.host.clone();
    let batchable: Vec<usize> = (0..entries.len()).filter(|&index| build_summaries_query(&[(&entries[index].target.repository, entries[index].target.number)], false).is_some()).collect();
    let lookups: Vec<(&str, u64)> = batchable.iter().map(|&index| (entries[index].target.repository.as_str(), entries[index].target.number)).collect();
    let github_com = host.eq_ignore_ascii_case("github.com");
    let mut answers: HashMap<usize, decode::GitHubSummary> = HashMap::new();
    if let Some(query) = build_summaries_query(&lookups, github_com) {
        let sent = Box::pin(api::graphql(
            gh.host(),
            GraphQlInput { host: host.clone(), operation: "getPullRequestSummary".into(), query, variables: None, allow_reserve: None, max_response_bytes: None },
        ))
        .await;
        match sent {
            // A paused budget fails every read: asking one at a time would only spend what is being saved.
            Err(GitHubApiError::Paused(paused)) => {
                let failure = PrFailure::from(GitHubApiError::Paused(paused));
                for entry in entries {
                    let _ = entry.reply.send(Err(failure.clone()));
                }
                return;
            }
            // A GraphQL error anywhere fails the whole document, so every read is left to its own.
            Err(error) => tracing::debug!(error = %error, "batched pull request summary read failed"),
            Ok(raw) => {
                if let Ok(summaries) = decode::decode_summaries(raw.trim()) {
                    for (alias, summary) in summaries {
                        if let Some(&index) = batchable.get(alias) {
                            answers.insert(index, summary);
                        }
                    }
                }
            }
        }
    }
    let mut unanswered = vec![];
    for (index, entry) in entries.into_iter().enumerate() {
        match answers.remove(&index) {
            Some(summary) => {
                let _ = entry.reply.send(Ok(summary));
            }
            None => unanswered.push(entry),
        }
    }
    futures_util::stream::iter(unanswered)
        .for_each_concurrent(SINGLE_READ_CONCURRENCY, |entry| {
            let gh = gh.clone();
            async move {
                let single = Box::pin(gh.summary_single(&entry.target)).await;
                let _ = entry.reply.send(single);
            }
        })
        .await;
}
