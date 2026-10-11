//! The repository identity a shell carries, and the frames that deliver it after the snapshot
//! (`ShellStream.ts`, `ws.ts` `enrichProjectShells`). A snapshot is built from what is known and is
//! never held up for a git probe; roots resolved later arrive as `snapshot` frames of the projects
//! alone, marked with the roots they resolved.
use crate::{persistence::Store, project_enrichment::ProjectEnrichment, source_control_host::Host, transport::ApiState};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};

/// The enrichment service of the server's store, started with the hosting-account refinement of the
/// server's settings.
pub fn service(state: &ApiState) -> Arc<ProjectEnrichment> {
    ProjectEnrichment::for_store(&state.store, || {
        Some(crate::repository_identity::forgejo_refine(Host::process_default().with_settings(state.settings.clone())))
    })
}

/// Sets each project's `repositoryIdentity` to what is resolved and queues the rest. Answers the
/// roots whose resolution has finished, in project order.
pub fn enrich_projects(enrichment: &ProjectEnrichment, projects: &mut [Value]) -> Vec<String> {
    let mut resolved = Vec::new();
    for project in projects {
        let Some(root) = project["workspaceRoot"].as_str().map(str::to_owned) else { continue };
        let known = enrichment.get_available(&root);
        project["repositoryIdentity"] = known.repository_identity.map_or(Value::Null, |identity| serde_json::to_value(identity).expect("identity"));
        if known.resolved {
            resolved.push(root);
        }
    }
    resolved
}

/// Enriches the `projects` of a snapshot document.
pub fn enrich_snapshot(enrichment: &ProjectEnrichment, snapshot: &mut Value) -> Vec<String> {
    match snapshot["projects"].as_array_mut() {
        Some(projects) => enrich_projects(enrichment, projects),
        None => Vec::new(),
    }
}

/// `shellStreamItemFromEnrichmentRefresh`: the projects whose roots resolved, in a snapshot with no
/// threads, marked with those roots (once each).
pub fn enrichment_frame(snapshot_sequence: u64, projects: Vec<Value>, roots: &[String]) -> Value {
    let mut unique = Vec::new();
    for root in roots {
        if !unique.contains(root) {
            unique.push(root.clone());
        }
    }
    let projects: Vec<Value> = projects.into_iter().filter(|project| project["workspaceRoot"].as_str().is_some_and(|root| unique.iter().any(|candidate| candidate == root))).collect();
    json!({"kind":"snapshot","snapshot":{"schemaVersion":2,"snapshotSequence":snapshot_sequence,"projects":projects,"threads":[],"archivedThreads":[]},"resolvedRepositoryIdentityRoots":unique})
}

/// The marked frame that follows a snapshot, or none when no root has resolved yet.
pub fn initial_enrichment_frame(snapshot: &Value, resolved: &[String]) -> Option<Value> {
    (!resolved.is_empty()).then(|| enrichment_frame(snapshot["snapshotSequence"].as_u64().unwrap_or(0), snapshot["projects"].as_array().cloned().unwrap_or_default(), resolved))
}

/// The current shells of the projects whose roots just resolved, each with the identity its root
/// resolved to; a project moved since is not named.
pub fn refresh_frame(store: &Store, changes: &[crate::project_enrichment::Change]) -> Result<Option<Value>, crate::persistence::StoreError> {
    let identities: std::collections::HashMap<&str, Value> = changes
        .iter()
        .map(|change| (change.workspace_root.as_str(), change.repository_identity.as_ref().map_or(Value::Null, |identity| serde_json::to_value(identity).expect("identity"))))
        .collect();
    let projects: Vec<Value> = crate::project::ProjectService::new(store.clone())
        .list()?
        .into_iter()
        .filter_map(|mut project| {
            let identity = identities.get(project["workspaceRoot"].as_str()?)?.clone();
            project["repositoryIdentity"] = identity;
            Some(project)
        })
        .collect();
    let roots: Vec<String> = changes.iter().map(|change| change.workspace_root.clone()).collect();
    Ok(Some(enrichment_frame(store.latest_sequence()?, projects, &roots)))
}

/// `dedupeShellEnrichment`: drops an enrichment frame that repeats the one before it on this
/// subscription (the same roots, the same projects), and forgets that one whenever a frame that
/// could change the answer goes by.
#[derive(Default)]
pub struct Dedupe {
    previous: Option<(Vec<Value>, HashSet<String>)>,
}

impl Dedupe {
    /// Whether to send this stream item.
    pub fn keep(&mut self, item: &Value) -> bool {
        match item["kind"].as_str() {
            Some("project.updated" | "project.removed") => {
                self.previous = None;
                true
            }
            Some("snapshot") => {
                let snapshot = &item["snapshot"];
                let roots = item["resolvedRepositoryIdentityRoots"].as_array();
                let has_threads = |key: &str| snapshot[key].as_array().is_some_and(|rows| !rows.is_empty());
                let Some(roots) = roots.filter(|_| !has_threads("threads") && !has_threads("archivedThreads")) else {
                    self.previous = None;
                    return true;
                };
                let roots: HashSet<String> = roots.iter().filter_map(|root| root.as_str().map(str::to_owned)).collect();
                let projects = snapshot["projects"].as_array().cloned().unwrap_or_default();
                if self.previous.as_ref().is_some_and(|(held, held_roots)| *held_roots == roots && *held == projects) {
                    return false;
                }
                self.previous = Some((projects, roots));
                true
            }
            _ => true,
        }
    }
}
