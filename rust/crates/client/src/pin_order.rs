//! Pinned-thread order: fractional keys, so moving one pin writes one key to one thread and every
//! client of the server converges on the same order. Port of the pin-order half of
//! `client-runtime/src/state/threadSort.ts`, replayed against it by `tests/pin_order_parity.rs`.
use std::collections::{BTreeMap, BTreeSet};

const DIGITS: &str = "abcdefghijklmnopqrstuvwxyz";

fn valid(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| DIGITS.contains(c)) && !key.ends_with('a')
}
fn digit(text: &str, at: usize) -> Option<char> {
    text.chars().nth(at)
}
/// Midpoint of two digit strings read as fractions in (0, 1); `""` is the open bound. Needs a < b.
fn midpoint(a: &str, b: &str) -> String {
    if !b.is_empty() {
        let mut n = 0;
        while digit(a, n).unwrap_or('a') == digit(b, n).unwrap_or('\0') {
            n += 1;
        }
        if n > 0 {
            return format!("{}{}", b.chars().take(n).collect::<String>(), midpoint(&a.chars().skip(n).collect::<String>(), &b.chars().skip(n).collect::<String>()));
        }
    }
    let index = |c: Option<char>| c.and_then(|c| DIGITS.find(c)).unwrap_or(0);
    let digit_a = if a.is_empty() { 0 } else { index(digit(a, 0)) };
    let digit_b = if b.is_empty() { DIGITS.len() } else { index(digit(b, 0)) };
    if digit_b - digit_a > 1 {
        // `Math.round` of a half goes up.
        return DIGITS.chars().nth((digit_a + digit_b).div_ceil(2)).map(String::from).unwrap_or_default();
    }
    if b.chars().count() > 1 {
        return digit(b, 0).map(String::from).unwrap_or_default();
    }
    format!("{}{}", DIGITS.chars().nth(digit_a).unwrap_or('a'), midpoint(&a.chars().skip(1).collect::<String>(), ""))
}
/// A key that sorts strictly between two neighbours, or `None` when the existing keys are corrupt
/// or out of order, in which case the caller rewrites the section.
pub fn key_between(before: Option<&str>, after: Option<&str>) -> Option<String> {
    let (a, b) = (before.unwrap_or(""), after.unwrap_or(""));
    if (!a.is_empty() && !valid(a)) || (!b.is_empty() && !valid(b)) || (!b.is_empty() && a >= b) {
        return None;
    }
    Some(midpoint(a, b))
}
/// Evenly spaced keys for materialising an order.
pub fn spread_keys(count: usize) -> Vec<String> {
    let base = DIGITS.len() as f64;
    let mut width = 2;
    let mut space = base.powi(width);
    while space <= ((count + 1) * 2) as f64 {
        width += 1;
        space *= base;
    }
    let step = space / (count + 1) as f64;
    (0..count)
        .map(|i| {
            // `Math.round`: halves go up.
            let mut value = (step * (i + 1) as f64 + 0.5).floor() as u64;
            if value % DIGITS.len() as u64 == 0 {
                value += 1;
            }
            let mut key = String::new();
            for _ in 0..width {
                key.insert(0, DIGITS.chars().nth((value % DIGITS.len() as u64) as usize).unwrap_or('a'));
                value /= DIGITS.len() as u64;
            }
            key
        })
        .collect()
}
/// The writes that realise a new order: one key for the moved thread when its neighbours have
/// keys, else fresh spread keys for the whole section.
pub fn plan_reorder(ordered: &[String], keys: &BTreeMap<String, Option<String>>, moved: &str) -> Vec<(String, String)> {
    let visible: BTreeSet<&str> = ordered.iter().map(String::as_str).collect();
    let reserved: BTreeSet<&str> = keys.iter().filter(|(id, key)| !visible.contains(id.as_str()) && key.is_some()).filter_map(|(_, key)| key.as_deref()).collect();
    let Some(at) = ordered.iter().position(|id| id == moved) else { return Vec::new() };
    let before = if at > 0 { Some(&ordered[at - 1]) } else { None };
    let after = ordered.get(at + 1);
    let key_of = |id: Option<&String>| id.and_then(|id| keys.get(id).cloned().flatten());
    let (before_key, after_key) = (key_of(before), key_of(after));
    if (before.is_none() || before_key.is_some()) && (after.is_none() || after_key.is_some()) {
        let mut key = key_between(before_key.as_deref(), after_key.as_deref());
        while let Some(current) = key.clone().filter(|key| reserved.contains(key.as_str())) {
            key = key_between(Some(&current), after_key.as_deref());
        }
        if let Some(key) = key {
            return vec![(moved.to_owned(), key)];
        }
    }
    // A neighbour without a key (or corrupt keys): the whole section is written in the new order.
    let fresh: Vec<String> = spread_keys(ordered.len() + reserved.len()).into_iter().filter(|key| !reserved.contains(key.as_str())).take(ordered.len()).collect();
    ordered.iter().enumerate().filter_map(|(index, id)| (keys.get(id).cloned().flatten().as_deref() != Some(fresh[index].as_str())).then(|| (id.clone(), fresh[index].clone()))).collect()
}
/// The order after moving the thread at `from` one place up or down within `ids`.
pub fn moved(ids: &[String], moved: &str, up: bool) -> Option<Vec<String>> {
    let at = ids.iter().position(|id| id == moved)?;
    let to = if up { at.checked_sub(1)? } else { (at + 1 < ids.len()).then_some(at + 1)? };
    let mut next = ids.to_vec();
    next.swap(at, to);
    Some(next)
}
