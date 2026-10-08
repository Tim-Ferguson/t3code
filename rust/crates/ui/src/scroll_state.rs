//! Pure state transitions from ChatView's timeline scroll controller.
use t3_client::timeline_scroll::{
    RememberedPosition, RowAnchor, RunInput, RunObservation, observe_run,
};

#[derive(Debug, Clone, PartialEq)]
pub enum ScrollMode {
    FollowingEnd,
    AnchoringNewTurn(String),
    FreeScrolling,
}
#[derive(Debug, Clone, PartialEq)]
pub enum ScrollTarget {
    End,
    Row(RowAnchor),
    AuthoredMessage(String),
}
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollRequest {
    owner: String,
    user_generation: u64,
    request_generation: u64,
    pub target: ScrollTarget,
}
#[derive(Debug, Clone)]
pub struct ScrollState {
    owner: String,
    pub mode: ScrollMode,
    user_generation: u64,
    request_generation: u64,
    observed_run: Option<RunObservation>,
}
impl ScrollState {
    pub fn new(owner: String, remembered: Option<&RememberedPosition>) -> Self {
        Self {
            owner,
            mode: if remembered.is_some_and(|position| !position.at_end) {
                ScrollMode::FreeScrolling
            } else {
                ScrollMode::FollowingEnd
            },
            user_generation: 0,
            request_generation: 0,
            observed_run: None,
        }
    }
    /// New/reopened threads default to the end. Explicit reading positions
    /// restore their row-relative offset; a citation owns positioning instead.
    pub fn initial_position(
        &mut self,
        remembered: Option<&RememberedPosition>,
        citation_requested: bool,
    ) -> Option<ScrollRequest> {
        if citation_requested {
            return None;
        }
        let target = remembered
            .filter(|position| !position.at_end)
            .map(|position| ScrollTarget::Row(position.anchor.clone()))
            .unwrap_or(ScrollTarget::End);
        Some(self.request(target))
    }
    pub fn manual_navigation(&mut self) {
        self.user_generation += 1;
        self.request_generation += 1;
        self.mode = ScrollMode::FreeScrolling;
    }
    /// An underflowing transcript, a nested tool scroller, a pinch zoom, or
    /// downward wheel at the live edge must not silently disable following.
    pub fn wheel(
        &mut self,
        delta_y: f64,
        ctrl_key: bool,
        timeline_target: bool,
        content_overflows: bool,
    ) -> bool {
        if !ctrl_key && timeline_target && delta_y < 0.0 && content_overflows {
            self.manual_navigation();
            true
        } else {
            false
        }
    }
    pub fn touch_move(&mut self, away_from_end: bool) -> bool {
        if away_from_end {
            self.manual_navigation();
            true
        } else {
            false
        }
    }
    pub fn pointer_down(
        &mut self,
        scrollbar: bool,
        content_overflows: bool,
        away_from_end: bool,
    ) -> bool {
        if if scrollbar {
            content_overflows
        } else {
            away_from_end
        } {
            self.manual_navigation();
            true
        } else {
            false
        }
    }
    pub fn own_send(&mut self, message_id: String, queued: bool) -> Option<ScrollRequest> {
        if queued {
            return None;
        }
        self.mode = ScrollMode::AnchoringNewTurn(message_id.clone());
        Some(self.request(ScrollTarget::AuthoredMessage(message_id)))
    }
    /// Delayed hydration establishes a baseline. Later remotely initiated
    /// turns frame once only while following; a reader is never pulled away.
    pub fn observe(&mut self, input: &RunInput) -> Option<ScrollRequest> {
        if input.observation.thread_key.as_deref() != Some(self.owner.as_str()) {
            return None;
        }
        let observed = observe_run(self.observed_run.as_ref(), input);
        self.observed_run = Some(observed.observation);
        let message = observed.anchor_message_id?;
        if self.mode != ScrollMode::FollowingEnd {
            return None;
        }
        self.mode = ScrollMode::AnchoringNewTurn(message.clone());
        Some(self.request(ScrollTarget::AuthoredMessage(message)))
    }
    pub fn at_end_changed(&mut self, at_end: bool) {
        if at_end && self.mode == ScrollMode::FreeScrolling {
            self.mode = ScrollMode::FollowingEnd;
        }
        // Streaming layout can report false before the following write.
        // Only an actual navigation gesture opts out of live following.
    }
    pub fn follow_end(&mut self) -> ScrollRequest {
        self.mode = ScrollMode::FollowingEnd;
        self.request(ScrollTarget::End)
    }
    /// Measured history prepends retain the reader's row, not the same absolute
    /// offset (which would now refer to the newly loaded older messages).
    pub fn preserve_history(&mut self, anchor: RowAnchor) -> ScrollRequest {
        self.request(ScrollTarget::Row(anchor))
    }
    pub fn anchor_settled(&mut self, message_id: &str) {
        if matches!(&self.mode,ScrollMode::AnchoringNewTurn(id) if id==message_id) {
            self.mode = ScrollMode::FollowingEnd;
            self.request_generation += 1;
        }
    }
    /// Async measurements/scroll completion must still own this destination,
    /// thread, manual-scroll generation, and latest imperative request.
    pub fn owns(&self, request: &ScrollRequest) -> bool {
        request.owner == self.owner
            && request.user_generation == self.user_generation
            && request.request_generation == self.request_generation
    }
    fn request(&mut self, target: ScrollTarget) -> ScrollRequest {
        self.request_generation += 1;
        ScrollRequest {
            owner: self.owner.clone(),
            user_generation: self.user_generation,
            request_generation: self.request_generation,
            target,
        }
    }
}

#[cfg(test)]
#[path = "scroll_state_tests.rs"]
mod tests;
