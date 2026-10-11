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
    /// A draft of a new thread, by the id of its session (`local_id`). The first version of this client kept
    /// one such draft per project and stored it as `project`; reading that sidecar moves each to a session.
    #[serde(alias = "project")]
    Draft,
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
    pub fn draft(environment: impl Into<String>, draft: impl Into<String>) -> Self {
        Self {
            environment: environment.into(),
            kind: DraftKind::Draft,
            local_id: draft.into(),
        }
    }
    fn valid(&self) -> bool {
        !self.environment.is_empty() && !self.local_id.is_empty()
    }
}
/// One draft of a new thread, at its own address (`/draft/<draft_id>`): the project it is for and when it was
/// started. What it holds (prompt, choices, attachments, chips) is keyed by its `DraftTarget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftSession {
    pub environment: String,
    pub draft_id: String,
    pub project_id: String,
    pub created_at: String,
}
impl DraftSession {
    pub fn target(&self) -> DraftTarget {
        DraftTarget::draft(self.environment.clone(), self.draft_id.clone())
    }
}
/// What a discarded draft leaves behind, for taking the discard back.
#[derive(Debug, Clone)]
pub struct DiscardedDraft {
    pub session: DraftSession,
    changes: Option<DraftChanges>,
    in_sidecar: bool,
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
    /// The context chips as last edited here (wire records). `None` defers to the original
    /// draft, so an intentionally emptied list is `Some(vec![])`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contexts: Option<Vec<Value>>,
    #[serde(default)]
    pub acknowledged: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Sidecar {
    version: u64,
    entries: Vec<(DraftTarget, DraftChanges)>,
    forgotten_environments: BTreeSet<String>,
    /// The drafts this client started, and the project of one of the original's it moved to another.
    #[serde(default)]
    sessions: Vec<DraftSession>,
}
const SIDECAR_VERSION: u64 = 2;
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
    sessions: BTreeMap<DraftTarget, DraftSession>,
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
        }
        targets.extend(self.drafts().into_iter().map(|session| session.target()));
        targets
    }
    /// The drafts of new threads that are still open, in every environment, newest first: those this client started, and the
    /// original's that were not sent or discarded. A draft the original never promoted is listed whether or not it was the one
    /// its project pointed at.
    pub fn drafts(&self) -> Vec<DraftSession> {
        let mut found: BTreeMap<DraftTarget, DraftSession> = BTreeMap::new();
        if let Some(sessions) = self.source_state.as_deref().and_then(|state| state["draftThreadsByThreadKey"].as_object()) {
            for (key, session) in sessions {
                if let (Some(environment), Some(project), true) = (session["environmentId"].as_str(), session["projectId"].as_str(), session["promotedTo"].is_null()) {
                    let session = DraftSession { environment: environment.into(), draft_id: key.clone(), project_id: project.into(), created_at: session["createdAt"].as_str().unwrap_or_default().into() };
                    found.insert(session.target(), session);
                }
            }
        }
        for (target, session) in &self.sessions {
            found.insert(target.clone(), session.clone());
        }
        let mut drafts: Vec<_> = found
            .into_values()
            .filter(|session| !self.forgotten_environments.contains(&session.environment) && !self.changes.get(&session.target()).is_some_and(|changes| changes.acknowledged))
            .collect();
        drafts.sort_by(|left, right| right.created_at.cmp(&left.created_at).then_with(|| left.draft_id.cmp(&right.draft_id)));
        drafts
    }
    pub fn draft(&self, target: &DraftTarget) -> Option<DraftSession> {
        self.drafts().into_iter().find(|session| session.target() == *target)
    }
    /// Starts a draft for a project under an id the caller chose.
    pub fn start_draft(&mut self, environment: &str, draft_id: &str, project_id: &str, now: &str) -> DraftSession {
        let session = DraftSession { environment: environment.into(), draft_id: draft_id.into(), project_id: project_id.into(), created_at: now.into() };
        self.sessions.insert(session.target(), session.clone());
        self.revision += 1;
        session
    }
    /// Whether a draft holds anything the person put there: text, attachments or chips (the original's `composerDraftHasUserContent`).
    pub fn has_content(&self, target: &DraftTarget) -> bool {
        let text = self.prompt(target).is_some_and(|prompt| !prompt.trim().is_empty());
        let attachments = match self.attachments(target) {
            SavedAttachments::None => false,
            SavedAttachments::Edited(list) => !list.is_empty(),
            SavedAttachments::Original(_) => true,
        };
        text || attachments || !self.contexts(target).is_empty()
    }
    /// Removes a draft: a sent one (its thread exists now) or a discarded one. What it held is hidden, not erased from the original's data.
    pub fn discard(&mut self, target: &DraftTarget) -> Option<DiscardedDraft> {
        let session = self.draft(target)?;
        let in_sidecar = self.sessions.remove(target).is_some();
        let changes = self.changes.get(target).cloned();
        self.acknowledge(target.clone());
        Some(DiscardedDraft { session, changes, in_sidecar })
    }
    /// Takes a discard back.
    pub fn restore(&mut self, discarded: DiscardedDraft) {
        let target = discarded.session.target();
        match discarded.changes {
            Some(changes) => {
                self.changes.insert(target.clone(), changes);
            }
            None => {
                self.changes.remove(&target);
            }
        }
        if discarded.in_sidecar {
            self.sessions.insert(target, discarded.session);
        }
        self.revision += 1;
    }
    /// Moves a draft to another project.
    pub fn move_draft(&mut self, target: &DraftTarget, project_id: &str) {
        if let Some(mut session) = self.draft(target) {
            session.project_id = project_id.into();
            self.sessions.insert(target.clone(), session);
            self.revision += 1;
        }
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
                    let saved = self.migrate(saved, now);
                    for session in saved.sessions {
                        self.sessions.entry(session.target()).or_insert(session);
                    }
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
    /// The first version of the overlay kept one draft per project, stored under the project's id. Each of those becomes a session: of
    /// the original's draft that project pointed at, when it has one (so the text the original saved stays under it), else of its own.
    fn migrate(&self, mut saved: Sidecar, now: &str) -> Sidecar {
        if saved.version >= SIDECAR_VERSION {
            return saved;
        }
        let mut made = vec![];
        for (target, _) in saved.entries.iter_mut().filter(|(target, _)| target.kind == DraftKind::Draft) {
            let project = target.local_id.clone();
            let pointed = self.source_state.as_deref().and_then(|state| {
                let sessions = state["draftThreadsByThreadKey"].as_object()?;
                state["logicalProjectDraftThreadKeyByLogicalProjectKey"].as_object()?.values().filter_map(Value::as_str).find(|key| {
                    sessions.get(*key).is_some_and(|session| session["environmentId"] == target.environment && session["projectId"] == project.as_str() && session["promotedTo"].is_null())
                }).map(str::to_owned)
            });
            match pointed {
                Some(key) => target.local_id = key,
                // A draft of its own, under the project's id: nothing else uses that id.
                None => made.push(DraftSession { environment: target.environment.clone(), draft_id: project.clone(), project_id: project, created_at: now.into() }),
            }
        }
        saved.sessions.extend(made);
        saved.version = SIDECAR_VERSION;
        saved
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
    pub fn edit_contexts(&mut self, target: DraftTarget, contexts: Vec<Value>) {
        self.changes.entry(target).or_default().contexts = Some(contexts);
        self.revision += 1;
    }
    /// The context chips of a draft: this client's edits, else the original's.
    pub fn contexts(&self, target: &DraftTarget) -> Vec<Value> {
        if let Some(saved) = self.changes.get(target).and_then(|changes| changes.contexts.as_ref()) {
            return saved.clone();
        }
        self.recovered(target).map(crate::composer_context::records_from_draft).unwrap_or_default()
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
        changes.contexts = None;
        changes.acknowledged = true;
        self.revision += 1;
    }
    pub fn forget(&mut self, environment: &str) {
        self.forgotten_environments.insert(environment.into());
        self.changes
            .retain(|target, _| target.environment != environment);
        self.sessions.retain(|target, _| target.environment != environment);
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
            DraftKind::Draft => {
                // The original files a draft of a new thread under the id of its session.
                let session = state["draftThreadsByThreadKey"].get(&target.local_id)?;
                if session["environmentId"] != target.environment || !session["promotedTo"].is_null() {
                    return None;
                }
                target.local_id.clone()
            }
        };
        state["draftsByThreadKey"].get(&key)
    }
    pub fn recovered_session(&self, target: &DraftTarget) -> Option<&Value> {
        if target.kind != DraftKind::Draft
            || self.forgotten_environments.contains(&target.environment)
            || self
                .changes
                .get(target)
                .is_some_and(|changes| changes.acknowledged)
        {
            return None;
        }
        let session = self.source_state.as_deref()?["draftThreadsByThreadKey"].get(&target.local_id)?;
        (session["environmentId"] == target.environment && session["promotedTo"].is_null()).then_some(session)
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
            version: SIDECAR_VERSION,
            entries: self
                .changes
                .iter()
                .map(|(target, changes)| (target.clone(), changes.clone()))
                .collect(),
            forgotten_environments: self.forgotten_environments.clone(),
            sessions: self.sessions.values().cloned().collect(),
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
    if !(1..=SIDECAR_VERSION).contains(&sidecar.version) {
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
    fn only_annotations_and_picked_elements_still_hold_a_draft_for_the_original_app() {
        let draft = json!({"prompt":"","attachments":[{"id":"x"}],"previewAnnotations":[{"id":"a"}]});
        assert!(crate::drafts::has_unrendered_content(&draft));
        assert!(crate::drafts::has_unrendered_content(&json!({"elementContexts":[{"id":"e"}]})));
        // Terminal selections, review comments and thread references are chips now.
        assert!(!crate::drafts::has_unrendered_content(&json!({"terminalContexts":[{"id":"t"}],"reviewComments":[{"id":"r"}],"threadContexts":[{"kind":"thread"}]})));
        assert!(!crate::drafts::has_unrendered_content(&json!({"prompt":"","attachments":[{"id":"x"}],"files":[{"id":"f"}]})));
    }

    #[test]
    fn chips_are_edited_here_or_come_from_the_original_draft_until_a_send_clears_them() {
        let record = crate::composer_context::thread_record("env", "thread-a", "Login bug");
        let mut storage = DraftStorage::default();
        let original = json!({"version":1,"draftsByThreadKey":{"env:thread-1":{"prompt":"p","attachments":[],"threadContexts":[record.clone()]}},"draftThreadsByThreadKey":{},"logicalProjectDraftThreadKeyByLogicalProjectKey":{}});
        storage.source_state = Some(std::rc::Rc::new(original));
        let target = DraftTarget::thread("env", "thread-1");
        assert_eq!(storage.contexts(&target), vec![record.clone()], "the original's chips are shown");
        storage.edit_contexts(target.clone(), vec![]);
        assert!(storage.contexts(&target).is_empty(), "an emptied list hides them");
        storage.edit_contexts(target.clone(), vec![record.clone()]);
        storage.acknowledge_content(target.clone());
        assert!(storage.contexts(&target).is_empty(), "a send clears them");
    }
}

#[cfg(test)]
mod draft_session_tests {
    use super::*;
    use serde_json::json;

    const NOW: &str = "2026-10-11T12:00:00.000Z";

    fn original() -> String {
        // Two drafts of one project: the one it points at, and an older one the original left behind; and a promoted one.
        json!({"version":9,"state":{
            "draftsByThreadKey":{"d-new":{"prompt":"newer","attachments":[]},"d-old":{"prompt":"older","attachments":[]},"d-sent":{"prompt":"sent","attachments":[]}},
            "draftThreadsByThreadKey":{
                "d-new":{"threadId":"t1","environmentId":"a","projectId":"p","logicalProjectKey":"a:p","createdAt":"2026-10-02T00:00:00.000Z","runtimeMode":"full-access","interactionMode":"default","envMode":"local"},
                "d-old":{"threadId":"t2","environmentId":"a","projectId":"p","logicalProjectKey":"a:p","createdAt":"2026-10-01T00:00:00.000Z","runtimeMode":"full-access","interactionMode":"default","envMode":"local"},
                "d-sent":{"threadId":"t3","environmentId":"a","projectId":"p","logicalProjectKey":"a:p","createdAt":"2026-10-03T00:00:00.000Z","runtimeMode":"full-access","interactionMode":"default","envMode":"local","promotedTo":{"environmentId":"a","threadId":"t3"}}
            },
            "logicalProjectDraftThreadKeyByLogicalProjectKey":{"a:p":"d-new"}
        }}).to_string()
    }

    #[test]
    fn every_unsent_draft_of_the_original_is_listed_not_only_the_one_a_project_points_at() {
        let mut storage = DraftStorage::default();
        storage.hydrate(Some(original()), None, NOW);
        let ids: Vec<_> = storage.drafts().into_iter().map(|session| session.draft_id).collect();
        assert_eq!(ids, ["d-new", "d-old"], "newest first; the promoted draft is a thread now");
        assert_eq!(storage.prompt(&DraftTarget::draft("a", "d-old")), Some("older"));
        assert_eq!(storage.prompt(&DraftTarget::draft("b", "d-old")), None, "another environment's id is another draft");
    }

    #[test]
    fn a_draft_started_here_survives_a_reload_and_a_discard_can_be_taken_back() {
        let mut storage = DraftStorage::default();
        storage.hydrate(None, None, NOW);
        let session = storage.start_draft("a", "mine", "p", NOW);
        storage.edit_prompt(session.target(), "hello".into());
        assert!(storage.has_content(&session.target()));
        let (_, bytes) = storage.prepare_write().unwrap().unwrap();

        let mut reloaded = DraftStorage::default();
        reloaded.hydrate(None, Some(bytes), NOW);
        assert_eq!(reloaded.drafts(), vec![session.clone()]);
        assert_eq!(reloaded.prompt(&session.target()), Some("hello"));

        let discarded = reloaded.discard(&session.target()).unwrap();
        assert!(reloaded.drafts().is_empty());
        reloaded.restore(discarded);
        assert_eq!(reloaded.drafts(), vec![session.clone()]);
        assert_eq!(reloaded.prompt(&session.target()), Some("hello"));
    }

    #[test]
    fn discarding_an_original_draft_hides_it_and_keeps_its_bytes() {
        let mut storage = DraftStorage::default();
        storage.hydrate(Some(original()), None, NOW);
        let target = DraftTarget::draft("a", "d-old");
        let discarded = storage.discard(&target).unwrap();
        assert_eq!(storage.drafts().len(), 1);
        assert_eq!(storage.prompt(&target), None);
        assert_eq!(storage.source_bytes.as_deref(), Some(original().as_str()), "the original's data is never rewritten");
        storage.restore(discarded);
        assert_eq!(storage.prompt(&target), Some("older"));
    }

    #[test]
    fn the_first_overlay_kept_one_draft_per_project_and_each_becomes_a_session() {
        // Version 1: `project` entries under a project id. One project's draft is the original's pointed-at draft; another has none.
        let overlay = json!({"version":1,"entries":[
            [{"environment":"a","kind":"project","localId":"p"},{"prompt":"edited here","acknowledged":false}],
            [{"environment":"a","kind":"project","localId":"q"},{"prompt":"for q","acknowledged":false}],
            [{"environment":"a","kind":"thread","localId":"t"},{"prompt":"thread text","acknowledged":false}]
        ],"forgottenEnvironments":[]}).to_string();
        let mut storage = DraftStorage::default();
        storage.hydrate(Some(original()), Some(overlay), NOW);
        assert!(storage.sidecar_error.is_none(), "{:?}", storage.sidecar_error);
        assert_eq!(storage.prompt(&DraftTarget::draft("a", "d-new")), Some("edited here"), "the edit rides on the draft the project pointed at");
        assert_eq!(storage.prompt(&DraftTarget::draft("a", "q")), Some("for q"));
        assert_eq!(storage.prompt(&DraftTarget::thread("a", "t")), Some("thread text"));
        let projects: Vec<_> = storage.drafts().into_iter().map(|session| (session.draft_id, session.project_id)).collect();
        assert!(projects.contains(&("q".to_owned(), "q".to_owned())), "{projects:?}");
        assert!(projects.contains(&("d-old".to_owned(), "p".to_owned())));
        // Writing it back is the current version, so the move happens once.
        storage.edit_prompt(DraftTarget::draft("a", "q"), "again".into());
        let (_, bytes) = storage.prepare_write().unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&bytes).unwrap()["version"], 2);
    }

    #[test]
    fn a_draft_moves_to_another_project_and_an_environment_forgotten_takes_its_drafts() {
        let mut storage = DraftStorage::default();
        storage.hydrate(Some(original()), None, NOW);
        let target = DraftTarget::draft("a", "d-old");
        storage.move_draft(&target, "other");
        assert_eq!(storage.draft(&target).unwrap().project_id, "other");
        storage.forget("a");
        assert!(storage.drafts().is_empty());
    }
}
