//! `$skill` in a sent message: the words of a message that name one of the provider's skills are drawn as a chip with the skill's
//! display name (`SkillInlineText.tsx`). A word counts when a currency sign starts it at the start of the text or after
//! whitespace, a letter-bearing name of letters, digits, `:`, `_` and `-` follows, whitespace or the end closes it, and the name is
//! not an amount (`$5`, `$2.5k`, `$1e6`). A name that is not a skill of the provider stays text.

/// A stretch of a message's text, or a skill named in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    /// The chip for `raw` (the text as written, `$name`), labelled with the skill's name for display.
    Skill { raw: String, label: String },
}

/// Whether a currency sign (`\p{Sc}`) stands here.
fn is_currency(character: char) -> bool {
    matches!(character, '$' | '\u{a2}'..='\u{a5}' | '\u{58f}' | '\u{60b}' | '\u{7fe}' | '\u{7ff}' | '\u{9f2}' | '\u{9f3}' | '\u{9fb}' | '\u{af1}' | '\u{bf9}' | '\u{e3f}' | '\u{17db}' | '\u{20a0}'..='\u{20c1}' | '\u{a838}' | '\u{fdfc}' | '\u{fe69}' | '\u{ff04}' | '\u{ffe0}' | '\u{ffe1}' | '\u{ffe5}' | '\u{ffe6}')
}
fn name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, ':' | '_' | '-')
}

/// The name's amount shape (`(?![0-9][0-9_]*(?:[kKmMbBtT]|[eE][0-9]+)?(?:\s|$))`): digits and underscores, then an optional magnitude
/// suffix or exponent, and nothing else before the end of the word.
fn looks_like_amount(rest: &str) -> bool {
    let mut characters = rest.char_indices().peekable();
    if !characters.next().is_some_and(|(_, first)| first.is_ascii_digit()) {
        return false;
    }
    let mut end = 1;
    while let Some(&(index, character)) = characters.peek() {
        if character.is_ascii_digit() || character == '_' {
            end = index + character.len_utf8();
            characters.next();
        } else {
            break;
        }
    }
    let tail = &rest[end..];
    let after = if let Some(suffix) = tail.strip_prefix(|c: char| "kKmMbBtT".contains(c)) {
        suffix
    } else if let Some(exponent) = tail.strip_prefix(['e', 'E']).filter(|exponent| exponent.starts_with(|c: char| c.is_ascii_digit())) {
        exponent.trim_start_matches(|c: char| c.is_ascii_digit())
    } else {
        tail
    };
    after.is_empty() || after.starts_with(char::is_whitespace)
}

/// `SkillInlineText`: splits `text` at the skills it names. `skill_label` gives the label of a skill by name, or none for a name that is
/// not one.
pub fn split(text: &str, skill_label: &dyn Fn(&str) -> Option<String>) -> Vec<Piece> {
    let mut pieces = vec![];
    let mut cursor = 0;
    let mut previous_space = true;
    let mut chars = text.char_indices();
    while let Some((start, character)) = chars.next() {
        let at_boundary = previous_space;
        previous_space = character.is_whitespace();
        if !(at_boundary && is_currency(character)) {
            continue;
        }
        let rest = &text[start + character.len_utf8()..];
        let name_end = rest.find(|c: char| !name_character(c)).unwrap_or(rest.len());
        let name = &rest[..name_end];
        let word_closed = rest[name_end..].is_empty() || rest[name_end..].starts_with(char::is_whitespace);
        let first_ok = name.chars().next().is_some_and(|first| first.is_ascii_alphanumeric());
        if !word_closed || !first_ok || !name.chars().any(|c| c.is_ascii_alphabetic()) || looks_like_amount(name) {
            continue;
        }
        let Some(label) = skill_label(name) else { continue };
        if start > cursor {
            pieces.push(Piece::Text(text[cursor..start].to_owned()));
        }
        pieces.push(Piece::Skill { raw: format!("{character}{name}"), label });
        cursor = start + character.len_utf8() + name.len();
        // The scan resumes after the token, which is followed by whitespace or the end.
        for _ in name.chars() {
            chars.next();
        }
        previous_space = false;
    }
    if pieces.is_empty() {
        return vec![Piece::Text(text.to_owned())];
    }
    if cursor < text.len() {
        pieces.push(Piece::Text(text[cursor..].to_owned()));
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labelled(name: &str) -> Option<String> {
        (name == "code-review" || name == "plugin:fmt").then(|| name.replace(['-', ':'], " "))
    }

    #[test]
    fn a_skill_the_provider_has_becomes_a_chip_between_the_words_around_it() {
        assert_eq!(
            split("please $code-review this", &labelled),
            vec![Piece::Text("please ".into()), Piece::Skill { raw: "$code-review".into(), label: "code review".into() }, Piece::Text(" this".into())]
        );
        assert_eq!(split("$code-review", &labelled), vec![Piece::Skill { raw: "$code-review".into(), label: "code review".into() }]);
        assert_eq!(split("a $plugin:fmt\nb $code-review", &labelled).len(), 4);
    }

    #[test]
    fn what_is_not_a_known_skill_or_not_a_word_of_its_own_stays_text() {
        for text in ["$unknown skill", "cost$code-review here", "$code-review. next", "pay $5 and $2.5k and $1e6 and $100", "price $code-reviewer", "no skill here", ""] {
            assert_eq!(split(text, &labelled), vec![Piece::Text(text.to_owned())], "{text:?}");
        }
        // Other currency signs open a token too, and the raw text keeps the sign.
        assert_eq!(split("\u{20ac}code-review", &labelled), vec![Piece::Skill { raw: "\u{20ac}code-review".into(), label: "code review".into() }]);
    }

    #[test]
    fn an_amount_is_digits_with_a_magnitude_or_exponent_and_nothing_after() {
        for amount in ["5", "100", "1_000", "5k", "5M", "2e6", "7B"] {
            assert!(looks_like_amount(amount), "{amount}");
        }
        for word in ["5x", "5kb", "1e", "a5", "2e6x", "5k5"] {
            assert!(!looks_like_amount(word), "{word}");
        }
    }
}
