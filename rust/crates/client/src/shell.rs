use t3_contracts::{ProjectShell, ShellSnapshot, ShellStreamItem, ThreadLocation};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShellState {
    pub snapshot: Option<ShellSnapshot>,
    pub synchronized: bool,
}

fn retain_identity(prior: Option<&ProjectShell>, next: &mut ProjectShell) {
    if next
        .extra
        .get("repositoryIdentity")
        .is_none_or(|identity| identity.is_null())
    {
        if let Some(prior) = prior.filter(|prior| prior.workspace_root == next.workspace_root) {
            if let Some(identity) = prior
                .extra
                .get("repositoryIdentity")
                .filter(|identity| !identity.is_null())
            {
                next.extra
                    .insert("repositoryIdentity".into(), identity.clone());
            }
        }
    }
}

impl ShellState {
    /// Authoritative snapshots may move the cursor backwards after a server
    /// database replacement. Metadata enrichment never alters structure/cursor.
    pub fn apply(&mut self, item: ShellStreamItem) -> bool {
        match item {
            ShellStreamItem::Synchronized => {
                self.synchronized = true;
                false
            }
            ShellStreamItem::Snapshot {
                mut snapshot,
                resolved_repository_identity_roots,
            } => {
                if let Some(previous) = self.snapshot.as_mut() {
                    if let Some(roots) = resolved_repository_identity_roots {
                        let mut changed = false;
                        for project in &mut previous.projects {
                            if let Some(candidate) = snapshot.projects.iter().find(|candidate| {
                                candidate.id == project.id
                                    && candidate.workspace_root == project.workspace_root
                            }) {
                                let prior_identity = project.extra.get("repositoryIdentity");
                                let next_identity = candidate.extra.get("repositoryIdentity");
                                if roots
                                    .iter()
                                    .any(|root| root == project.workspace_root.as_str())
                                    || (prior_identity.is_none_or(|identity| identity.is_null())
                                        && next_identity
                                            .is_some_and(|identity| !identity.is_null()))
                                {
                                    if prior_identity != next_identity {
                                        changed = true;
                                        project.extra.insert(
                                            "repositoryIdentity".into(),
                                            next_identity
                                                .cloned()
                                                .unwrap_or(serde_json::Value::Null),
                                        );
                                    }
                                }
                            }
                        }
                        return changed;
                    }
                    for project in &mut snapshot.projects {
                        retain_identity(
                            previous
                                .projects
                                .iter()
                                .find(|prior| prior.id == project.id),
                            project,
                        );
                    }
                }
                let changed = self.snapshot.as_ref() != Some(&snapshot);
                self.snapshot = Some(snapshot);
                self.synchronized = false;
                changed
            }
            event => {
                let Some(snapshot) = self.snapshot.as_mut() else {
                    return false;
                };
                let sequence = match &event {
                    ShellStreamItem::ProjectUpdated { sequence, .. }
                    | ShellStreamItem::ProjectRemoved { sequence, .. }
                    | ShellStreamItem::ThreadUpdated { sequence, .. }
                    | ShellStreamItem::ThreadRemoved { sequence, .. } => *sequence,
                    _ => unreachable!(),
                };
                if sequence <= snapshot.snapshot_sequence {
                    return false;
                }
                snapshot.snapshot_sequence = sequence;
                match event {
                    ShellStreamItem::ProjectUpdated { mut project, .. } => {
                        let index = snapshot
                            .projects
                            .iter()
                            .position(|prior| prior.id == project.id);
                        retain_identity(index.map(|index| &snapshot.projects[index]), &mut project);
                        if let Some(index) = index {
                            if snapshot.projects[index] == project {
                                return false;
                            }
                            snapshot.projects[index] = project;
                        } else {
                            snapshot.projects.push(project);
                        }
                    }
                    ShellStreamItem::ProjectRemoved { project_id, .. } => {
                        snapshot.projects.retain(|project| project.id != project_id)
                    }
                    ShellStreamItem::ThreadUpdated {
                        thread, location, ..
                    } => {
                        snapshot
                            .archived_threads
                            .retain(|prior| prior.id != thread.id);
                        if location == ThreadLocation::Archive {
                            snapshot.threads.retain(|prior| prior.id != thread.id);
                        } else if let Some(index) = snapshot
                            .threads
                            .iter()
                            .position(|prior| prior.id == thread.id)
                        {
                            if snapshot.threads[index] == thread {
                                return false;
                            }
                            snapshot.threads[index] = thread;
                        } else {
                            snapshot.threads.push(thread);
                        }
                    }
                    ShellStreamItem::ThreadRemoved { thread_id, .. } => {
                        snapshot.threads.retain(|thread| thread.id != thread_id);
                        snapshot
                            .archived_threads
                            .retain(|thread| thread.id != thread_id);
                    }
                    _ => unreachable!(),
                }
                true
            }
        }
    }
}
