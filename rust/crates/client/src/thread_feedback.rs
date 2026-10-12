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

/// One `/feedback` command and how it stands (`CodexFeedbackSubmission`). They live as long as the open page: the exchange is
/// drawn as a banner above the composer and as two rows of the conversation, and is not kept on the server.
#[derive(Debug, Clone, PartialEq)]
pub struct Submission {
    pub id: String,
    /// The text typed, as sent.
    pub command: String,
    pub created_at: String,
    pub status: Status,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Uploading,
    Sent { feedback_id: String },
    Failed { error: String },
}

/// `codexFeedbackNotice` for a submission: the banner's title and description, none for a submission that drew none (the
/// original's "interrupted" has no counterpart here: a request is never interrupted by the person).
pub fn notice(submission: &Submission) -> (String, Option<String>) {
    match &submission.status {
        Status::Uploading => (UPLOADING.to_owned(), None),
        Status::Sent { feedback_id } => {
            let (title, detail) = sent_notice(feedback_id);
            (title.to_owned(), Some(detail))
        }
        Status::Failed { error } => {
            let (title, detail) = failed_notice(error);
            (title.to_owned(), Some(detail))
        }
    }
}

/// `codexFeedbackMessage`: the text of the conversation row the command makes (what was typed) or its answer.
pub fn message_text(submission: &Submission, assistant: bool) -> String {
    if !assistant {
        return submission.command.clone();
    }
    match &submission.status {
        Status::Sent { feedback_id } => format!("Feedback sent to OpenAI.\n\nThread ID: `{feedback_id}`"),
        Status::Failed { error } => format!("Could not send feedback to OpenAI.\n\n{error}"),
        Status::Uploading => UPLOADING.to_owned(),
    }
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
    fn a_submission_is_a_notice_and_two_rows_by_how_it_stands() {
        let mut submission = Submission { id: "m".into(), command: "/feedback slow".into(), created_at: "2026-01-01T00:00:00Z".into(), status: Status::Uploading };
        assert_eq!(notice(&submission), ("Sending feedback to OpenAI...".to_owned(), None));
        assert_eq!(message_text(&submission, false), "/feedback slow");
        assert_eq!(message_text(&submission, true), "Sending feedback to OpenAI...");
        submission.status = Status::Sent { feedback_id: "abc".into() };
        assert_eq!(notice(&submission), ("Feedback sent to OpenAI".to_owned(), Some("Thread ID: abc".to_owned())));
        assert_eq!(message_text(&submission, true), "Feedback sent to OpenAI.\n\nThread ID: `abc`");
        submission.status = Status::Failed { error: "offline".into() };
        assert_eq!(notice(&submission).0, "Could not send feedback to OpenAI");
        assert_eq!(message_text(&submission, true), "Could not send feedback to OpenAI.\n\noffline");
    }

    #[test]
    fn the_request_carries_the_reason_only_when_there_is_one() {
        assert_eq!(input("t", None), json!({"threadId": "t"}));
        assert_eq!(input("t", Some("why")), json!({"threadId": "t", "reason": "why"}));
        assert_eq!(sent_notice("abc").1, "Thread ID: abc");
    }
}
