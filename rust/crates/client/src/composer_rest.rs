//! The composer's resting layout: with "Collapse composer on scroll" on, scrolling the conversation of an existing thread rests the
//! composer into a single line, and the next interaction with it lifts that. Port of `composerScrollGesture.ts` and
//! `shouldUseRestingComposerLayout` (`composerFooterLayout.ts`), with the thresholds `ChatComposer.tsx` gives them.

/// Scrolling this far down the conversation in one gesture rests the composer (`COMPOSER_SCROLL_COLLAPSE_THRESHOLD_PX`).
pub const COLLAPSE_THRESHOLD_PX: f64 = 24.0;
/// A pause this long ends a gesture (`COMPOSER_SCROLL_GESTURE_RESET_MS`).
pub const GESTURE_RESET_MS: f64 = 120.0;

/// What the wheel has done so far in the gesture it is part of.
#[derive(Debug, Clone, PartialEq)]
pub struct Gesture {
    accumulated_px: f64,
    suppressed: bool,
    last_event_at: f64,
}
impl Default for Gesture {
    fn default() -> Self {
        Self { accumulated_px: 0.0, suppressed: false, last_event_at: f64::NEG_INFINITY }
    }
}

/// One wheel event over the conversation, as the renderer measured it.
#[derive(Debug, Clone, Copy)]
pub struct Wheel {
    /// The event's time stamp, in milliseconds.
    pub at: f64,
    /// The vertical delta, with `delta_mode` applied (lines are 16 px, a page is the viewport).
    pub delta_px: f64,
    /// Whether the wheel is over the conversation itself and not over something inside it that scrolls on its own.
    pub over_timeline: bool,
    pub collapse_eligible: bool,
    /// The scroll position has room to move the way the wheel turns.
    pub can_scroll_that_way: bool,
    pub toward_end: bool,
}

impl Gesture {
    /// `recordComposerScrollGestureEvent`, after the pause that ends a gesture has been honoured: whether this event rests the composer.
    pub fn record(&mut self, wheel: Wheel) -> bool {
        if wheel.at - self.last_event_at > GESTURE_RESET_MS {
            *self = Self::default();
        }
        self.last_event_at = wheel.at;
        if self.suppressed || !wheel.collapse_eligible || !wheel.can_scroll_that_way || wheel.toward_end {
            self.accumulated_px = 0.0;
            return false;
        }
        self.accumulated_px += wheel.delta_px;
        if self.accumulated_px < COLLAPSE_THRESHOLD_PX {
            return false;
        }
        self.accumulated_px = 0.0;
        true
    }
    /// `suppressActiveComposerScrollGesture`: a gesture that is under way when the person touches the composer does not rest it.
    pub fn suppress(&mut self, now: f64) {
        if now - self.last_event_at <= GESTURE_RESET_MS {
            self.suppressed = true;
        }
    }
}

/// `shouldCollapseComposerForScrollKey`: whether a navigation key pressed in the composer scrolls the conversation away from where it is.
pub fn key_rests_composer(key: &str, scroll_top: f64, scroll_height: f64, client_height: f64, at_logical_end: bool) -> bool {
    match key {
        "PageUp" | "Home" => scroll_top > 1.0,
        "PageDown" | "End" => !at_logical_end && scroll_top < scroll_height - client_height - 1.0,
        _ => false,
    }
}

/// What decides whether the composer rests (`shouldUseRestingComposerLayout`).
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub existing_thread: bool,
    /// A gesture rested it, and nothing has lifted that since.
    pub scroll_collapsed: bool,
    /// Something is open in the composer that needs room: a menu, an error, an attachment that needs a look.
    pub expanded_chrome: bool,
    pub multiline_prompt: bool,
    /// The conversation has more than fits above the composer; a short one has nothing to give back.
    pub timeline_overflows: bool,
}
impl Layout {
    pub fn resting(self) -> bool {
        self.existing_thread && self.timeline_overflows && self.scroll_collapsed && !self.multiline_prompt && !self.expanded_chrome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel(at: f64, delta_px: f64) -> Wheel {
        Wheel { at, delta_px, over_timeline: true, collapse_eligible: true, can_scroll_that_way: true, toward_end: false }
    }

    #[test]
    fn a_gesture_rests_the_composer_once_it_has_scrolled_far_enough() {
        let mut gesture = Gesture::default();
        assert!(!gesture.record(wheel(0.0, 10.0)));
        assert!(!gesture.record(wheel(16.0, 10.0)));
        assert!(gesture.record(wheel(32.0, 10.0)), "30 px is past the threshold");
        assert!(!gesture.record(wheel(48.0, 10.0)), "the count starts again after a rest");
    }

    #[test]
    fn a_pause_ends_the_gesture_and_its_count() {
        let mut gesture = Gesture::default();
        assert!(!gesture.record(wheel(0.0, 20.0)));
        // 20 more px would pass 24, but not after a pause longer than the reset.
        assert!(!gesture.record(wheel(500.0, 20.0)));
        assert!(gesture.record(wheel(510.0, 20.0)));
    }

    #[test]
    fn scrolling_toward_the_end_or_off_the_conversation_or_with_nothing_to_scroll_never_rests_it() {
        for blocked in [
            Wheel { toward_end: true, ..wheel(0.0, 100.0) },
            Wheel { can_scroll_that_way: false, ..wheel(0.0, 100.0) },
            Wheel { collapse_eligible: false, ..wheel(0.0, 100.0) },
        ] {
            let mut gesture = Gesture::default();
            assert!(!gesture.record(blocked));
        }
        // A blocked event also empties what was counted.
        let mut gesture = Gesture::default();
        assert!(!gesture.record(wheel(0.0, 20.0)));
        assert!(!gesture.record(Wheel { toward_end: true, ..wheel(10.0, 5.0) }));
        assert!(!gesture.record(wheel(20.0, 20.0)));
    }

    #[test]
    fn touching_the_composer_in_the_middle_of_a_gesture_ends_its_chance_to_rest_it() {
        let mut gesture = Gesture::default();
        assert!(!gesture.record(wheel(0.0, 10.0)));
        gesture.suppress(50.0);
        assert!(!gesture.record(wheel(60.0, 100.0)));
        // Suppression ends with the gesture.
        assert!(gesture.record(wheel(1000.0, 100.0)));
        // Touching it between gestures suppresses nothing.
        let mut gesture = Gesture::default();
        assert!(!gesture.record(wheel(0.0, 10.0)));
        gesture.suppress(1000.0);
        assert!(gesture.record(wheel(1010.0, 100.0)));
    }

    #[test]
    fn navigation_keys_rest_the_composer_only_when_they_move_the_conversation() {
        assert!(key_rests_composer("PageUp", 400.0, 2000.0, 600.0, false));
        assert!(!key_rests_composer("Home", 0.0, 2000.0, 600.0, false));
        assert!(key_rests_composer("PageDown", 400.0, 2000.0, 600.0, false));
        assert!(!key_rests_composer("End", 1400.0, 2000.0, 600.0, false));
        assert!(!key_rests_composer("End", 400.0, 2000.0, 600.0, true));
        assert!(!key_rests_composer("a", 400.0, 2000.0, 600.0, false));
    }

    #[test]
    fn the_layout_rests_only_for_an_existing_thread_that_overflows_with_nothing_open() {
        let rests = Layout { existing_thread: true, scroll_collapsed: true, expanded_chrome: false, multiline_prompt: false, timeline_overflows: true };
        assert!(rests.resting());
        assert!(!Layout { existing_thread: false, ..rests }.resting());
        assert!(!Layout { scroll_collapsed: false, ..rests }.resting());
        assert!(!Layout { expanded_chrome: true, ..rests }.resting());
        assert!(!Layout { multiline_prompt: true, ..rests }.resting());
        assert!(!Layout { timeline_overflows: false, ..rests }.resting());
    }
}
