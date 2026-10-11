//! Small JavaScript-compatible helpers shared by the ports.

/// JavaScript's `Number.parseInt(part, 10)`: the leading integer, or `None` (NaN).
pub fn js_parse_int(part: &str) -> Option<i64> {
    let trimmed = part.trim_start();
    let (sign, rest) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<i64>().ok().map(|n| sign * n)
}
