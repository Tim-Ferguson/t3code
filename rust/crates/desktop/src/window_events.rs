//! What the page is told when the window changes in ways its own events do not report: leaving or entering full screen
//! (`WINDOW_FULLSCREEN_STATE_CHANNEL`, `getWindowFullscreenState` of the original) and coming to the front
//! (`browser-window-focus`, which clears the dock badge and tells the page to clear its own). The window toolkit reports the
//! raw state on every event; this says which reports are changes, so that the page hears of each change once.

/// The last full screen and focus state reported.
#[derive(Debug, Default, Clone, Copy)]
pub struct Watch {
    fullscreen: bool,
    focused: bool,
}

impl Watch {
    /// The new full screen state when `now` differs from the last one reported.
    pub fn fullscreen(&mut self, now: bool) -> Option<bool> {
        (std::mem::replace(&mut self.fullscreen, now) != now).then_some(now)
    }

    /// Whether the window has just come to the front: the first report of focus after it was lost, not the report that repeats it.
    pub fn gained_focus(&mut self, now: bool) -> bool {
        std::mem::replace(&mut self.focused, now) != now && now
    }
}

/// The attribute the page's style reads to leave the room it keeps for the traffic lights, which full screen takes away
/// (`isMacosDesktop && !isWindowFullscreen` of `AppSidebarLayout.tsx`).
pub const FULLSCREEN_ATTRIBUTE: &str = "data-window-fullscreen";

/// Runs in the page when the full screen state changes.
pub fn fullscreen_script(fullscreen: bool) -> String {
    format!("document.documentElement.toggleAttribute('{FULLSCREEN_ATTRIBUTE}', {fullscreen});")
}

/// Runs in the page when the window comes to the front: what the page does when its own window gains focus (shows the unread
/// notifications as seen and clears the badge) is done once the shell has said so.
pub const FOCUS_SCRIPT: &str = "window.dispatchEvent(new Event('focus'));";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_screen_is_reported_when_it_changes_and_not_while_it_stays() {
        let mut watch = Watch::default();
        assert_eq!(watch.fullscreen(false), None, "a window that starts in a window is not a change");
        assert_eq!(watch.fullscreen(true), Some(true));
        assert_eq!(watch.fullscreen(true), None, "every resize of a full screen window repeats it");
        assert_eq!(watch.fullscreen(false), Some(false));
    }

    #[test]
    fn coming_to_the_front_is_reported_once_and_losing_focus_not_at_all() {
        let mut watch = Watch::default();
        assert!(watch.gained_focus(true));
        assert!(!watch.gained_focus(true), "a repeated report is not a new arrival");
        assert!(!watch.gained_focus(false));
        assert!(watch.gained_focus(true));
    }

    #[test]
    fn the_page_is_told_with_an_attribute_it_can_style_by() {
        assert_eq!(fullscreen_script(true), "document.documentElement.toggleAttribute('data-window-fullscreen', true);");
        assert_eq!(fullscreen_script(false), "document.documentElement.toggleAttribute('data-window-fullscreen', false);");
    }
}
