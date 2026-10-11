//! What the composer checks before it lets a prompt go (`composerSubmission.ts`): the provider takes at most
//! `PROVIDER_SEND_TURN_MAX_INPUT_CHARS` UTF-16 units of prompt, and a longer one is refused with the reader told by how much.
use t3_contracts::PROVIDER_SEND_TURN_MAX_INPUT_CHARS;

/// `getComposerPromptLengthValidationMessage`. The length is the trimmed prompt's, in UTF-16 units as the original counts them.
// TODO(original assistant citations): the original also measures the prompt with its assistant citations expanded for the
// provider (`expandAssistantCitationsForProvider`) and takes the larger; this composer has no citations to expand.
pub fn prompt_length_message(prompt: &str) -> Option<String> {
    let length = prompt.trim().encode_utf16().count();
    let excess = length.checked_sub(PROVIDER_SEND_TURN_MAX_INPUT_CHARS).filter(|excess| *excess > 0)?;
    Some(format!(
        "Prompt is {} {} over the {}-character limit. Shorten or split it before sending.",
        grouped(excess),
        if excess == 1 { "character" } else { "characters" },
        grouped(PROVIDER_SEND_TURN_MAX_INPUT_CHARS)
    ))
}

/// `toLocaleString("en-US")` for a count.
fn grouped(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_over_the_limit_is_refused_with_the_excess_in_the_originals_words() {
        let limit = PROVIDER_SEND_TURN_MAX_INPUT_CHARS;
        assert_eq!(prompt_length_message(&"a".repeat(limit)), None);
        assert_eq!(prompt_length_message(&format!("  {}  \n", "a".repeat(limit))), None, "the prompt is trimmed first");
        assert_eq!(prompt_length_message(&"a".repeat(limit + 1)).as_deref(), Some("Prompt is 1 character over the 120,000-character limit. Shorten or split it before sending."));
        assert_eq!(prompt_length_message(&"a".repeat(limit + 1234)).as_deref(), Some("Prompt is 1,234 characters over the 120,000-character limit. Shorten or split it before sending."));
        // Units are UTF-16: a character outside the basic plane counts twice.
        assert_eq!(prompt_length_message(&format!("{}\u{1F600}", "a".repeat(limit - 1))).as_deref(), Some("Prompt is 1 character over the 120,000-character limit. Shorten or split it before sending."));
    }
}
