//! What follows a successful stacked action (port of `linkCreatedPullRequest.ts` and
//! `refreshPushedPullRequests.ts`): the pull request it left behind is linked to the thread it ran
//! beside, and what the server holds about pull requests is dropped after a push. Neither can
//! fail the action, which already succeeded and whose result is on its way to the client: a link
//! that cannot be made is logged and dropped.
use crate::{
    git_workflow_stacked::StackedInput,
    pull_request_service::{Env, PullRequestService},
    thread::ThreadService,
    thread_pull_requests::parse_change_request_url,
};
use chrono::Utc;
use serde_json::{Value, json};

/// The identity a stacked action's pull request should be linked under, or `None` when the action
/// left none behind. The host and repository come from the URL, which keeps the link host-level
/// even when the checkout's remote differs from where the pull request was opened (a fork, say);
/// the project's own remote is only asked when the URL is one that cannot be read
/// (`createdPullRequestKey`). `fallback` is that remote's `(host, repository)`.
pub fn created_pull_request_key(result: &Value, fallback: Option<(String, String)>) -> Option<Value> {
    let pr = &result["pr"];
    let status = pr["status"].as_str()?;
    let number = pr["number"].as_u64()?;
    let url = pr["url"].as_str().filter(|url| !url.is_empty())?;
    if status != "created" && status != "opened_existing" {
        return None;
    }
    let mut key = match parse_change_request_url(url) {
        Some(parsed) => parsed,
        None => {
            let (host, repository) = fallback?;
            json!({"host":host,"repository":repository.to_lowercase(),"number":number})
        }
    };
    key["url"] = json!(url);
    Some(key)
}

async fn thread_project_id(env: &Env, thread_id: &str) -> Option<String> {
    let (store, id) = (env.store.clone(), thread_id.to_string());
    let projection = tokio::task::spawn_blocking(move || ThreadService::new(store).projection(&id)).await.ok()?.ok()??;
    projection["thread"]["projectId"].as_str().map(str::to_string)
}

/// `linkCreatedPullRequest`: files the pull request under the thread as one it created. A
/// duplicate link is the thread saying it already knew.
pub async fn link_created_pull_request(env: &Env, thread_id: &str, result: &Value) {
    let Some(project_id) = thread_project_id(env, thread_id).await else { return };
    let service = PullRequestService::shared();
    // Only read when the URL gave no identity.
    let fallback = if result["pr"]["url"].as_str().and_then(parse_change_request_url).is_some() {
        None
    } else {
        let root = {
            let (store, id) = (env.store.clone(), project_id.clone());
            tokio::task::spawn_blocking(move || store.projections("project").ok().and_then(|rows| rows.into_iter().find(|row| row["projectId"] == id.as_str()).and_then(|row| row["workspaceRoot"].as_str().map(str::to_string)))).await.ok().flatten()
        };
        match root {
            Some(root) => service.identity(env, &root).await.and_then(|identity| Some((identity.host(), identity.repository_selector()?))),
            None => None,
        }
    };
    let Some(mut command) = created_pull_request_key(result, fallback) else { return };
    command["type"] = json!("thread.pull-request.link");
    command["commandId"] = json!(format!("server:pr-created-link:{}", uuid::Uuid::new_v4()));
    command["threadId"] = json!(thread_id);
    command["source"] = json!("created");
    let store = env.store.clone();
    match tokio::task::spawn_blocking(move || ThreadService::new(store).dispatch(&command, Utc::now())).await {
        Ok(Ok(_)) => {}
        _ => tracing::warn!(thread_id = %thread_id, "failed to link created pull request to thread"),
    }
}

/// `refreshPushedPullRequests`: after a push the pull requests of the thread's project, the named
/// project, or every project at this checkout may have moved.
pub async fn refresh_pushed_pull_requests(env: &Env, input: &StackedInput, result: &Value) {
    if result["push"]["status"] != "pushed" {
        return;
    }
    let service = PullRequestService::shared();
    if let Some(thread_id) = &input.thread_id {
        if let Some(project_id) = thread_project_id(env, thread_id).await {
            service.refresh_after_turn(&project_id);
            return;
        }
    }
    if let Some(project_id) = &input.project_id {
        service.refresh_after_turn(project_id);
        return;
    }
    let (store, cwd) = (env.store.clone(), input.cwd.clone());
    let projects: Vec<String> = tokio::task::spawn_blocking(move || {
        store
            .projections("project")
            .unwrap_or_default()
            .into_iter()
            .filter(|row| row["deletedAt"].is_null() && row["workspaceRoot"] == cwd.as_str())
            .filter_map(|row| row["projectId"].as_str().map(str::to_string))
            .collect()
    })
    .await
    .unwrap_or_default();
    for project_id in projects {
        service.refresh_after_turn(&project_id);
    }
}
