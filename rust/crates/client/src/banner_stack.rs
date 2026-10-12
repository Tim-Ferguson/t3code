//! Which notice is in front of the composer and which wait behind it: the ordering of `ComposerBannerStack.tsx` (`bannerPriority` and the stable sort
//! that follows it). The stack itself, with the peek and the expanded group, is `composer_banners.rs` of the `ui` crate.

/// How soon a notice is shown (`bannerPriority`): work in progress stays attached to the composer; urgency and severity only order the
/// notices behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Activity,
    Urgent,
    Notice,
}

/// `bannerPriority` for a notice that says so (`priority`) or has only a variant: an error or a warning is urgent whatever it says.
pub fn priority(explicit: Option<Priority>, urgent: bool, variant: &str) -> Priority {
    match explicit {
        Some(Priority::Activity) => Priority::Activity,
        _ if urgent || matches!(variant, "error" | "warning") => Priority::Urgent,
        Some(other) => other,
        None => Priority::Notice,
    }
}

/// One notice the stack holds. `slot` is where its source stands among the stack's sources and `seq` the order it appeared in within it:
/// together they stand for the order the original passes its items in, which a tie in priority keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub id: u64,
    pub slot: u32,
    pub seq: u64,
    pub priority: Priority,
}

/// The notices front first (`items.toSorted(...)`, which is stable): the first is the one attached to the composer, the rest are behind it.
pub fn sorted(entries: &[Entry]) -> Vec<Entry> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|entry| (entry.priority, entry.slot, entry.seq));
    sorted
}

/// Where a notice sits among the others, and whether it is the front: its index in the sorted order. The stack draws the notices behind the front
/// above it, in that order, and the front last.
pub fn position(entries: &[Entry], id: u64) -> Option<(usize, bool)> {
    let order = sorted(entries);
    let at = order.iter().position(|entry| entry.id == id)?;
    Some((at, at == 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, slot: u32, seq: u64, priority: Priority) -> Entry {
        Entry { id, slot, seq, priority }
    }

    #[test]
    fn work_in_progress_leads_then_urgent_notices_then_the_rest_and_a_tie_keeps_the_order_given() {
        let entries = [
            entry(1, 0, 1, Priority::Notice),
            entry(2, 1, 2, Priority::Urgent),
            entry(3, 2, 3, Priority::Activity),
            entry(4, 1, 4, Priority::Urgent),
            entry(5, 0, 5, Priority::Notice),
            entry(6, 3, 6, Priority::Activity),
        ];
        let ids: Vec<u64> = sorted(&entries).iter().map(|entry| entry.id).collect();
        assert_eq!(ids, [3, 6, 2, 4, 1, 5]);
        assert_eq!(position(&entries, 3), Some((0, true)));
        assert_eq!(position(&entries, 1), Some((4, false)));
        assert_eq!(position(&entries, 99), None);
        assert_eq!(position(&entries[..1], 1), Some((0, true)), "a lone notice is the front");
    }

    #[test]
    fn an_error_or_a_warning_is_urgent_and_activity_stays_activity() {
        assert_eq!(priority(None, false, "default"), Priority::Notice);
        assert_eq!(priority(None, true, "info"), Priority::Urgent);
        assert_eq!(priority(None, false, "error"), Priority::Urgent);
        assert_eq!(priority(None, false, "warning"), Priority::Urgent);
        assert_eq!(priority(Some(Priority::Activity), false, "warning"), Priority::Activity);
        assert_eq!(priority(Some(Priority::Urgent), false, "info"), Priority::Urgent);
        assert_eq!(priority(Some(Priority::Notice), false, "info"), Priority::Notice);
    }
}
