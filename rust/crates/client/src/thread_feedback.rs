//! `/feedback` in a Codex thread's composer: the text sent to OpenAI with the thread's conversation, and what the person is told while
//! it goes and after (`packages/client-runtime/src/state/threadFeedback.ts`, `ChatView.tsx`'s `feedbackCommand`).
use serde_json::{Value, json};

pub const METHOD: &str = "provider.uploadFeedback";

/// `parseCodexFeedbackCommand`: the reason typed after `/feedback` (none when nothing follows); none when the text is not the command.
pub fn parse_command(text: &str) -> Option<Option<String>> {
    let trimmed = text.trim();
    let head = trimmed.get(..9)?;
    if !head.eq_ignore_ascii_case("/feedback") {
        return None;
    }
    let rest = &trimmed[9..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let reason = rest.trim();
    Some((!reason.is_empty()).then(|| reason.to_owned()))
}

/// The request for a thread's feedback.
pub fn input(thread_id: &str, reason: Option<&str>) -> Value {
    let mut input = json!({"threadId": thread_id});
    if let Some(reason) = reason {
        input["reason"] = json!(reason);
    }
    input
}

/// `codexFeedbackNotice`: the title and detail of each stage.
pub const UPLOADING: &str = "Sending feedback to OpenAI...";
pub const START_FIRST: (&str, &str) = ("Start a Codex thread first", "Send a message before you submit feedback.");
pub fn sent_notice(feedback_id: &str) -> (&'static str, String) {
    ("Feedback sent to OpenAI", format!("Thread ID: {feedback_id}"))
}
pub fn failed_notice(message: &str) -> (&'static str, String) {
    ("Could not send feedback to OpenAI", message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_the_word_alone_or_with_a_reason() {
        assert_eq!(parse_command("/feedback"), Some(None));
        assert_eq!(parse_command("  /FEEDBACK  "), Some(None));
        assert_eq!(parse_command("/feedback it forgot the tests"), Some(Some("it forgot the tests".into())));
        assert_eq!(parse_command("/feedback\nline one\nline two"), Some(Some("line one\nline two".into())));
        assert_eq!(parse_command("/feedbacks"), None);
        assert_eq!(parse_command("please /feedback"), None);
        assert_eq!(parse_command("/fee"), None);
        assert_eq!(parse_command("\u{e9}/feedback"), None);
    }

    #[test]
    fn the_request_carries_the_reason_only_when_there_is_one() {
        assert_eq!(input("t", None), json!({"threadId": "t"}));
        assert_eq!(input("t", Some("why")), json!({"threadId": "t", "reason": "why"}));
        assert_eq!(sent_notice("abc").1, "Thread ID: abc");
    }
}
