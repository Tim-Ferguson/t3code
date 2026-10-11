//! Quitting with the quit shortcut (`window/QuitHold.ts`): by default the shortcut must be held, or pressed
//! twice, so that a stray press does not end a session with agents working. Quitting from the application menu
//! is not affected and always quits at once.
//!
//! This is only the decisions. The caller feeds it key events and the time (in milliseconds, from any clock),
//! runs the `Effect`s it returns, and calls `timer` when the one timer it asked for fires.

pub const HOLD_MS: u64 = 1200;
pub const DOUBLE_PRESS_MS: u64 = 500;
/// "Still held" is shown by auto-repeat key events, not by the lack of a release: macOS does not deliver a
/// letter's release while the command key is down. Once held, quitting waits for the release or for this long
/// without a repeat (or twice the observed repeat interval, if that is longer).
pub const RELEASE_GRACE_MS: u64 = 600;

/// `QuitConfirmationMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Direct,
    Hold,
    DoubleClick,
}

/// Which hint to show: the original's `QUIT_SHORTCUT_CHANNEL` events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    Down(Mode),
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Hint(Hint),
    Quit,
    /// Call `timer` after this long, replacing any timer already set.
    Arm(u64),
    Disarm,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub down: bool,
    /// The key as the page reports it (`"q"`, `"Meta"`, `"Control"`); compared in lower case.
    pub key: String,
    pub meta: bool,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub repeat: bool,
}

/// What the caller does with the key event itself.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The event was part of the quit shortcut and goes no further.
    pub swallow: bool,
    pub effects: Vec<Effect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Watch {
    Quit,
    Release,
}

#[derive(Debug)]
pub struct QuitHold {
    macos: bool,
    watchdog: Option<Watch>,
    holding: bool,
    mode: Option<Mode>,
    notified: bool,
    armed: bool,
    quit_on_release: bool,
    held_since: u64,
    last_press: u64,
    last_repeat: u64,
    repeat_cadence: u64,
}

impl QuitHold {
    pub fn new(macos: bool) -> Self {
        Self { macos, watchdog: None, holding: false, mode: None, notified: false, armed: false, quit_on_release: false, held_since: 0, last_press: 0, last_repeat: 0, repeat_cadence: 0 }
    }

    pub fn key(&mut self, input: &Key, now: u64, mode: Mode) -> Outcome {
        let mut effects = Vec::new();
        let swallow = self.handle(input, now, mode, &mut effects);
        Outcome { swallow, effects }
    }

    /// The timer asked for by `Effect::Arm` has run out.
    pub fn timer(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        match self.watchdog.take() {
            Some(Watch::Quit) => self.quit_now(&mut effects),
            Some(Watch::Release) => self.release(false, &mut effects),
            None => {}
        }
        effects
    }

    fn modifier(&self) -> &'static str {
        if self.macos { "meta" } else { "control" }
    }

    fn handle(&mut self, input: &Key, now: u64, mode: Mode, effects: &mut Vec<Effect>) -> bool {
        let key = input.key.to_lowercase();
        if !input.down {
            if key == "q" {
                let quit = self.quit_on_release;
                self.release(true, effects);
                if quit {
                    effects.push(Effect::Quit);
                }
            } else if key == self.modifier() {
                if self.quit_on_release {
                    self.quit_after_quiet_period(effects);
                } else {
                    self.release(true, effects);
                }
            }
            return false;
        }
        let modifier_down = if self.macos { input.meta } else { input.control };
        if input.repeat && modifier_down && key == "q" {
            let since = if self.last_repeat == 0 { self.held_since } else { self.last_repeat };
            self.repeat_cadence = now.saturating_sub(since);
            self.last_repeat = now;
        }
        if self.quit_on_release {
            // A held key's repeats must not reach the next application.
            if key == "q" {
                self.quit_after_quiet_period(effects);
            }
            return true;
        }
        if !modifier_down || input.alt || input.shift || key != "q" {
            // Pressing the modifier again is the first half of a second full shortcut, and must not cancel a
            // double press that is waiting.
            if key == self.modifier() && !input.alt && !input.shift {
                return false;
            }
            // Any other key cancels the hold and the first press.
            if !input.repeat {
                self.last_press = 0;
                self.release(false, effects);
            }
            return false;
        }
        if input.repeat {
            if self.mode == Some(Mode::Hold) && self.armed && now.saturating_sub(self.held_since) >= HOLD_MS {
                self.armed = false;
                self.quit_on_release = true;
                self.quit_after_quiet_period(effects);
            }
            return true;
        }
        let previous = self.last_press;
        self.last_press = now;
        if self.holding || self.notified {
            self.release(false, effects);
        }
        // Every mode accepts two presses.
        if previous != 0 && now.saturating_sub(previous) <= DOUBLE_PRESS_MS {
            self.quit_now(effects);
            return true;
        }
        self.holding = true;
        self.held_since = now;
        match mode {
            Mode::Direct => self.quit_now(effects),
            Mode::DoubleClick => {
                self.mode = Some(mode);
                self.notified = true;
                effects.push(Effect::Hint(Hint::Down(mode)));
                self.watch(Watch::Release, DOUBLE_PRESS_MS, effects);
            }
            Mode::Hold => {
                self.mode = Some(mode);
                self.notified = true;
                effects.push(Effect::Hint(Hint::Down(mode)));
                self.armed = true;
                // No repeats by then: the key was let go (its release possibly unseen), or repeat is off.
                self.watch(Watch::Release, HOLD_MS + RELEASE_GRACE_MS, effects);
            }
        }
        true
    }

    fn watch(&mut self, watch: Watch, after: u64, effects: &mut Vec<Effect>) {
        self.watchdog = Some(watch);
        effects.push(Effect::Arm(after));
    }

    fn clear_watchdog(&mut self, effects: &mut Vec<Effect>) {
        if self.watchdog.take().is_some() {
            effects.push(Effect::Disarm);
        }
    }

    /// Ends the press. A release the page saw keeps a double-press hint up for the rest of its window.
    fn release(&mut self, keep_double_hint: bool, effects: &mut Vec<Effect>) {
        if !self.holding && !self.notified {
            return;
        }
        let keep = keep_double_hint && self.mode == Some(Mode::DoubleClick) && self.notified;
        self.holding = false;
        self.armed = false;
        self.quit_on_release = false;
        self.last_repeat = 0;
        self.repeat_cadence = 0;
        if keep {
            return;
        }
        self.mode = None;
        self.clear_watchdog(effects);
        if self.notified {
            self.notified = false;
            effects.push(Effect::Hint(Hint::Up));
        }
    }

    /// The hint is taken down first so a quit that is cancelled cannot leave it up.
    fn quit_now(&mut self, effects: &mut Vec<Effect>) {
        self.release(false, effects);
        self.last_press = 0;
        effects.push(Effect::Quit);
    }

    fn quit_after_quiet_period(&mut self, effects: &mut Vec<Effect>) {
        self.clear_watchdog(effects);
        let quiet = RELEASE_GRACE_MS.max(self.repeat_cadence * 2);
        self.watch(Watch::Quit, quiet, effects);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: &str, down: bool, repeat: bool) -> Key {
        Key { down, key: key.into(), meta: true, control: false, alt: false, shift: false, repeat }
    }
    fn q_down() -> Key {
        press("q", true, false)
    }
    fn q_repeat() -> Key {
        press("q", true, true)
    }
    fn q_up() -> Key {
        press("q", false, false)
    }
    /// A clock the test moves by hand, so no test waits for a duration to pass.
    struct Run {
        hold: QuitHold,
        now: u64,
        quit: bool,
        hints: Vec<Hint>,
        armed: Option<u64>,
        deadline: Option<u64>,
    }
    impl Run {
        fn new() -> Self {
            Self { hold: QuitHold::new(true), now: 1_000, quit: false, hints: Vec::new(), armed: None, deadline: None }
        }
        fn apply(&mut self, effects: Vec<Effect>) {
            for effect in effects {
                match effect {
                    Effect::Quit => self.quit = true,
                    Effect::Hint(hint) => self.hints.push(hint),
                    Effect::Arm(after) => {
                        self.armed = Some(after);
                        self.deadline = Some(self.now + after);
                    }
                    Effect::Disarm => {
                        self.armed = None;
                        self.deadline = None;
                    }
                }
            }
        }
        fn send(&mut self, key: Key, mode: Mode) -> bool {
            let outcome = self.hold.key(&key, self.now, mode);
            self.apply(outcome.effects);
            outcome.swallow
        }
        /// Moves the clock, running the timer if its time comes.
        fn advance(&mut self, by: u64) {
            let target = self.now + by;
            if let Some(deadline) = self.deadline.filter(|deadline| *deadline <= target) {
                self.now = deadline;
                self.deadline = None;
                self.armed = None;
                let effects = self.hold.timer();
                self.apply(effects);
            }
            self.now = target;
        }
    }

    #[test]
    fn direct_mode_quits_on_the_first_press() {
        let mut run = Run::new();
        assert!(run.send(q_down(), Mode::Direct));
        assert!(run.quit);
        assert!(run.hints.is_empty());
    }

    #[test]
    fn two_presses_within_the_window_quit_in_every_mode_and_a_slow_second_press_starts_over() {
        for mode in [Mode::Hold, Mode::DoubleClick] {
            let mut run = Run::new();
            run.send(q_down(), mode);
            run.advance(100);
            run.send(q_up(), mode);
            run.advance(200);
            run.send(q_down(), mode);
            assert!(run.quit, "{mode:?}");
        }
        let mut run = Run::new();
        run.send(q_down(), Mode::DoubleClick);
        assert_eq!(run.hints, [Hint::Down(Mode::DoubleClick)]);
        // The window closes with the hint, and the next press is a first press again.
        run.advance(DOUBLE_PRESS_MS);
        assert_eq!(run.hints, [Hint::Down(Mode::DoubleClick), Hint::Up]);
        run.advance(100);
        run.send(q_down(), Mode::DoubleClick);
        assert!(!run.quit);
        assert_eq!(run.hints.last(), Some(&Hint::Down(Mode::DoubleClick)));
    }

    #[test]
    fn holding_shows_the_hint_and_quits_when_released_after_the_hold_time() {
        let mut run = Run::new();
        run.send(q_down(), Mode::Hold);
        assert_eq!(run.hints, [Hint::Down(Mode::Hold)]);
        assert_eq!(run.armed, Some(HOLD_MS + RELEASE_GRACE_MS));
        // Auto-repeat every 33 ms; before the hold time nothing quits.
        while run.now - 1_000 < HOLD_MS - 50 {
            run.advance(33);
            assert!(run.send(q_repeat(), Mode::Hold));
        }
        assert!(!run.quit);
        // Past the hold time the next repeat arms the quit, which waits for the release.
        run.advance(60);
        assert!(run.send(q_repeat(), Mode::Hold));
        assert!(!run.quit);
        run.advance(20);
        run.send(q_up(), Mode::Hold);
        assert!(run.quit);
        assert_eq!(run.hints.last(), Some(&Hint::Up));
    }

    #[test]
    fn a_hold_whose_release_is_never_seen_quits_after_the_repeats_stop() {
        let mut run = Run::new();
        run.send(q_down(), Mode::Hold);
        for _ in 0..40 {
            run.advance(33);
            run.send(q_repeat(), Mode::Hold);
        }
        assert!(!run.quit, "still held: repeats keep pushing the quit back");
        // The page never sees the letter's release; the quiet period after the last repeat quits.
        run.advance(RELEASE_GRACE_MS - 1);
        assert!(!run.quit);
        run.advance(1);
        assert!(run.quit);
    }

    #[test]
    fn a_slow_repeat_rate_waits_two_intervals_so_the_next_repeat_is_not_mistaken_for_a_release() {
        let mut run = Run::new();
        run.send(q_down(), Mode::Hold);
        for _ in 0..8 {
            run.advance(400);
            run.send(q_repeat(), Mode::Hold);
        }
        assert!(run.now - 1_000 >= HOLD_MS);
        assert_eq!(run.armed, Some(800), "twice the 400 ms between repeats, more than the 600 ms grace");
        run.advance(799);
        assert!(!run.quit);
        run.advance(1);
        assert!(run.quit);
    }

    #[test]
    fn letting_go_early_or_pressing_anything_else_cancels_without_quitting() {
        let mut run = Run::new();
        run.send(q_down(), Mode::Hold);
        run.advance(300);
        run.send(q_up(), Mode::Hold);
        assert!(!run.quit);
        assert_eq!(run.hints, [Hint::Down(Mode::Hold), Hint::Up]);
        run.advance(HOLD_MS * 3);
        assert!(!run.quit);

        // No repeats at all (repeat turned off): the hold times out.
        let mut run = Run::new();
        run.send(q_down(), Mode::Hold);
        run.advance(HOLD_MS + RELEASE_GRACE_MS);
        assert!(!run.quit);
        assert_eq!(run.hints, [Hint::Down(Mode::Hold), Hint::Up]);

        // Another key ends the hold and also the first press of a double press.
        let mut run = Run::new();
        run.send(q_down(), Mode::Hold);
        run.send(press("a", true, false), Mode::Hold);
        assert_eq!(run.hints, [Hint::Down(Mode::Hold), Hint::Up]);
        run.send(q_up(), Mode::Hold);
        run.send(q_down(), Mode::Hold);
        assert!(!run.quit, "the earlier press was cancelled");
    }

    #[test]
    fn the_modifier_alone_does_not_cancel_a_double_press_and_unrelated_keys_are_not_swallowed() {
        let mut run = Run::new();
        run.send(q_down(), Mode::DoubleClick);
        run.advance(50);
        run.send(q_up(), Mode::DoubleClick);
        run.send(press("meta", false, false), Mode::DoubleClick);
        run.send(press("meta", true, false), Mode::DoubleClick);
        run.advance(50);
        run.send(q_down(), Mode::DoubleClick);
        assert!(run.quit);
        // A plain letter without the modifier belongs to the page.
        let mut run = Run::new();
        let plain = Key { meta: false, ..press("q", true, false) };
        assert!(!run.send(plain, Mode::Hold));
        assert!(run.hints.is_empty());
        // Command with shift is another shortcut.
        let shifted = Key { shift: true, ..press("q", true, false) };
        assert!(!run.send(shifted, Mode::Hold));
    }

    #[test]
    fn other_platforms_use_the_control_key() {
        let mut run = Run::new();
        run.hold = QuitHold::new(false);
        let control = |key: &str, down, repeat| Key { down, key: key.into(), meta: false, control: true, alt: false, shift: false, repeat };
        assert!(run.send(control("q", true, false), Mode::Direct));
        assert!(run.quit);
    }
}
