//! Source timeline geometry and thread-scoped reading-position policy.
//! Platform adapters supply measured rows; no DOM or renderer lives here.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const CHAT_TIMELINE_ANCHOR_OFFSET: f64 = 24.0;
pub const TIMELINE_FOLLOW_REARM_THRESHOLD: f64 = 40.0;
pub const REMEMBERED_POSITION_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunObservation {
    pub thread_key: Option<String>,
    pub hydrated: bool,
    pub run_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunInput {
    #[serde(flatten)]
    pub observation: RunObservation,
    pub queued: bool,
    pub message_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedRun {
    pub observation: RunObservation,
    pub anchor_message_id: Option<String>,
}
/// Source observeTimelineRun: hydration opens a baseline, never an old turn anchor.
pub fn observe_run(previous: Option<&RunObservation>, input: &RunInput) -> ObservedRun {
    let mut observation = input.observation.clone();
    if input.queued {
        observation.run_id = None;
    }
    if previous
        .is_none_or(|previous| previous.thread_key != observation.thread_key || !previous.hydrated)
    {
        return ObservedRun {
            observation,
            anchor_message_id: None,
        };
    }
    let previous = previous.expect("baseline checked");
    if !input.observation.hydrated
        || input.observation.run_id.is_none()
        || input.queued
        || previous.run_id == input.observation.run_id
        || input.message_id.is_none()
    {
        return ObservedRun {
            observation: previous.clone(),
            anchor_message_id: None,
        };
    }
    ObservedRun {
        observation,
        anchor_message_id: input.message_id.clone(),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndState {
    pub is_at_end: Option<bool>,
    pub content_length: Option<f64>,
    pub scroll: Option<f64>,
    pub scroll_length: Option<f64>,
}
/// The source uses measured distance, including the composer inset, instead of
/// trusting the virtualizer's strict flag while streaming growth settles.
pub fn resolve_at_end(state: Option<&EndState>) -> Option<bool> {
    let state = state?;
    match (state.content_length, state.scroll, state.scroll_length) {
        (Some(content), Some(scroll), Some(viewport)) => {
            Some(content - scroll - viewport <= TIMELINE_FOLLOW_REARM_THRESHOLD)
        }
        _ => state.is_at_end,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Measurements {
    pub row_ids: Vec<String>,
    pub positions: Vec<Option<f64>>,
    pub sizes: Vec<Option<f64>>,
    pub scroll: f64,
    pub viewport_height: f64,
}
impl Measurements {
    fn position(&self, index: usize) -> Option<f64> {
        self.positions
            .get(index)
            .copied()
            .flatten()
            .filter(|value| value.is_finite())
    }
    pub fn row_bottom(&self, index: usize) -> Option<f64> {
        let top = self.position(index)?;
        let height = self
            .sizes
            .get(index)
            .copied()
            .flatten()
            .filter(|value| value.is_finite())?;
        Some(top + js_max(1.0, height))
    }
    pub fn content_overflows(&self, composer_inset: f64, anchor_offset: f64) -> bool {
        if self.row_ids.is_empty()
            || !self.viewport_height.is_finite()
            || self.viewport_height <= 0.0
        {
            return false;
        }
        self.row_bottom(self.row_ids.len() - 1).is_some_and(|last| {
            last > js_max(0.0, self.viewport_height - composer_inset - anchor_offset)
        })
    }
    pub fn anchored_turn(
        &self,
        anchor_index: f64,
        composer_overlay_height: f64,
        anchor_offset: f64,
    ) -> Option<AnchoredTurnMetrics> {
        if self.row_ids.is_empty() {
            return None;
        }
        let index = js_max(0.0, js_min(anchor_index, (self.row_ids.len() - 1) as f64));
        if !index.is_finite() || index.fract() != 0.0 {
            return None;
        }
        let anchor_top = self.position(index as usize)?;
        let last_bottom = self.row_bottom(self.row_ids.len() - 1)?;
        let usable_viewport_height = js_max(
            0.0,
            self.viewport_height - composer_overlay_height - anchor_offset,
        );
        let turn_height = js_max(0.0, last_bottom - anchor_top);
        let target_scroll_to_reveal_end = js_max(0.0, last_bottom - usable_viewport_height);
        Some(AnchoredTurnMetrics {
            anchor_top,
            last_bottom,
            turn_height,
            usable_viewport_height,
            visible_usable_bottom: self.scroll + usable_viewport_height,
            overflows_usable_viewport: turn_height > usable_viewport_height,
            target_scroll_to_reveal_end,
            scroll_delta_to_reveal_end: js_max(0.0, target_scroll_to_reveal_end - self.scroll),
        })
    }
    /// Source resolveWorkGroupScrollAnchor uses actual offsets, never cached
    /// visible ranges that can lag a fling or a newly prepended history page.
    pub fn reading_anchor(&self) -> Option<RowAnchor> {
        if self.row_ids.is_empty() || !self.scroll.is_finite() {
            return None;
        }
        let scroll_offset = js_max(0.0, self.scroll);
        let mut low = 0;
        let mut high = self.row_ids.len();
        let mut index = 0;
        let mut row_top = self.position(0)?;
        while low < high {
            let middle = low + (high - low - 1) / 2;
            let top = self.position(middle)?;
            if top <= scroll_offset {
                index = middle;
                row_top = top;
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Some(RowAnchor {
            row_id: self.row_ids[index].clone(),
            offset_within_row: js_max(0.0, scroll_offset - row_top),
            scroll_offset,
        })
    }
    /// Same measured-row reconciliation as MessagesTimeline. Missing rows
    /// fall back to the saved absolute scroll offset.
    pub fn restore_offset(&self, anchor: &RowAnchor, max_scroll: f64) -> f64 {
        let offset = self
            .row_ids
            .iter()
            .position(|id| id == &anchor.row_id)
            .and_then(|index| self.position(index))
            .map(|top| top + anchor.offset_within_row)
            .unwrap_or(anchor.scroll_offset);
        js_max(0.0, js_min(offset, max_scroll))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnchoredTurnMetrics {
    pub anchor_top: f64,
    pub last_bottom: f64,
    pub turn_height: f64,
    pub usable_viewport_height: f64,
    pub visible_usable_bottom: f64,
    pub overflows_usable_viewport: bool,
    pub target_scroll_to_reveal_end: f64,
    pub scroll_delta_to_reveal_end: f64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowAnchor {
    pub row_id: String,
    pub offset_within_row: f64,
    pub scroll_offset: f64,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Disclosures {
    pub runs: BTreeSet<String>,
    pub work_groups: BTreeSet<String>,
    pub attempts: BTreeSet<String>,
    pub work_group_scroll_positions: BTreeMap<String, EntryPosition>,
    pub expanded_entries: BTreeSet<String>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct EntryPosition {
    pub entry_id: String,
    pub offset: f64,
}
#[derive(Debug, Clone, PartialEq)]
pub struct RememberedPosition {
    pub anchor: RowAnchor,
    pub at_end: bool,
    pub disclosures: Option<Disclosures>,
}
/// Source cache replaces recency on write, not read; identities include the
/// destination environment and remain independent for equal local thread IDs.
#[derive(Debug, Clone, Default)]
pub struct PositionCache(VecDeque<(String, RememberedPosition)>);
impl PositionCache {
    pub fn read(&self, key: &str) -> Option<&RememberedPosition> {
        self.0
            .iter()
            .find(|(owner, _)| owner == key)
            .map(|(_, position)| position)
    }
    pub fn remember(&mut self, key: String, position: RememberedPosition) {
        self.0.retain(|(owner, _)| owner != &key);
        self.0.push_back((key, position));
        if self.0.len() > REMEMBERED_POSITION_LIMIT {
            self.0.pop_front();
        }
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

// Rust max/min suppress NaN, unlike ECMAScript Math.max/min. Keep the source
// semantics for incomplete measurements and numeric boundary tests.
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}
fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

/// Exact ChatView.logic predicate. Explicit work-entry metadata is required;
/// a notice/message in another run cannot release new-turn framing.
pub fn release_anchor_for_tool_activity(input: &serde_json::Value) -> bool {
    if input["anchorMessageId"].is_null()
        || input["liveFollowEnabled"] != true
        || input["runningTurnId"].is_null()
    {
        return false;
    }
    input["timelineEntries"].as_array().is_some_and(|entries| entries.iter().any(|row| {
        let entry=&row["entry"];
        row["kind"]=="work" && entry["runId"]==input["runningTurnId"] && (
            entry["tone"]=="tool" || entry.get("itemType").is_some() || entry.get("requestKind").is_some() || entry["command"].as_str().is_some_and(|command|!command.trim_matches(|c| matches!(c,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')).is_empty())
        )
    }))
}
