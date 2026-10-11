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

/// Whether `c` is trimmed by JavaScript's `String.prototype.trim` (WhiteSpace and LineTerminator).
/// Not the same set as Rust's `char::is_whitespace`: JS trims U+FEFF and keeps U+0085.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'
            | '\u{a}'
            | '\u{b}'
            | '\u{c}'
            | '\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// JavaScript's `String.prototype.trim`.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// JavaScript's `String(number)` for a finite number.
pub fn js_number_to_string(value: f64) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    if !value.is_finite() {
        return if value.is_nan() {
            "NaN".to_owned()
        } else if value > 0.0 {
            "Infinity".to_owned()
        } else {
            "-Infinity".to_owned()
        };
    }
    let magnitude = value.abs();
    if (1e-6..1e21).contains(&magnitude) {
        // Rust prints the shortest representation that round-trips, as JavaScript does, and
        // never switches to an exponent in `{}`.
        return format!("{value}");
    }
    // Exponent form: `1e+21`, `1.5e-7`.
    let scientific = format!("{value:e}");
    match scientific.split_once('e') {
        Some((mantissa, exponent)) if !exponent.starts_with('-') => {
            format!("{mantissa}e+{exponent}")
        }
        _ => scientific,
    }
}

/// The inside of a regex character class matching exactly JavaScript's `\s` (not Rust's, which
/// keeps U+0085 and lacks U+FEFF).
pub const JS_WHITESPACE_CLASS: &str = "\\t\\n\\x0b\\x0c\\r \\u{a0}\\u{1680}\\u{2000}-\\u{200a}\\u{2028}\\u{2029}\\u{202f}\\u{205f}\\u{3000}\\u{feff}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_trim_differs_from_rust_trim_where_javascript_does() {
        assert_eq!(js_trim("\u{feff} x \u{2028}"), "x");
        assert_eq!(js_trim("\u{85}x\u{85}"), "\u{85}x\u{85}");
        assert_eq!("\u{85}x\u{85}".trim(), "x");
        assert_eq!(js_trim("\u{a0}\u{3000}x"), "x");
    }

    #[test]
    fn numbers_print_like_javascript() {
        for (value, text) in [
            (0.0, "0"),
            (-0.0, "0"),
            (3773.0, "3773"),
            (3773.5, "3773.5"),
            (-1.0, "-1"),
            (1e21, "1e+21"),
            (1.5e-7, "1.5e-7"),
            (123456789012345680000.0, "123456789012345680000"),
            (0.000001, "0.000001"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
        ] {
            assert_eq!(js_number_to_string(value), text, "{value}");
        }
    }
}
