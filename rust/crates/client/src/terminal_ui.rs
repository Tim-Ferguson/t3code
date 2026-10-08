//! Scoped terminal drawer transitions from terminalUiStateStore.ts.
//! Suppressed ids are transient: stale server metadata cannot reopen a closed pane.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
pub const STORAGE_KEY: &str = "t3code:terminal-state:v1";
pub const STORAGE_VERSION: u32 = 4;
pub const DEFAULT_HEIGHT: f64 = 280.0;
pub const DEFAULT_ID: &str = "term-1";
pub const MAX_PER_GROUP: usize = 4;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Horizontal,
    Vertical,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub terminal_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_direction: Option<Direction>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneState {
    pub terminal_open: bool,
    pub terminal_height: f64,
    pub terminal_ids: Vec<String>,
    pub active_terminal_id: String,
    pub terminal_groups: Vec<Group>,
    pub active_terminal_group_id: String,
}
impl Default for PaneState {
    fn default() -> Self {
        Self {
            terminal_open: false,
            terminal_height: DEFAULT_HEIGHT,
            terminal_ids: vec![],
            active_terminal_id: String::new(),
            terminal_groups: vec![],
            active_terminal_group_id: String::new(),
        }
    }
}
fn trim(id: &str) -> &str {
    id.trim_matches(|ch| matches!(ch,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'))
}
fn normalized_ids(ids: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    ids.iter()
        .filter_map(|id| {
            let id = trim(id);
            (!id.is_empty() && seen.insert(id)).then(|| id.to_owned())
        })
        .collect()
}
fn fallback(id: &str) -> String {
    format!("group-{id}")
}
fn unique(base: String, used: &mut BTreeSet<String>) -> String {
    let mut candidate = base.clone();
    let mut index = 2;
    while used.contains(&candidate) {
        candidate = format!("{base}-{index}");
        index += 1;
    }
    used.insert(candidate.clone());
    candidate
}
fn groups(groups: &[Group], ids: &[String]) -> Vec<Group> {
    let mut assigned = BTreeSet::new();
    let mut used = BTreeSet::new();
    let mut result = vec![];
    for group in groups {
        let members: Vec<_> = normalized_ids(&group.terminal_ids)
            .into_iter()
            .filter(|id| ids.contains(id) && !assigned.contains(id))
            .collect();
        if members.is_empty() {
            continue;
        }
        assigned.extend(members.iter().cloned());
        let base = if trim(&group.id).is_empty() {
            fallback(&members[0])
        } else {
            trim(&group.id).to_owned()
        };
        result.push(Group {
            id: unique(base, &mut used),
            terminal_ids: members,
            split_direction: group
                .split_direction
                .filter(|direction| *direction == Direction::Vertical),
        });
    }
    for id in ids {
        if !assigned.contains(id) {
            result.push(Group {
                id: unique(fallback(id), &mut used),
                terminal_ids: vec![id.clone()],
                split_direction: None,
            });
        }
    }
    result
}
impl PaneState {
    pub fn normalized(&self) -> Self {
        let ids = normalized_ids(&self.terminal_ids);
        let active = if ids.contains(&self.active_terminal_id) {
            self.active_terminal_id.clone()
        } else {
            ids.first().cloned().unwrap_or_default()
        };
        let groups = groups(&self.terminal_groups, &ids);
        let active_group = groups
            .iter()
            .find(|group| group.id == self.active_terminal_group_id)
            .or_else(|| {
                groups
                    .iter()
                    .find(|group| group.terminal_ids.contains(&active))
            })
            .or_else(|| groups.first())
            .map(|group| group.id.clone())
            .unwrap_or_default();
        Self {
            terminal_open: self.terminal_open,
            terminal_height: if self.terminal_height.is_finite() && self.terminal_height > 0.0 {
                self.terminal_height
            } else {
                DEFAULT_HEIGHT
            },
            terminal_ids: ids,
            active_terminal_id: active,
            terminal_groups: groups,
            active_terminal_group_id: active_group,
        }
    }
    pub fn set_open(&self, open: bool) -> Self {
        let mut state = self.normalized();
        if open && state.terminal_ids.is_empty() {
            return state.upsert(DEFAULT_ID, false, Direction::Horizontal);
        }
        state.terminal_open = open;
        state
    }
    pub fn set_height(&self, height: f64) -> Self {
        let mut state = self.normalized();
        if height.is_finite() && height > 0.0 {
            state.terminal_height = height
        }
        state
    }
    pub fn upsert(&self, id: &str, split: bool, direction: Direction) -> Self {
        let mut state = self.normalized();
        if trim(id).is_empty() {
            return state;
        }
        let original = state.clone();
        let is_new = !state.terminal_ids.iter().any(|existing| existing == id);
        if is_new {
            state.terminal_ids.push(id.to_owned())
        }
        for group in &mut state.terminal_groups {
            group.terminal_ids.retain(|existing| existing != id)
        }
        state
            .terminal_groups
            .retain(|group| !group.terminal_ids.is_empty());
        let group_index = if !split || original.terminal_ids.is_empty() {
            let mut used = state
                .terminal_groups
                .iter()
                .map(|group| group.id.clone())
                .collect();
            state.terminal_groups.push(Group {
                id: unique(fallback(id), &mut used),
                terminal_ids: vec![id.to_owned()],
                split_direction: None,
            });
            state.terminal_groups.len() - 1
        } else {
            let index = state
                .terminal_groups
                .iter()
                .position(|group| group.id == original.active_terminal_group_id)
                .or_else(|| {
                    state
                        .terminal_groups
                        .iter()
                        .position(|group| group.terminal_ids.contains(&original.active_terminal_id))
                })
                .unwrap_or_else(|| {
                    let mut used = state
                        .terminal_groups
                        .iter()
                        .map(|group| group.id.clone())
                        .collect();
                    state.terminal_groups.push(Group {
                        id: unique(fallback(&original.active_terminal_id), &mut used),
                        terminal_ids: vec![original.active_terminal_id.clone()],
                        split_direction: None,
                    });
                    state.terminal_groups.len() - 1
                });
            let group = &mut state.terminal_groups[index];
            if is_new
                && !group.terminal_ids.iter().any(|existing| existing == id)
                && group.terminal_ids.len() >= MAX_PER_GROUP
            {
                return original;
            }
            if !group.terminal_ids.iter().any(|existing| existing == id) {
                let insert = group
                    .terminal_ids
                    .iter()
                    .position(|existing| existing == &original.active_terminal_id)
                    .map_or(group.terminal_ids.len(), |index| index + 1);
                group.terminal_ids.insert(insert, id.to_owned());
            }
            group.split_direction = (direction == Direction::Vertical).then_some(direction);
            index
        };
        state.active_terminal_group_id = state.terminal_groups[group_index].id.clone();
        state.active_terminal_id = id.to_owned();
        state.terminal_open = true;
        state.normalized()
    }
    pub fn activate(&self, id: &str) -> Self {
        let mut state = self.normalized();
        if !state.terminal_ids.iter().any(|existing| existing == id) {
            return state;
        }
        state.active_terminal_id = id.to_owned();
        if let Some(group) = state
            .terminal_groups
            .iter()
            .find(|group| group.terminal_ids.iter().any(|existing| existing == id))
        {
            state.active_terminal_group_id = group.id.clone()
        }
        state
    }
    pub fn close(&self, id: &str) -> Self {
        let mut state = self.normalized();
        let Some(index) = state
            .terminal_ids
            .iter()
            .position(|existing| existing == id)
        else {
            return state;
        };
        state.terminal_ids.remove(index);
        if state.terminal_ids.is_empty() {
            return Self::default();
        }
        if state.active_terminal_id == id {
            state.active_terminal_id =
                state.terminal_ids[index.min(state.terminal_ids.len() - 1)].clone()
        }
        for group in &mut state.terminal_groups {
            group.terminal_ids.retain(|existing| existing != id)
        }
        state
            .terminal_groups
            .retain(|group| !group.terminal_ids.is_empty());
        state.active_terminal_group_id = state
            .terminal_groups
            .iter()
            .find(|group| group.terminal_ids.contains(&state.active_terminal_id))
            .or_else(|| state.terminal_groups.first())
            .map(|group| group.id.clone())
            .unwrap_or_else(|| fallback(&state.active_terminal_id));
        state.normalized()
    }
    pub fn reconcile(&self, ids: &[String]) -> Self {
        let mut state = self.normalized();
        if state.terminal_ids == ids {
            return state;
        }
        if !ids.contains(&state.active_terminal_id) {
            state.active_terminal_id = ids.first().cloned().unwrap_or_default()
        }
        state.terminal_groups = groups(&state.terminal_groups, ids);
        state.active_terminal_group_id = state
            .terminal_groups
            .iter()
            .find(|group| group.terminal_ids.contains(&state.active_terminal_id))
            .or_else(|| state.terminal_groups.first())
            .map(|group| group.id.clone())
            .unwrap_or_default();
        state.terminal_ids = ids.to_vec();
        state.normalized()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedPanes {
    pub terminal_ui_state_by_thread_key: BTreeMap<String, PaneState>,
    #[serde(skip)]
    pub suppressed: BTreeMap<String, BTreeSet<String>>,
}
impl ScopedPanes {
    pub fn key(environment: &str, thread: &str) -> String {
        format!("{environment}:{thread}")
    }
    pub fn get(&self, key: &str) -> PaneState {
        self.terminal_ui_state_by_thread_key
            .get(key)
            .cloned()
            .unwrap_or_default()
    }
    pub fn set(&mut self, key: &str, state: PaneState) {
        let state = state.normalized();
        if state == PaneState::default() {
            self.terminal_ui_state_by_thread_key.remove(key);
        } else {
            self.terminal_ui_state_by_thread_key
                .insert(key.to_owned(), state);
        }
    }
    pub fn close(&mut self, key: &str, id: &str) {
        self.set(key, self.get(key).close(id));
        let id = trim(id);
        if !id.is_empty() {
            self.suppressed
                .entry(key.to_owned())
                .or_default()
                .insert(id.to_owned());
        }
    }
    pub fn upsert(&mut self, key: &str, id: &str, split: bool, direction: Direction) {
        self.set(key, self.get(key).upsert(id, split, direction));
        if let Some(ids) = self.suppressed.get_mut(key) {
            ids.remove(trim(id));
            if ids.is_empty() {
                self.suppressed.remove(key);
            }
        }
    }
    pub fn set_open(&mut self, key: &str, open: bool) {
        let state = self.get(key);
        let unsuppress = open && state.terminal_ids.is_empty();
        self.set(key, state.set_open(open));
        if unsuppress {
            if let Some(ids) = self.suppressed.get_mut(key) {
                ids.remove(DEFAULT_ID);
                if ids.is_empty() {
                    self.suppressed.remove(key);
                }
            }
        }
    }
    pub fn reconcile(&mut self, key: &str, ids: &[String]) {
        let ids: Vec<_> = ids
            .iter()
            .filter(|id| {
                !self
                    .suppressed
                    .get(key)
                    .is_some_and(|set| set.contains(*id))
            })
            .cloned()
            .collect();
        self.set(key, self.get(key).reconcile(&ids));
    }
    pub fn remove(&mut self, key: &str) {
        self.terminal_ui_state_by_thread_key.remove(key);
        self.suppressed.remove(key);
    }
    pub fn retain_threads(&mut self, keys: &BTreeSet<String>) {
        self.terminal_ui_state_by_thread_key
            .retain(|key, _| keys.contains(key));
        self.suppressed.retain(|key, _| keys.contains(key));
    }
}
