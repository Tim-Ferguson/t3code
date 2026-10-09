//! Moving between threads by keyboard. Port of `resolveAdjacentThreadId` and the jump commands.

/// The thread before or after `current` in the sidebar's order. With no current thread, next is
/// the first and previous the last. It stops at either end rather than wrapping, and finds
/// nothing if the current thread is not in the list.
pub fn adjacent<'a>(ids: &'a [String], current: Option<&str>, forward: bool) -> Option<&'a str> {
    if ids.is_empty() {
        return None;
    }
    let Some(current) = current else {
        return if forward { ids.first() } else { ids.last() }.map(String::as_str);
    };
    let index = ids.iter().position(|id| id == current)?;
    if forward { ids.get(index + 1) } else { index.checked_sub(1).and_then(|previous| ids.get(previous)) }.map(String::as_str)
}
/// `thread.jump.N` (1 to 9) as a zero-based index.
pub fn jump_index(command: &str) -> Option<usize> {
    let number: usize = command.strip_prefix("thread.jump.")?.parse().ok()?;
    (1..=9).contains(&number).then(|| number - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        ["a", "b", "c"].map(String::from).to_vec()
    }
    #[test]
    fn traversal_starts_at_the_ends_stops_there_and_ignores_unknown_threads() {
        let ids = ids();
        assert_eq!((adjacent(&ids, None, true), adjacent(&ids, None, false)), (Some("a"), Some("c")));
        assert_eq!((adjacent(&ids, Some("a"), true), adjacent(&ids, Some("b"), true), adjacent(&ids, Some("c"), true)), (Some("b"), Some("c"), None));
        assert_eq!((adjacent(&ids, Some("a"), false), adjacent(&ids, Some("c"), false)), (None, Some("b")));
        assert_eq!(adjacent(&ids, Some("zzz"), true), None);
        assert_eq!(adjacent(&[], None, true), None);
    }
    #[test]
    fn jump_commands_are_one_to_nine() {
        assert_eq!((jump_index("thread.jump.1"), jump_index("thread.jump.9"), jump_index("thread.jump.0"), jump_index("thread.jump.10"), jump_index("thread.next")), (Some(0), Some(8), None, None, None));
    }
}
