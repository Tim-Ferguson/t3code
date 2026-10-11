//! The main window's saved size and position (`DesktopAppSettings.mainWindowBounds` and `mainWindowMaximized`,
//! and `resolveInitialMainWindowBounds` in `DesktopWindow.ts`). Sizes are logical points, so a saved window
//! means the same thing on a display with another scale.
use serde::{Deserialize, Serialize};
use std::path::Path;

/// `DEFAULT_MAIN_WINDOW_SIZE`.
pub const DEFAULT_SIZE: (u32, u32) = (1100, 780);
/// The window cannot be made smaller than this (`minWidth` and `minHeight` of the original's window).
pub const MIN_SIZE: (u32, u32) = (840, 620);
/// A burst of moves and resizes writes once, this long after the last (`MAIN_WINDOW_BOUNDS_PERSIST_DEBOUNCE_MS`).
pub const SAVE_DEBOUNCE_MS: u64 = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowState {
    pub bounds: Option<Bounds>,
    /// Only meaningful with bounds: a window that was maximized comes back maximized over its normal bounds.
    pub maximized: bool,
}

/// The saved state in `text`, or none if it is not valid. Bounds smaller than the minimum are not a window
/// the person chose; they are dropped, and so is "maximized" without bounds.
pub fn parse(text: &str) -> WindowState {
    let Ok(mut state) = serde_json::from_str::<WindowState>(text) else { return WindowState::default() };
    if state.bounds.is_some_and(|bounds| bounds.width < MIN_SIZE.0 || bounds.height < MIN_SIZE.1) {
        state.bounds = None;
    }
    state.maximized &= state.bounds.is_some();
    state
}

pub fn load(path: &Path) -> WindowState {
    std::fs::read_to_string(path).map(|text| parse(&text)).unwrap_or_default()
}

/// Writes the state whole or not at all, so a crash mid-write leaves the earlier file.
pub fn save(path: &Path, state: &WindowState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(state).expect("window state is plain data"))?;
    std::fs::rename(temporary, path)
}

/// What the window is opened with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Initial {
    /// The default size, placed by the system.
    Default,
    Saved { bounds: Bounds, maximized: bool },
}

/// The saved bounds if one display still holds the whole window; otherwise the default (a window saved on a
/// display that is no longer connected must not open out of reach).
pub fn initial(saved: &WindowState, displays: &[Bounds]) -> Initial {
    match saved.bounds {
        Some(bounds) if displays.iter().any(|display| fits(&bounds, display)) => Initial::Saved { bounds, maximized: saved.maximized },
        _ => Initial::Default,
    }
}

fn fits(window: &Bounds, display: &Bounds) -> bool {
    let (wx, wy, dx, dy) = (i64::from(window.x), i64::from(window.y), i64::from(display.x), i64::from(display.y));
    wx >= dx && wy >= dy && wx + i64::from(window.width) <= dx + i64::from(display.width) && wy + i64::from(window.height) <= dy + i64::from(display.height)
}

/// What the window is, when it moved or changed size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observed {
    pub bounds: Bounds,
    pub maximized: bool,
    pub fullscreen: bool,
    pub minimized: bool,
}

/// Decides what to save. A maximized window cannot tell its normal bounds, so those are the last ones seen
/// while it was normal; a full-screen or minimized window changes nothing.
#[derive(Debug, Default)]
pub struct Tracker {
    normal: Option<Bounds>,
}
impl Tracker {
    pub fn new(saved: &WindowState) -> Self {
        Self { normal: saved.bounds }
    }
    pub fn observe(&mut self, seen: Observed) -> Option<WindowState> {
        if seen.fullscreen || seen.minimized {
            return None;
        }
        if !seen.maximized {
            self.normal = Some(seen.bounds);
        }
        Some(WindowState { bounds: self.normal, maximized: seen.maximized && self.normal.is_some() })
    }
}

/// Collects the states a burst of window events asks to save. `note` returns a ticket; once the debounce time
/// has passed, `due` hands over the latest state only for the most recent ticket, so earlier timers do nothing.
#[derive(Debug, Default)]
pub struct Debounce {
    latest: Option<WindowState>,
    ticket: u64,
}
impl Debounce {
    pub fn note(&mut self, state: WindowState) -> u64 {
        self.latest = Some(state);
        self.ticket += 1;
        self.ticket
    }
    pub fn due(&mut self, ticket: u64) -> Option<WindowState> {
        if ticket == self.ticket { self.latest.take() } else { None }
    }
    /// The window is closing: whatever is waiting is saved now.
    pub fn flush(&mut self) -> Option<WindowState> {
        self.ticket += 1;
        self.latest.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: i32, y: i32, width: u32, height: u32) -> Bounds {
        Bounds { x, y, width, height }
    }
    const DISPLAY: Bounds = Bounds { x: 0, y: 0, width: 1728, height: 1117 };

    #[test]
    fn saved_bounds_come_back_only_on_a_display_that_still_holds_them() {
        let state = WindowState { bounds: Some(bounds(100, 80, 1200, 800)), maximized: true };
        assert_eq!(initial(&state, &[DISPLAY]), Initial::Saved { bounds: bounds(100, 80, 1200, 800), maximized: true });
        // The display it was on is gone; a second display to the right is where it was.
        let moved = WindowState { bounds: Some(bounds(2000, 80, 1200, 800)), maximized: false };
        assert_eq!(initial(&moved, &[DISPLAY]), Initial::Default);
        assert_eq!(initial(&moved, &[DISPLAY, bounds(1728, 0, 1920, 1080)]), Initial::Saved { bounds: bounds(2000, 80, 1200, 800), maximized: false });
        // Half off the edge is not held, and a window with no display at all gets the default.
        let hanging = WindowState { bounds: Some(bounds(1000, 0, 1200, 800)), maximized: false };
        assert_eq!(initial(&hanging, &[DISPLAY]), Initial::Default);
        assert_eq!(initial(&state, &[]), Initial::Default);
        assert_eq!(initial(&WindowState::default(), &[DISPLAY]), Initial::Default);
    }

    #[test]
    fn a_damaged_or_implausible_file_is_the_default_not_an_error() {
        assert_eq!(parse("not json"), WindowState::default());
        assert_eq!(parse(r#"{"bounds":{"x":1,"y":2,"width":"wide","height":3}}"#), WindowState::default());
        // Too small to be a window anyone chose, and "maximized" with nothing to come back to.
        assert_eq!(parse(r#"{"bounds":{"x":0,"y":0,"width":100,"height":100},"maximized":true}"#), WindowState::default());
        assert_eq!(parse(r#"{"maximized":true}"#), WindowState::default());
        let good = parse(r#"{"bounds":{"x":-5,"y":10,"width":900,"height":700},"maximized":true}"#);
        assert_eq!(good, WindowState { bounds: Some(bounds(-5, 10, 900, 700)), maximized: true });
    }

    #[test]
    fn the_state_survives_a_write_and_read_and_a_missing_file_is_the_default() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("window-state.json");
        assert_eq!(load(&path), WindowState::default());
        let state = WindowState { bounds: Some(bounds(10, 20, 1000, 700)), maximized: false };
        save(&path, &state).unwrap();
        assert_eq!(load(&path), state);
        assert!(!path.with_extension("json.tmp").exists(), "the temporary file is renamed away");
        let later = WindowState { bounds: Some(bounds(30, 40, 1200, 900)), maximized: true };
        save(&path, &later).unwrap();
        assert_eq!(load(&path), later);
    }

    #[test]
    fn a_maximized_window_keeps_the_bounds_it_had_when_normal_and_full_screen_changes_nothing() {
        let mut tracker = Tracker::default();
        let normal = bounds(50, 60, 1100, 780);
        let seen = |bounds, maximized, fullscreen, minimized| Observed { bounds, maximized, fullscreen, minimized };
        assert_eq!(tracker.observe(seen(normal, false, false, false)), Some(WindowState { bounds: Some(normal), maximized: false }));
        // Maximized: the window now reports the screen's size, which is not what to come back to.
        assert_eq!(tracker.observe(seen(DISPLAY, true, false, false)), Some(WindowState { bounds: Some(normal), maximized: true }));
        assert_eq!(tracker.observe(seen(DISPLAY, true, true, false)), None);
        assert_eq!(tracker.observe(seen(normal, false, false, true)), None);
        let resized = bounds(50, 60, 1300, 800);
        assert_eq!(tracker.observe(seen(resized, false, false, false)), Some(WindowState { bounds: Some(resized), maximized: false }));
        // A maximized window that was never seen normal has nowhere to come back to.
        assert_eq!(Tracker::default().observe(seen(DISPLAY, true, false, false)), Some(WindowState::default()));
        // The saved bounds seed the tracker, so maximizing right after opening still keeps them.
        let mut seeded = Tracker::new(&WindowState { bounds: Some(normal), maximized: false });
        assert_eq!(seeded.observe(seen(DISPLAY, true, false, false)), Some(WindowState { bounds: Some(normal), maximized: true }));
    }

    #[test]
    fn a_burst_of_changes_saves_once_with_the_last_state_and_closing_saves_what_is_waiting() {
        let mut debounce = Debounce::default();
        let state = |width| WindowState { bounds: Some(bounds(0, 0, width, 700)), maximized: false };
        let first = debounce.note(state(900));
        let second = debounce.note(state(950));
        let third = debounce.note(state(1000));
        // The timers of the first two find that something newer is waiting.
        assert_eq!(debounce.due(first), None);
        assert_eq!(debounce.due(second), None);
        assert_eq!(debounce.due(third), Some(state(1000)));
        assert_eq!(debounce.due(third), None, "saved once");
        let fourth = debounce.note(state(1100));
        assert_eq!(debounce.flush(), Some(state(1100)));
        assert_eq!(debounce.due(fourth), None, "the flush took it");
        assert_eq!(debounce.flush(), None);
    }
}
