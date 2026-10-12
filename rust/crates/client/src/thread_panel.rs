//! Where the thread details card sits and how much of it shows: ports of `threadDetailsCardLayout.ts` (its placement beside the
//! conversation and the density that folds its content), the card's open state in `rightPanelStore.ts`
//! (`threadPanelVisibilityByThreadKey`, `updateThreadPanelVisibilityMap`, what `partialize` keeps), and what each density holds from
//! `ThreadDetailsPanel.tsx`. The card itself is `thread_details.rs` of the `ui` crate.
use std::collections::BTreeMap;

/// The card's width, kept in step with `--thread-details-panel-width` (17.5rem) which sizes the popover.
pub const CARD_WIDTH: f64 = 280.;
/// The gap between the card and the edge of the conversation column, and above it.
pub const GAP: f64 = 12.;
/// The space between the chat and the card (`DETAILS_CARD_CLEARANCE`): the chat stays centred while the card fits beside it with this much room.
pub const CLEARANCE: f64 = 32.;
/// The card is not placed in less height than this.
pub const MIN_HEIGHT: f64 = 160.;
/// The chat lane the canvas keeps (`resolveChatCanvasLayout`'s defaults): its padding and the narrowest it is kept readable at.
pub const LANE_PADDING: f64 = 20.;
pub const MIN_CHAT_WIDTH: f64 = 640.;
/// The widest the chat lane is (`maxChatWidth`).
pub const MAX_CHAT_WIDTH: f64 = 768.;

/// How much of the card shows (`resolveThreadDetailsCardDensity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Full,
    Compact,
    Essential,
}

/// The heights the card's content has been seen to need at each density (0: not seen yet).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Heights {
    pub full: f64,
    pub compact: f64,
}

/// `resolveThreadDetailsCardDensity`: the most that fits in `height`. A density never measured is assumed to fit, so that it is
/// shown once and measured.
pub fn density(height: f64, content: Heights) -> Density {
    if content.full == 0. || content.full <= height {
        Density::Full
    } else if content.compact == 0. || content.compact <= height {
        Density::Compact
    } else {
        Density::Essential
    }
}

/// Where the card goes when it is pinned to the top right of the conversation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A floating browser player's frame, which the card steps around while the player is resized (`PreviewMiniPlayerFrame`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// `resolveThreadDetailsCardLayout`: the card pins to the top right while a readable chat lane fits beside it, else `None` (the card
/// is a popover from the header then). `frame` is a floating player the card must clear; this app has none, so it is `None` there.
pub fn layout(container: (f64, f64), lane_padding: f64, min_chat_width: f64, frame: Option<Frame>, overlaps_details_card: bool) -> Option<Placement> {
    let (width, x) = (CARD_WIDTH, container.0 - CARD_WIDTH - GAP);
    if x - CLEARANCE - lane_padding < min_chat_width {
        return None;
    }
    let height = match frame {
        Some(frame) if overlaps_details_card && frame.x + frame.width > x - GAP && frame.x < x + width + GAP => (container.1 - GAP * 2.).min(frame.y - GAP * 2.),
        _ => container.1 - GAP * 2.,
    };
    if height < MIN_HEIGHT {
        return None;
    }
    Some(Placement { x, y: GAP, width, height })
}

/// How far the chat lane moves left to leave room for the card inline (`resolveChatCanvasLayout`'s `normalLeft` against the centred
/// lane): zero while the card fits beside the centred chat, else only as far as it needs. The lane narrows only after it reaches the
/// left padding, which a placement (it keeps `MIN_CHAT_WIDTH` of lane) never asks for.
pub fn lane_shift(container_width: f64, card_left: f64) -> f64 {
    let centered_width = (container_width - LANE_PADDING * 2.).clamp(0., MAX_CHAT_WIDTH);
    let lane_right = card_left - CLEARANCE;
    let width = centered_width.min(lane_right - LANE_PADDING).max(0.);
    let centered_left = (container_width - centered_width) / 2.;
    let left = LANE_PADDING.max(((container_width - width) / 2.).min(lane_right - width));
    (centered_left - left).max(0.)
}

/// What a density holds of the card (`ThreadDetailsPanel.tsx`): which parts are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parts {
    /// The environment and workspace choices of the branch toolbar.
    pub workspace_toolbar: bool,
    pub open_in: bool,
    /// The branch and Git actions section, with a rule above it, and its Git actions in their short form.
    pub separated: bool,
    pub compact_git: bool,
    pub automations: bool,
    pub lineage: bool,
}

/// `density === "full"` shows the whole card; "compact" drops the workspace choices, the rule, the automations and the lineage;
/// "essential" also drops the editor picker. A draft has no automations or lineage to show.
pub fn parts(density: Density, draft: bool) -> Parts {
    let full = density == Density::Full;
    Parts { workspace_toolbar: full, open_in: density != Density::Essential, separated: full, compact_git: !full, automations: full && !draft, lineage: full && !draft }
}

/// Whether the card is open in each of its two presentations (`ThreadPanelVisibility`): inline beside the conversation, and as a popover
/// from the header. Opening inline is the default; the popover starts closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visibility {
    pub inline_open: bool,
    pub popover_open: bool,
}
impl Default for Visibility {
    fn default() -> Self {
        Self { inline_open: true, popover_open: false }
    }
}

/// How the card is presented now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presentation {
    Inline,
    Popover,
}

/// The card's open state per thread (`threadPanelVisibilityByThreadKey`). A thread at the default has no entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Visibilities {
    by_thread: BTreeMap<String, Visibility>,
}
impl Visibilities {
    pub fn get(&self, key: &str) -> Visibility {
        self.by_thread.get(key).copied().unwrap_or_default()
    }
    /// `selectThreadPanelOpen`.
    pub fn is_open(&self, key: &str, presentation: Presentation) -> bool {
        let visibility = self.get(key);
        match presentation {
            Presentation::Inline => visibility.inline_open,
            Presentation::Popover => visibility.popover_open,
        }
    }
    fn update(&mut self, key: &str, change: impl FnOnce(Visibility) -> Visibility) {
        let next = change(self.get(key));
        if next == Visibility::default() {
            self.by_thread.remove(key);
        } else {
            self.by_thread.insert(key.to_owned(), next);
        }
    }
    /// `setThreadPanelOpen`.
    pub fn set_open(&mut self, key: &str, presentation: Presentation, open: bool) {
        self.update(key, |visibility| match presentation {
            Presentation::Inline => Visibility { inline_open: open, ..visibility },
            Presentation::Popover => Visibility { popover_open: open, ..visibility },
        });
    }
    /// `toggleThreadPanel`.
    pub fn toggle(&mut self, key: &str, presentation: Presentation) {
        let open = self.is_open(key, presentation);
        self.set_open(key, presentation, !open);
    }
    /// The threads whose card is not at the default.
    pub fn keys(&self) -> Vec<String> {
        self.by_thread.keys().cloned().collect()
    }
    /// `removeThread`: a deleted thread keeps no state.
    pub fn remove(&mut self, key: &str) {
        self.by_thread.remove(key);
    }
    /// What `partialize` keeps: only a card closed inline is remembered; an open popover is not.
    pub fn to_json(&self) -> serde_json::Value {
        self.by_thread.iter().filter(|(_, visibility)| !visibility.inline_open).map(|(key, _)| (key.clone(), serde_json::json!({"inlineOpen":false,"popoverOpen":false}))).collect::<serde_json::Map<_, _>>().into()
    }
    /// What storage held (`migratePersistedRightPanelState`): an entry that is not two booleans is dropped, and one at the default is not kept.
    pub fn from_json(stored: Option<&serde_json::Value>) -> Self {
        let mut visibilities = Self::default();
        for (key, entry) in stored.and_then(serde_json::Value::as_object).into_iter().flatten() {
            let (Some(inline_open), Some(popover_open)) = (entry["inlineOpen"].as_bool(), entry["popoverOpen"].as_bool()) else { continue };
            visibilities.update(key, |_| Visibility { inline_open, popover_open });
        }
        visibilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANE: (f64, f64) = (LANE_PADDING, MIN_CHAT_WIDTH);
    fn resolve(width: f64, height: f64, preview_y: Option<f64>) -> Option<Placement> {
        layout((width, height), LANE.0, LANE.1, preview_y.map(|y| Frame { x: width - 332., y, width: 320., height: 240. }), false)
    }
    fn at(x: f64, height: f64) -> Placement {
        Placement { x, y: 12., width: 280., height }
    }

    // The cases of `threadDetailsCardLayout.test.ts`, replayed.
    #[test]
    fn the_card_pins_to_the_top_right_at_a_fixed_width_and_hides_when_no_readable_lane_fits_beside_it() {
        assert_eq!(resolve(1600., 900., None), Some(at(1308., 876.)));
        assert_eq!(resolve(1344., 900., None).map(|p| (p.x, p.width)), Some((1052., 280.)));
        assert_eq!(resolve(984., 900., None).map(|p| p.x), Some(692.));
        assert_eq!(resolve(983., 900., None), None);
        for y in [12., 170., 250., 400., 648.] {
            assert_eq!(resolve(1600., 900., Some(y)), Some(at(1308., 876.)), "a player dragged vertically does not move the card");
        }
        assert_eq!(layout((1600., 900.), LANE.0, LANE.1, Some(Frame { x: 12., y: 100., width: 320., height: 240. }), false).map(|p| p.height), Some(876.));
    }

    #[test]
    fn content_folds_only_when_it_cannot_fit_and_returns_when_space_does() {
        let place = |preview_y: f64, preview_height: f64, overlaps: bool| layout((1584., 988.), LANE.0, LANE.1, Some(Frame { x: 1260., y: preview_y, width: 240., height: preview_height }), overlaps);
        let content = Heights { full: 327., compact: 182. };
        for y in [12., 170., 225., 240., 340., 380., 611.] {
            let placement = place(y, 365., false).unwrap();
            assert_eq!((placement.x, placement.y, placement.height), (1292., 12., 964.));
            assert_eq!(density(placement.height, content), Density::Full);
        }
        assert_eq!(density(place(351., 625., true).unwrap().height, content), Density::Full);
        assert_eq!(density(place(350., 626., true).unwrap().height, content), Density::Compact);
        assert_eq!(place(184., 792., true).map(|p| (p.y, p.height)), Some((12., 160.)));
        assert_eq!(place(183., 793., true), None, "no readable height");
        // Never measured: shown, to be measured. Too tall at both: essential.
        assert_eq!(density(100., Heights::default()), Density::Full);
        assert_eq!(density(100., Heights { full: 300., compact: 0. }), Density::Compact);
        assert_eq!(density(100., Heights { full: 300., compact: 200. }), Density::Essential);
    }

    #[test]
    fn the_chat_lane_moves_left_only_as_far_as_the_card_needs() {
        // Beside the centred lane with room to spare: no shift.
        assert_eq!(lane_shift(1700., 1700. - 292.), 0.);
        // At the narrowest placement (984 wide) the lane is its 640 minimum against the left padding.
        assert_eq!(lane_shift(984., 692.), 88.);
        // Between: the centred 768 lane would touch the card, so it moves by the overlap.
        let width = 1300.;
        let card_left = width - CARD_WIDTH - GAP;
        let centred_right = (width + MAX_CHAT_WIDTH) / 2.;
        assert_eq!(lane_shift(width, card_left), centred_right - (card_left - CLEARANCE));
    }

    #[test]
    fn each_density_draws_what_the_original_draws() {
        let full = parts(Density::Full, false);
        assert!(full.workspace_toolbar && full.open_in && full.separated && !full.compact_git && full.automations && full.lineage);
        let compact = parts(Density::Compact, false);
        assert!(!compact.workspace_toolbar && compact.open_in && !compact.separated && compact.compact_git && !compact.automations && !compact.lineage);
        let essential = parts(Density::Essential, false);
        assert!(!essential.open_in && essential.compact_git);
        let draft = parts(Density::Full, true);
        assert!(draft.workspace_toolbar && !draft.automations && !draft.lineage, "a draft has no automations or relatives");
    }

    #[test]
    fn a_card_is_open_inline_until_closed_and_only_a_closed_card_is_remembered() {
        let mut visibilities = Visibilities::default();
        assert!(visibilities.is_open("a", Presentation::Inline) && !visibilities.is_open("a", Presentation::Popover));
        visibilities.toggle("a", Presentation::Inline);
        assert!(!visibilities.is_open("a", Presentation::Inline));
        visibilities.toggle("b", Presentation::Popover);
        assert!(visibilities.is_open("b", Presentation::Popover));
        assert_eq!(visibilities.to_json(), serde_json::json!({"a":{"inlineOpen":false,"popoverOpen":false}}), "the popover is not saved");
        // Back to the default leaves no entry, so nothing is kept for it.
        visibilities.toggle("a", Presentation::Inline);
        visibilities.set_open("b", Presentation::Popover, false);
        assert_eq!(visibilities, Visibilities::default());
        // Reading what was saved: bad entries and entries at the default are dropped.
        let read = Visibilities::from_json(Some(&serde_json::json!({"a":{"inlineOpen":false,"popoverOpen":false},"b":{"inlineOpen":true,"popoverOpen":false},"c":{"inlineOpen":"no"},"d":3})));
        assert!(!read.is_open("a", Presentation::Inline) && read.is_open("b", Presentation::Inline) && read.is_open("c", Presentation::Inline));
        let mut read = read;
        read.remove("a");
        assert_eq!(read, Visibilities::default(), "a deleted thread keeps no state");
    }
}
