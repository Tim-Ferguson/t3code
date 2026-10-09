//! `pullRequests.list` and `pullRequests.listStats` (the listing half of
//! `PullRequestService.ts`). Every GitHub repository of the workspace on one host is read in a
//! single search; a further page carries on from a cursor that names, per repository, the
//! instant its last row was updated and the rows already sent at exactly that instant.
//! Line counts are not in the listing: they cost 40-60% of the search, so a page asks for them
//! once its rows are on screen.
use crate::{
    pull_request_decode::GitHubListItem,
    pull_request_github::{self as github, Reason, SEARCH_MAX_ROWS, SearchInput},
    pull_request_service::{Env, PrError, PrResult, PullRequestService, Supported, provider_detail, with_avatar},
};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, OnceLock},
    time::Duration,
};
use t3_contracts::*;

const DEFAULT_REPOSITORY_LIST_LIMIT: u32 = 99;
const REPOSITORY_SEARCH_CHUNK: usize = 100;
const LIST_CACHE_TTL: Duration = Duration::from_secs(30);
const LIST_STATS_CACHE_TTL: Duration = Duration::from_secs(60);
const RECENT_STATS_CAPACITY: usize = 2_048;

/// Where a repository carries on: the instant of its last row, how many rows were handed over,
/// and which of them were at exactly that instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListCursor {
    pub updated_before: String,
    pub delivered: u32,
    pub seen_at: Vec<u64>,
}

/// A continuation comes back from a client and has to be believed or refused on sight: everything
/// a host is given is either a timestamp of this shape or a number of this length.
pub fn parse_list_cursor(raw: &str) -> Option<ListCursor> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?(?:Z|[+-]\d{2}:\d{2}))\|(\d{1,9})\|(\d{1,9}(?:,\d{1,9})*)?$").unwrap()
    });
    let captures = pattern.captures(raw)?;
    Some(ListCursor {
        updated_before: captures[1].to_string(),
        delivered: captures[2].parse().ok()?,
        seen_at: captures.get(3).map(|seen| seen.as_str().split(',').filter_map(|number| number.parse().ok()).collect()).unwrap_or_default(),
    })
}

/// The same cursor against a boundary chosen for the whole slice (`listCursorAt`): every
/// repository in it is read up to the oldest row of the slice, including the ones that
/// contributed nothing — their rows are simply all older.
pub fn list_cursor_at(previous: Option<&ListCursor>, boundary: &str, fetched: &[&GitHubListItem], delivered_count: usize) -> String {
    let mut seen_at: Vec<u64> = previous.filter(|previous| previous.updated_before == boundary).map(|previous| previous.seen_at.clone()).unwrap_or_default();
    seen_at.extend(fetched.iter().filter(|item| item.updated_at == boundary).map(|item| item.number));
    format!(
        "{boundary}|{}|{}",
        previous.map_or(0, |previous| previous.delivered) as usize + delivered_count,
        seen_at.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
    )
}

/// The narrowings a row can be judged by from its own fields, applied here rather than trusted
/// to the host. `checks` is the host's alone: no listed row carries its check state.
pub fn matches_row_filters(item: &GitHubListItem, filters: Option<&PullRequestListFilters>, viewer: &str) -> bool {
    let Some(filters) = filters else { return true };
    let labels: std::collections::HashSet<String> = item.labels.iter().map(|label| label.name.trim().to_lowercase()).collect();
    let holds = |label: &TrimmedNonEmptyString| labels.contains(&label.as_str().trim().to_lowercase());
    let draft = filters.draft.is_none_or(|draft| item.is_draft == (draft == PullRequestDraftFilter::Only));
    let review = filters.review.is_none_or(|review| match review {
        PullRequestReviewFilter::None => item.review_decision.is_none(),
        PullRequestReviewFilter::Approved => item.review_decision == Some(PullRequestReviewDecision::Approved),
        PullRequestReviewFilter::ChangesRequested => item.review_decision == Some(PullRequestReviewDecision::ChangesRequested),
        PullRequestReviewFilter::ReviewRequired => item.review_decision == Some(PullRequestReviewDecision::ReviewRequired),
    });
    let wanted_labels = filters.labels.as_ref().is_none_or(|groups| groups.iter().all(|group| group.iter().any(holds)));
    let excluded = filters.excluded_labels.as_ref().is_none_or(|excluded| !excluded.iter().any(holds));
    let author = filters.author.as_ref().is_none_or(|author| {
        item.author.as_ref().map(|actor| actor.login.as_str().to_lowercase()) == Some(github::resolve_author_filter(author.as_str(), viewer).to_lowercase())
    });
    draft && review && wanted_labels && excluded && author
}

fn to_entry(project: &Supported, item: &GitHubListItem, viewer: &str, observed_at: i64) -> PullRequestListEntry {
    let viewer = viewer.to_lowercase();
    PullRequestListEntry {
        stack: item.stack.clone(),
        provider: project.kind,
        host: project.host.clone(),
        project_id: project.project.id.clone(),
        project_title: project.project.title.clone(),
        repository: project.repository.clone(),
        number: item.number,
        title: item.title.clone(),
        url: item.url.clone(),
        author: with_avatar(item.author.clone(), &project.host),
        head_branch: item.head_branch.clone(),
        base_branch: item.base_branch.clone(),
        state: item.state,
        is_draft: item.is_draft,
        mergeability: item.mergeability,
        additions: item.additions,
        deletions: item.deletions,
        created_at: item.created_at.clone(),
        updated_at: item.updated_at.clone(),
        observed_at: Some(observed_at),
        checks_state: item.checks_state,
        viewer_review_requested: item.author.as_ref().map(|author| author.login.as_str().to_lowercase()) != Some(viewer.clone())
            && item.review_request_logins.iter().any(|login| login.to_lowercase() == viewer),
        labels: item.labels.clone(),
        review_decision: item.review_decision,
    }
}

struct Batch {
    key: String,
    entries: Vec<PullRequestListEntry>,
    errors: Vec<PullRequestListProjectError>,
    truncated: bool,
    next_cursor: Option<String>,
}

impl PullRequestService {
    pub async fn list(self: &Arc<Self>, env: &Env, input: &PullRequestListInput) -> PrResult<PullRequestListResult> {
        let mut normalized = input.clone();
        if let Some(ids) = &mut normalized.project_ids {
            ids.sort();
        }
        let key = json!([self.listings_epoch(), normalized]).to_string();
        self.lists
            .get(key, Box::pin(self.list_uncached(env.clone(), input.clone())), |result| if result.is_ok() { LIST_CACHE_TTL } else { Duration::ZERO })
            .await
    }

    async fn list_uncached(&self, env: Env, input: PullRequestListInput) -> PrResult<PullRequestListResult> {
        let involvement = input.involvement.unwrap_or(PullRequestInvolvement::All);
        let mut continuation: Option<HashMap<String, ListCursor>> = None;
        if let Some(cursors) = &input.cursors {
            let mut decoded = HashMap::new();
            for (key, raw) in cursors {
                let cursor = parse_list_cursor(raw).ok_or_else(|| PrError::operation("list", "The list could not be carried on from where it left off."))?;
                decoded.insert(key.clone(), cursor);
            }
            continuation = Some(decoded);
        }
        let (projects, unimplemented) = self
            .workspace_projects(
                &env,
                |project| {
                    input.project_id.as_ref().is_none_or(|id| *id == project.id)
                        && input.project_ids.as_ref().is_none_or(|ids| ids.contains(&project.id))
                },
                input.host.as_ref().map(|host| host.as_str()),
            )
            .await;

        // Who is signed in, per host: two GitHub hosts are two accounts, and the wrong login
        // would misattribute every review request. The failure doubles as the answer to "is this
        // host set up", which is what the provider switcher shows.
        let mut hosts: Vec<(String, SourceControlProviderKind)> = vec![];
        for project in &projects {
            if !hosts.iter().any(|(host, _)| *host == project.host) {
                hosts.push((project.host.clone(), project.kind));
            }
        }
        let mut viewers = BTreeMap::new();
        let mut failures: Vec<(String, github::PrFailure)> = vec![];
        let mut providers = vec![];
        for (host, kind) in &hosts {
            let count = projects.iter().filter(|project| project.host == *host).count() as u64;
            let (configured, detail) = match self.viewer_of(&env, host).await {
                Ok(viewer) => {
                    viewers.insert(host.clone(), viewer);
                    (true, None)
                }
                Err(failure) => {
                    let detail = provider_detail(&failure, *kind);
                    failures.push((host.clone(), failure));
                    (false, Some(detail))
                }
            };
            providers.push(PullRequestProviderSummary { host: host.clone(), kind: *kind, searches_on_host: true, project_count: count, configured, detail });
        }
        for (host, (kind, count)) in &unimplemented {
            providers.push(PullRequestProviderSummary {
                host: host.clone(),
                kind: *kind,
                searches_on_host: false,
                project_count: *count,
                configured: false,
                detail: Some("This host cannot be browsed here yet.".into()),
            });
        }

        let selected: Vec<&Supported> = projects
            .iter()
            .filter(|project| continuation.as_ref().is_none_or(|cursors| cursors.contains_key(&project.cursor_key)))
            .collect();
        let readable: Vec<&Supported> = selected.iter().copied().filter(|project| viewers.contains_key(&project.host)).collect();
        let unreadable: Vec<PullRequestListProjectError> = selected
            .iter()
            .filter(|project| !viewers.contains_key(&project.host))
            .map(|project| PullRequestListProjectError {
                project_id: project.project.id.clone(),
                project_title: project.project.title.clone(),
                message: format!("{} could not be read.", project.repository),
            })
            .collect();
        if readable.is_empty() {
            let blocking = failures
                .iter()
                .filter(|(host, _)| selected.iter().any(|project| project.host == *host))
                .map(|(_, failure)| failure)
                .min_by_key(|failure| !matches!(failure.reason, Reason::MissingTool | Reason::Unauthenticated));
            if let Some(failure) = blocking {
                return Err(PrError::from_failure("list", failure.clone(), SourceControlProviderKind::Github));
            }
            return Ok(PullRequestListResult { viewers, providers, entries: vec![], errors: vec![], truncated: false, next_cursors: Default::default() });
        }

        let limit = input.limit.unwrap_or(DEFAULT_REPOSITORY_LIST_LIMIT);
        let cursor_of = |project: &Supported| continuation.as_ref().and_then(|cursors| cursors.get(&project.cursor_key)).cloned();
        let gh = crate::pull_request_service::github_for(&env);

        // One search per host and slice boundary, a hundred repositories at a time.
        let mut groups: Vec<((String, String), Vec<&Supported>)> = vec![];
        for project in &readable {
            let slice = (project.host.clone(), cursor_of(project).map(|cursor| cursor.updated_before).unwrap_or_default());
            match groups.iter_mut().find(|(key, _)| *key == slice) {
                Some((_, group)) => group.push(project),
                None => groups.push((slice, vec![project])),
            }
        }
        let mut batches: Vec<Batch> = vec![];
        for ((host, updated_before), group) in &groups {
            for chunk in group.chunks(REPOSITORY_SEARCH_CHUNK) {
                let first = chunk[0];
                let viewer = viewers[host].clone();
                let observed_at = self.now_ms();
                let repositories: Vec<String> = chunk.iter().map(|project| project.repository.clone()).collect();
                let query = github::search_query(&SearchInput {
                    repositories: &repositories,
                    state: input.state,
                    involvement,
                    viewer: &viewer,
                    query: input.query.as_ref().map(|query| query.as_str()),
                    updated_before: (!updated_before.is_empty()).then_some(updated_before.as_str()),
                    filters: input.filters.as_ref(),
                });
                let Some(query) = query else {
                    batches.extend(chunk.iter().map(|project| failed(project)));
                    continue;
                };
                // One extra row reveals that the host has more than the slice shows.
                let rows = (limit as usize + 1).min(SEARCH_MAX_ROWS);
                let page = match Box::pin(gh.search(host, &query, rows, first.host.eq_ignore_ascii_case("github.com"))).await {
                    Ok(page) => page,
                    Err(failure) => {
                        tracing::warn!(host=%host,detail=%failure.detail,"pull request search failed");
                        batches.extend(chunk.iter().map(|project| failed(project)));
                        continue;
                    }
                };
                let truncated = page.raw_count > limit as usize || page.has_next_page;
                let kept: Vec<&crate::pull_request_decode::SearchItem> = page.items.iter().take(limit as usize).collect();
                let boundary = kept.iter().map(|item| item.item.updated_at.as_str()).min().map(str::to_string);
                for project in chunk {
                    let fetched: Vec<&GitHubListItem> =
                        kept.iter().filter(|item| item.repository.trim().eq_ignore_ascii_case(project.repository.trim())).map(|item| &item.item).collect();
                    let cursor = cursor_of(project);
                    let items: Vec<&GitHubListItem> = match &cursor {
                        None => fetched.clone(),
                        Some(cursor) => fetched.iter().copied().filter(|item| item.updated_at != cursor.updated_before || !cursor.seen_at.contains(&item.number)).collect(),
                    };
                    batches.push(Batch {
                        key: project.cursor_key.clone(),
                        entries: items.iter().filter(|item| matches_row_filters(item, input.filters.as_ref(), &viewer)).map(|item| to_entry(project, item, &viewer, observed_at)).collect(),
                        errors: vec![],
                        truncated,
                        next_cursor: match (&boundary, truncated) {
                            (Some(boundary), true) => Some(list_cursor_at(cursor.as_ref(), boundary, &fetched, items.len())),
                            _ => None,
                        },
                    });
                }
            }
        }
        let mut entries: Vec<PullRequestListEntry> = batches.iter_mut().flat_map(|batch| std::mem::take(&mut batch.entries)).collect();
        entries.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(PullRequestListResult {
            viewers,
            providers,
            entries,
            errors: unreadable.into_iter().chain(batches.iter_mut().flat_map(|batch| std::mem::take(&mut batch.errors))).collect(),
            truncated: batches.iter().any(|batch| batch.truncated),
            next_cursors: batches.iter().filter_map(|batch| batch.next_cursor.clone().map(|cursor| (batch.key.clone(), cursor))).collect(),
        })
    }

    /// The line counts of rows a listing already handed over.
    pub async fn list_stats(&self, env: &Env, input: &PullRequestListStatsInput) -> PrResult<PullRequestListStatsResult> {
        let mut held = vec![];
        let mut missing: Vec<(String, crate::pull_request_service::Resolved)> = vec![];
        let now = self.now();
        for reference in &input.refs {
            // A reference no project can serve is left out rather than failing the page.
            let Ok(resolved) = self.require_project(env, reference).await.map(|project| crate::pull_request_service::Resolved {
                project,
                number: reference.number,
                input: reference.clone(),
                namespace: None,
            }) else {
                continue;
            };
            let key = json!([self.listings_epoch(), self.key(&resolved)]).to_string();
            let cached = self.recent_stats.lock().unwrap().get(&key).filter(|(at, _)| now.saturating_sub(*at) < LIST_STATS_CACHE_TTL).map(|(_, stat)| stat.clone());
            match cached {
                Some(stat) => held.push(stat),
                None => missing.push((key, resolved)),
            }
        }
        let mut by_host: Vec<(String, Vec<(String, crate::pull_request_service::Resolved)>)> = vec![];
        for entry in missing {
            let host = entry.1.project.host.clone();
            match by_host.iter_mut().find(|(known, _)| *known == host) {
                Some((_, group)) => group.push(entry),
                None => by_host.push((host, vec![entry])),
            }
        }
        let gh = crate::pull_request_service::github_for(env);
        let mut stats = held;
        for (host, group) in by_host {
            let asked: Vec<(String, u64)> = group.iter().map(|(_, resolved)| (resolved.project.repository.clone(), resolved.number)).collect();
            let found = Box::pin(gh.stats(&host, &asked)).await.map_err(|failure| PrError::from_failure("listStats", failure, SourceControlProviderKind::Github))?;
            for (key, resolved) in &group {
                let Some((_, _, additions, deletions)) = found.iter().find(|(repository, number, ..)| repository.eq_ignore_ascii_case(&resolved.project.repository) && *number == resolved.number) else {
                    continue;
                };
                let stat = PullRequestDiffStat {
                    project_id: resolved.project.project.id.clone(),
                    repository: resolved.project.repository.clone(),
                    number: resolved.number,
                    additions: *additions,
                    deletions: *deletions,
                };
                let mut recent = self.recent_stats.lock().unwrap();
                if recent.len() >= RECENT_STATS_CAPACITY && !recent.contains_key(key) {
                    if let Some(oldest) = recent.keys().next().cloned() {
                        recent.remove(&oldest);
                    }
                }
                recent.insert(key.clone(), (now, stat.clone()));
                stats.push(stat);
            }
        }
        Ok(PullRequestListStatsResult { stats })
    }
}

fn failed(project: &Supported) -> Batch {
    Batch {
        key: project.cursor_key.clone(),
        entries: vec![],
        errors: vec![PullRequestListProjectError {
            project_id: project.project.id.clone(),
            project_title: project.project.title.clone(),
            message: format!("{} could not be read.", project.repository),
        }],
        truncated: false,
        next_cursor: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn cursor_of(value: &Value) -> Option<ListCursor> {
        (!value.is_null()).then(|| ListCursor {
            updated_before: value["updatedBefore"].as_str().unwrap().into(),
            delivered: value["delivered"].as_u64().unwrap() as u32,
            seen_at: value["seenAt"].as_array().unwrap().iter().map(|n| n.as_u64().unwrap()).collect(),
        })
    }

    #[test]
    fn cursors_parse_and_advance_as_the_original_does() {
        let mut count = 0;
        for line in include_str!("../tests/fixtures/pull-request-list-cursors.jsonl").lines().filter(|line| !line.is_empty()) {
            let row: Value = serde_json::from_str(line).unwrap();
            match row["type"].as_str().unwrap() {
                "parse" => {
                    let actual = parse_list_cursor(row["raw"].as_str().unwrap()).map(|cursor| json!({"updatedBefore":cursor.updated_before,"delivered":cursor.delivered,"seenAt":cursor.seen_at}));
                    assert_eq!(actual.unwrap_or(Value::Null), row["result"], "{row}");
                }
                "at" => {
                    let items: Vec<GitHubListItem> = row["fetched"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|fetched| GitHubListItem {
                            stack: None,
                            author_id: None,
                            number: fetched["number"].as_u64().unwrap(),
                            title: String::new(),
                            url: String::new(),
                            author: None,
                            head_branch: String::new(),
                            base_branch: String::new(),
                            state: PullRequestState::Open,
                            is_draft: false,
                            mergeability: PullRequestMergeability::Unknown,
                            review_decision: None,
                            additions: 0,
                            deletions: 0,
                            created_at: String::new(),
                            updated_at: fetched["updatedAt"].as_str().unwrap().into(),
                            review_request_logins: vec![],
                            has_team_review_request: false,
                            labels: vec![],
                            checks_state: None,
                        })
                        .collect();
                    let refs: Vec<&GitHubListItem> = items.iter().collect();
                    let actual = list_cursor_at(cursor_of(&row["previous"]).as_ref(), row["boundary"].as_str().unwrap(), &refs, row["delivered"].as_u64().unwrap() as usize);
                    assert_eq!(actual, row["result"].as_str().unwrap(), "{row}");
                }
                other => panic!("unknown case {other}"),
            }
            count += 1;
        }
        assert_eq!(count, 46);
    }
}
