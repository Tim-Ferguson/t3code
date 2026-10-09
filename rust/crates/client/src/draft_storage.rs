//! Lossless storage boundary while the source composer UI is being ported.
//! Source bytes are read-only; changes and acknowledgements live in a sidecar.
use crate::drafts::{SOURCE_VERSION, recover_state};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftTarget {
    pub environment: String,
    pub kind: DraftKind,
    pub local_id: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DraftKind {
    Thread,
    Project,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftChoices {
    pub model_selection: Option<t3_contracts::ModelSelection>,
    pub runtime_mode: Option<t3_contracts::RuntimeMode>,
    pub environment_mode: Option<t3_contracts::ThreadEnvMode>,
    pub base_ref: String,
    /// An existing worktree the thread starts in, instead of a new one (with `environment_mode`
    /// `worktree`), as the original's draft context carries it.
    #[serde(default)]
    pub worktree_path: Option<String>,
}
impl DraftTarget {
    pub fn thread(environment: impl Into<String>, thread: impl Into<String>) -> Self {
        Self {
            environment: environment.into(),
            kind: DraftKind::Thread,
            local_id: thread.into(),
        }
    }
    pub fn project(environment: impl Into<String>, project: impl Into<String>) -> Self {
        Self {
            environment: environment.into(),
            kind: DraftKind::Project,
            local_id: project.into(),
        }
    }
    fn valid(&self) -> bool {
        !self.environment.is_empty() && !self.local_id.is_empty()
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftChanges {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<Value>,
    /// The attachment list as last edited here. `None` defers to the original draft,
    /// so an intentionally emptied list is `Some(vec![])`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<crate::attachments::Persisted>>,
    #[serde(default)]
    pub acknowledged: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Sidecar {
    version: u64,
    entries: Vec<(DraftTarget, DraftChanges)>,
    forgotten_environments: BTreeSet<String>,
}
#[derive(Debug)]
pub enum SavedAttachments<'a> {
    None,
    Edited(&'a [crate::attachments::Persisted]),
    /// The original draft record, which still carries image data URLs.
    Original(&'a Value),
}
#[derive(Debug, Default)]
pub struct DraftStorage {
    /// Kept verbatim even when decoding/migration fails or the version is newer.
    pub source_bytes: Option<Rc<str>>,
    pub source_state: Option<Rc<Value>>,
    pub sidecar_bytes: Option<Rc<str>>,
    pub recovery_error: Option<String>,
    changes: BTreeMap<DraftTarget, DraftChanges>,
    forgotten_environments: BTreeSet<String>,
    revision: u64,
    saved_revision: u64,
    /// Invalid/newer sidecars must be explicitly retried/replaced, never overwritten
    /// just because another field caused a render or a connection succeeded.
    pub sidecar_error: Option<String>,
}
impl DraftStorage {
    pub fn targets(&self) -> BTreeSet<DraftTarget> {
        let mut targets: BTreeSet<_> = self.changes.keys().cloned().collect();
        if let Some(state) = self.source_state.as_deref() {
            for (key, _) in state["draftsByThreadKey"].as_object().into_iter().flatten() {
                if let Some((environment, thread)) = crate::drafts::parse_scoped_key(key) {
                    targets.insert(DraftTarget::thread(environment, thread));
                }
            }
            for session in state["draftThreadsByThreadKey"]
                .as_object()
                .into_iter()
                .flat_map(|object| object.values())
            {
                if let (Some(environment), Some(project)) = (
                    session["environmentId"].as_str(),
                    session["projectId"].as_str(),
                ) {
                    targets.insert(DraftTarget::project(environment, project));
                }
            }
        }
        targets
    }
    pub fn hydrate(&mut self, source: Option<String>, sidecar: Option<String>, now: &str) {
        self.source_bytes = source.as_deref().map(Rc::from);
        self.sidecar_bytes = sidecar.as_deref().map(Rc::from);
        self.source_state = None;
        self.recovery_error = None;
        if let Some(source) = source {
            match decode_source(&source, now) {
                Ok(state) => self.source_state = Some(Rc::new(state)),
                Err(error) => self.recovery_error = Some(error),
            }
        }
        // Early edits (including an intentional empty prompt) win over disk.
        if let Some(sidecar) = sidecar {
            match decode_sidecar(&sidecar) {
                Ok(saved) => {
                    for environment in saved.forgotten_environments {
                        self.forgotten_environments.insert(environment);
                    }
                    for (target, changes) in saved.entries {
                        if self.forgotten_environments.contains(&target.environment) {
                            continue;
                        }
                        match self.changes.entry(target) {
                            std::collections::btree_map::Entry::Vacant(entry) => {
                                entry.insert(changes);
                            }
                            std::collections::btree_map::Entry::Occupied(mut entry) => {
                                let early = entry.get_mut();
                                if !early.acknowledged {
                                    if early.prompt.is_none() {
                                        early.prompt = changes.prompt;
                                    }
                                    if early.choices.is_none() {
                                        early.choices = changes.choices;
                                    }
                                    if early.attachments.is_none() {
                                        early.attachments = changes.attachments;
                                    }
                                    early.acknowledged = changes.acknowledged;
                                }
                            }
                        }
                    }
                    self.sidecar_error = None;
                }
                Err(error) => self.sidecar_error = Some(error),
            }
        } else {
            self.sidecar_error = None;
        }
    }
    pub fn edit_prompt(&mut self, target: DraftTarget, prompt: String) {
        self.changes.entry(target).or_default().prompt = Some(prompt);
        self.revision += 1;
    }
    pub fn edit_choices(&mut self, target: DraftTarget, choices: Value) {
        self.changes.entry(target).or_default().choices = Some(choices);
        self.revision += 1;
    }
    pub fn edit_attachments(
        &mut self,
        target: DraftTarget,
        attachments: Vec<crate::attachments::Persisted>,
    ) {
        self.changes.entry(target).or_default().attachments = Some(attachments);
        self.revision += 1;
    }
    pub fn acknowledge(&mut self, target: DraftTarget) {
        self.changes.insert(
            target,
            DraftChanges {
                acknowledged: true,
                ..Default::default()
            },
        );
        self.revision += 1;
    }
    /// Sending content leaves the composer model/runtime choices intact, as in
    /// the source clearComposerContent operation. Promotion clears all choices.
    pub fn acknowledge_content(&mut self, target: DraftTarget) {
        let changes = self.changes.entry(target).or_default();
        changes.prompt = None;
        changes.attachments = None;
        changes.acknowledged = true;
        self.revision += 1;
    }
    pub fn forget(&mut self, environment: &str) {
        self.forgotten_environments.insert(environment.into());
        self.changes
            .retain(|target, _| target.environment != environment);
        self.revision += 1;
    }
    pub fn changes(&self, target: &DraftTarget) -> Option<&DraftChanges> {
        self.changes.get(target)
    }
    pub fn recovered(&self, target: &DraftTarget) -> Option<&Value> {
        if self.forgotten_environments.contains(&target.environment)
            || self
                .changes
                .get(target)
                .is_some_and(|changes| changes.acknowledged)
        {
            return None;
        }
        let state = self.source_state.as_deref()?;
        let key = match target.kind {
            DraftKind::Thread => format!("{}:{}", target.environment, target.local_id),
            DraftKind::Project => {
                // Match concrete environment-local project identity, not only a
                // workspace-path logical alias from a different destination.
                let sessions = state["draftThreadsByThreadKey"].as_object()?;
                let mappings =
                    state["logicalProjectDraftThreadKeyByLogicalProjectKey"].as_object()?;
                mappings
                    .values()
                    .filter_map(Value::as_str)
                    .find(|key| {
                        sessions.get(*key).is_some_and(|session| {
                            session["environmentId"] == target.environment
                                && session["projectId"] == target.local_id
                        })
                    })?
                    .into()
            }
        };
        state["draftsByThreadKey"].get(key)
    }
    pub fn recovered_session(&self, target: &DraftTarget) -> Option<&Value> {
        if target.kind != DraftKind::Project
            || self.forgotten_environments.contains(&target.environment)
            || self
                .changes
                .get(target)
                .is_some_and(|changes| changes.acknowledged)
        {
            return None;
        }
        let state = self.source_state.as_deref()?;
        let sessions = state["draftThreadsByThreadKey"].as_object()?;
        state["logicalProjectDraftThreadKeyByLogicalProjectKey"]
            .as_object()?
            .values()
            .filter_map(Value::as_str)
            .find_map(|key| {
                sessions.get(key).filter(|session| {
                    session["environmentId"] == target.environment
                        && session["projectId"] == target.local_id
                })
            })
    }
    pub fn prompt(&self, target: &DraftTarget) -> Option<&str> {
        self.changes
            .get(target)
            .and_then(|changes| changes.prompt.as_deref())
            .or_else(|| {
                self.recovered(target)
                    .and_then(|draft| draft["prompt"].as_str())
            })
    }
    /// Attachments to show for a draft: this client's edits, else the original's.
    pub fn attachments(&self, target: &DraftTarget) -> SavedAttachments<'_> {
        if let Some(saved) = self
            .changes
            .get(target)
            .and_then(|changes| changes.attachments.as_ref())
        {
            return SavedAttachments::Edited(saved);
        }
        self.recovered(target)
            .filter(|draft| {
                ["attachments", "files"]
                    .iter()
                    .any(|key| draft[*key].as_array().is_some_and(|list| !list.is_empty()))
            })
            .map_or(SavedAttachments::None, SavedAttachments::Original)
    }
    pub fn dirty(&self) -> bool {
        self.revision != self.saved_revision
    }
    /// Serialization happens at flush, never while editing a prompt. Receipts
    /// are revision-specific so a late successful write cannot clear newer edits.
    pub fn prepare_write(&self) -> Result<Option<(u64, String)>, String> {
        if let Some(error) = &self.sidecar_error {
            return Err(error.clone());
        }
        if !self.dirty() {
            return Ok(None);
        }
        let sidecar = Sidecar {
            version: 1,
            entries: self
                .changes
                .iter()
                .map(|(target, changes)| (target.clone(), changes.clone()))
                .collect(),
            forgotten_environments: self.forgotten_environments.clone(),
        };
        serde_json::to_string(&sidecar)
            .map(|bytes| Some((self.revision, bytes)))
            .map_err(|error| error.to_string())
    }
    pub fn write_succeeded(&mut self, revision: u64) {
        self.saved_revision = self.saved_revision.max(revision.min(self.revision));
    }
}
fn decode_source(bytes: &str, now: &str) -> Result<Value, String> {
    let envelope: Value = serde_json::from_str(bytes)
        .map_err(|_| "Saved composer drafts contain invalid JSON.".to_string())?;
    let version = envelope
        .get("version")
        .and_then(Value::as_u64)
        .ok_or("Saved composer drafts have an invalid version.")?;
    if version > SOURCE_VERSION {
        return Err(
            "Saved composer drafts were written by a newer app. The original data was preserved."
                .into(),
        );
    }
    let state = envelope
        .get("state")
        .ok_or("Saved composer drafts have no state.")?;
    recover_state(state, version, now)
}
fn decode_sidecar(bytes: &str) -> Result<Sidecar, String> {
    let sidecar: Sidecar = serde_json::from_str(bytes)
        .map_err(|_| "Rust draft overlay is invalid; its saved data was preserved.".to_string())?;
    if sidecar.version != 1 {
        return Err(
            "Rust draft overlay was written by a newer app; its saved data was preserved.".into(),
        );
    }
    let mut targets = BTreeSet::new();
    if sidecar.entries.iter().any(|(target, changes)| {
        !target.valid()
            || !targets.insert(target)
            || changes
                .choices
                .as_ref()
                .is_some_and(|value| serde_json::from_value::<DraftChoices>(value.clone()).is_err())
    }) || sidecar.forgotten_environments.iter().any(String::is_empty)
    {
        return Err(
            "Rust draft overlay contains ambiguous identities; its saved data was preserved."
                .into(),
        );
    }
    Ok(sidecar)
}

#[cfg(test)]
mod attachment_tests {
    use super::*;
    use crate::attachments::{Kind, Persisted};
    use serde_json::json;

    fn saved(id: &str, attachment_id: Option<&str>) -> Persisted {
        Persisted {
            id: id.into(),
            r#type: Kind::File,
            name: "notes.pdf".into(),
            mime_type: "application/pdf".into(),
            size_bytes: 9,
            attachment_id: attachment_id.map(Into::into),
        }
    }
    fn original() -> String {
        let image = json!({"id":"img","name":"s.png","mimeType":"image/png","sizeBytes":5,"dataUrl":"data:image/png;base64,iVBORw0="});
        json!({"version":9,"state":{"draftsByThreadKey":{"a:thread":{"prompt":"hi","attachments":[image]}}}}).to_string()
    }
    fn target() -> DraftTarget {
        DraftTarget::thread("a", "thread")
    }

    #[test]
    fn original_attachments_show_until_this_client_edits_the_list() {
        let mut storage = DraftStorage::default();
        storage.hydrate(Some(original()), None, "2026-10-08T12:00:00.000Z");
        assert!(matches!(storage.attachments(&target()), SavedAttachments::Original(_)));
        assert!(
            storage.recovered(&target()).is_some_and(|draft| !crate::drafts::has_unrendered_content(draft)),
            "attachments no longer hold a draft; contexts still do"
        );
        // An emptied list is an edit, not an absence: the original's images stay removed.
        storage.edit_attachments(target(), vec![]);
        assert!(matches!(storage.attachments(&target()), SavedAttachments::Edited([])));
        storage.edit_attachments(target(), vec![saved("a", Some("pending-1"))]);
        let (_, bytes) = storage.prepare_write().unwrap().unwrap();
        let mut reloaded = DraftStorage::default();
        reloaded.hydrate(Some(original()), Some(bytes), "2026-10-08T12:00:00.000Z");
        let SavedAttachments::Edited(list) = reloaded.attachments(&target()) else {
            panic!("the saved list wins over the original")
        };
        assert_eq!(list, [saved("a", Some("pending-1"))]);
    }

    #[test]
    fn sending_content_clears_original_and_edited_attachments() {
        let mut storage = DraftStorage::default();
        storage.hydrate(Some(original()), None, "2026-10-08T12:00:00.000Z");
        storage.edit_attachments(target(), vec![saved("a", None)]);
        storage.acknowledge_content(target());
        assert!(matches!(storage.attachments(&target()), SavedAttachments::None));
        // Attachments added after the send are saved again.
        storage.edit_attachments(target(), vec![saved("b", None)]);
        assert!(matches!(storage.attachments(&target()), SavedAttachments::Edited([_])));
    }

    #[test]
    fn contexts_still_hold_a_draft_for_the_original_app() {
        let draft = json!({"prompt":"","attachments":[{"id":"x"}],"terminalContexts":[{"id":"t"}]});
        assert!(crate::drafts::has_unrendered_content(&draft));
        assert!(!crate::drafts::has_unrendered_content(&json!({"prompt":"","attachments":[{"id":"x"}],"files":[{"id":"f"}]})));
    }
}
