//! Saving an edited file: edits are written a moment after they stop, the newest contents win, an edit made while a write is
//! in flight is written after it, an edit left in the debounce window is written when the editor closes, a failed write
//! leaves the file pending, and nothing is written without permission. Port of `fileSaveCoordinator.ts`, as a machine with
//! no clock and no I/O: it says what to do (`Effect`), the caller schedules, writes and reports back.
//!
//! The original's `onConfirmed` can refuse a confirmation when another editor holds newer unsaved contents; only one editor
//! of a file exists here, so a write that succeeded is confirmed.

/// What the caller does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// The file has (true) or no longer has (false) edits that are not on disk.
    Pending(bool),
    /// Call `timer` after this many milliseconds, replacing any timer that is waiting.
    Schedule(u64),
    /// Write these contents, then report with `written`.
    Persist { revision: u64, contents: String },
    /// These contents are on disk.
    Confirmed(String),
}

pub const DEBOUNCE_MS: u64 = 500;

#[derive(Debug)]
pub struct Saver {
    debounce_ms: u64,
    latest: String,
    latest_revision: u64,
    confirmed_revision: u64,
    last_change_ms: i64,
    /// The contents of the write in flight.
    in_flight: Option<String>,
    disposed: bool,
}

impl Saver {
    pub fn new(debounce_ms: u64) -> Self {
        Self { debounce_ms, latest: String::new(), latest_revision: 0, confirmed_revision: 0, last_change_ms: 0, in_flight: None, disposed: false }
    }

    /// The contents now in the editor.
    pub fn change(&mut self, contents: String, now_ms: i64) -> Vec<Effect> {
        if self.disposed {
            return vec![];
        }
        self.latest = contents;
        self.latest_revision += 1;
        self.last_change_ms = now_ms;
        vec![Effect::Pending(true), Effect::Schedule(self.debounce_ms)]
    }

    /// The editor closed: what is not on disk is written now.
    pub fn dispose(&mut self, can_persist: bool) -> Vec<Effect> {
        self.disposed = true;
        if self.latest_revision > 0 { self.persist_latest(can_persist) } else { vec![] }
    }

    /// The timer of `Schedule` fired.
    pub fn timer(&mut self, can_persist: bool) -> Vec<Effect> {
        self.persist_latest(can_persist)
    }

    fn persist_latest(&mut self, can_persist: bool) -> Vec<Effect> {
        if self.in_flight.is_some() || self.latest_revision == self.confirmed_revision || !can_persist {
            return vec![];
        }
        self.in_flight = Some(self.latest.clone());
        vec![Effect::Persist { revision: self.latest_revision, contents: self.latest.clone() }]
    }

    /// The write of `revision` ended.
    pub fn written(&mut self, revision: u64, succeeded: bool, now_ms: i64, can_persist: bool) -> Vec<Effect> {
        let mut effects = vec![];
        let written = self.in_flight.take().unwrap_or_default();
        if succeeded {
            self.confirmed_revision = revision;
            effects.push(Effect::Confirmed(written));
        }
        if revision == self.latest_revision {
            if succeeded {
                effects.push(Effect::Pending(false));
            }
            return effects;
        }
        // Edited during the write: the rest waits out what is left of the debounce, or goes at once when the editor is gone.
        let remaining = (self.debounce_ms as i64 - (now_ms - self.last_change_ms)).max(0) as u64;
        if self.disposed {
            effects.extend(self.persist_latest(can_persist));
        } else {
            effects.push(Effect::Schedule(remaining));
        }
        effects
    }

    pub fn has_unsaved(&self) -> bool {
        self.latest_revision != self.confirmed_revision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persists(effects: &[Effect]) -> Vec<(u64, &str)> {
        effects.iter().filter_map(|effect| if let Effect::Persist { revision, contents } = effect { Some((*revision, contents.as_str())) } else { None }).collect()
    }

    #[test]
    fn edits_are_written_once_after_they_stop_and_only_the_latest_contents() {
        let mut saver = Saver::new(500);
        assert_eq!(saver.change("first".into(), 0), [Effect::Pending(true), Effect::Schedule(500)]);
        assert_eq!(saver.change("latest".into(), 300), [Effect::Pending(true), Effect::Schedule(500)], "the timer is replaced, not stacked");
        let fired = saver.timer(true);
        assert_eq!(persists(&fired), [(2, "latest")]);
        let done = saver.written(2, true, 800, true);
        assert_eq!(done, [Effect::Confirmed("latest".into()), Effect::Pending(false)]);
        assert!(!saver.has_unsaved());
        assert!(saver.timer(true).is_empty(), "nothing left to write");
    }

    #[test]
    fn an_edit_made_during_a_write_stays_pending_until_it_is_written_too() {
        let mut saver = Saver::new(500);
        saver.change("first".into(), 0);
        assert_eq!(persists(&saver.timer(true)), [(1, "first")]);
        saver.change("latest".into(), 500);
        assert!(saver.timer(true).is_empty(), "a write is in flight; one at a time");
        let after_first = saver.written(1, true, 600, true);
        assert!(!after_first.contains(&Effect::Pending(false)), "still pending: {after_first:?}");
        assert_eq!(after_first.last(), Some(&Effect::Schedule(400)), "what is left of the debounce");
        assert_eq!(persists(&saver.timer(true)), [(2, "latest")]);
        assert!(saver.written(2, true, 1000, true).contains(&Effect::Pending(false)));
    }

    #[test]
    fn closing_the_editor_writes_what_is_not_on_disk_once_and_what_a_write_in_flight_left() {
        let mut saver = Saver::new(500);
        saver.change("unsaved".into(), 0);
        assert_eq!(persists(&saver.dispose(true)), [(1, "unsaved")]);
        assert!(saver.change("ignored".into(), 1).is_empty(), "a closed editor takes no more edits");

        // Edited during a write, then closed: the write that lands is followed by the newest contents, at once.
        let mut saver = Saver::new(500);
        saver.change("first".into(), 0);
        saver.timer(true);
        saver.change("latest".into(), 500);
        assert!(saver.dispose(true).is_empty(), "the write in flight is not doubled");
        assert_eq!(persists(&saver.written(1, true, 510, true)), [(2, "latest")]);

        // A write that lands while the editor closes is not written again.
        let mut saver = Saver::new(500);
        saver.change("only".into(), 0);
        saver.timer(true);
        assert!(saver.dispose(true).is_empty());
        let done = saver.written(1, true, 600, true);
        assert!(persists(&done).is_empty() && done.contains(&Effect::Pending(false)));

        // Nothing was edited: nothing is written.
        assert!(Saver::new(500).dispose(true).is_empty());
    }

    #[test]
    fn a_failed_write_leaves_the_file_pending_and_closing_tries_again() {
        let mut saver = Saver::new(500);
        saver.change("latest".into(), 0);
        saver.timer(true);
        let failed = saver.written(1, false, 500, true);
        assert!(failed.is_empty(), "no confirmation, and pending is not cleared: {failed:?}");
        assert!(saver.has_unsaved());
        assert_eq!(persists(&saver.dispose(true)), [(1, "latest")]);
    }

    #[test]
    fn without_permission_nothing_is_written_and_the_edit_stays_pending() {
        for closing in [false, true] {
            let mut saver = Saver::new(500);
            saver.change("unsaved".into(), 0);
            let effects = if closing { saver.dispose(false) } else { saver.timer(false) };
            assert!(effects.is_empty(), "closing={closing}: {effects:?}");
            assert!(saver.has_unsaved());
        }
    }
}
