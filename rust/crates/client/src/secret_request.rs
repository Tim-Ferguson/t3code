//! What a secret request card says and sends. Port of `secretRequest.ts` (client-runtime) and the
//! failure copy of `secretRequest.ts` (contracts). The typed value is only ever an argument here:
//! nothing in this module keeps it, logs it, or puts it in a message.
use serde_json::{Value, json};

/// Shown under the field: the one promise the card makes about the value.
pub const PRIVACY_NOTE: &str = "Stored securely, never shown to the agent";
pub const DEFAULT_PLACEHOLDER: &str = "Paste the secret";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    /// The form.
    Pending,
    /// Asked in another thread (a fork inherited it), so it can only be answered there.
    PendingElsewhere,
    Answered(Outcome),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Saved,
    Declined,
    Ended,
}
impl Display {
    /// The one-line outcome of a card that is not a form.
    pub fn label(self) -> &'static str {
        match self {
            Display::Pending => "",
            Display::PendingElsewhere => "Waiting for an answer in the original thread",
            Display::Answered(Outcome::Saved) => "Saved securely and kept private",
            Display::Answered(Outcome::Declined) => "Declined",
            Display::Answered(Outcome::Ended) => "Request ended",
        }
    }
}
/// `secretRequestDisplay`. `visibility` is the projected row's: `local`, `inherited` or `synthetic`.
pub fn display(item: &Value, visibility: &str) -> Display {
    match item["secretStatus"].as_str() {
        Some("saved") => Display::Answered(Outcome::Saved),
        Some("declined") => Display::Answered(Outcome::Declined),
        Some("cancelled") => Display::Answered(Outcome::Ended),
        // Anything not yet answered, including a status this client does not know, is still open.
        _ if visibility == "local" => Display::Pending,
        _ => Display::PendingElsewhere,
    }
}

/// `secretRequestAnswerInput`: the payload of `secrets.answerRequest`. A save of a blank value is
/// none, since the server rejects it.
pub fn answer_input(item: &Value, secret: Option<&str>) -> Option<Value> {
    let (thread, id) = (item["threadId"].as_str()?, item["id"].as_str()?);
    match secret {
        None => Some(json!({"threadId":thread,"turnItemId":id,"answer":{"type":"decline"}})),
        Some(secret) => {
            let secret = secret.trim();
            (!secret.is_empty()).then(|| json!({"threadId":thread,"turnItemId":id,"answer":{"type":"save","secret":secret}}))
        }
    }
}

/// The server's own words for a failed answer (`SECRET_REQUEST_FAILURE_MESSAGES`), which never
/// carry the request. Anything else gets the generic copy, so the typed value cannot surface.
const KNOWN_FAILURES: [&str; 8] = [
    "Could not load the secret request.",
    "This secret request no longer exists.",
    "This secret request was already answered.",
    "The agent that asked has stopped, so this secret can't be used.",
    "Could not store the secret.",
    "Saved the secret, but could not update the request.",
    "Could not read the secret.",
    "Could not use that secretRef. Try again.",
];
pub fn failure_message(message: &str) -> String {
    let message = message.trim();
    if KNOWN_FAILURES.contains(&message) { message.to_owned() } else { "Could not answer the request. Try again.".to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_is_a_form_only_where_it_was_asked_and_until_it_is_answered() {
        let item = |status: &str| json!({"id":"i","threadId":"t","secretStatus":status});
        assert_eq!(display(&item("pending"), "local"), Display::Pending);
        assert_eq!(display(&item("pending"), "inherited"), Display::PendingElsewhere);
        assert_eq!(display(&item("saved"), "inherited").label(), "Saved securely and kept private");
        assert_eq!(display(&item("declined"), "local").label(), "Declined");
        assert_eq!(display(&item("cancelled"), "local").label(), "Request ended");
    }

    #[test]
    fn an_answer_carries_the_trimmed_value_once_and_a_blank_one_is_not_sent() {
        let item = json!({"id":"item-1","threadId":"thread-1"});
        assert_eq!(answer_input(&item, Some("  s3cret \n")), Some(json!({"threadId":"thread-1","turnItemId":"item-1","answer":{"type":"save","secret":"s3cret"}})));
        assert_eq!(answer_input(&item, Some("   ")), None);
        assert_eq!(answer_input(&item, None), Some(json!({"threadId":"thread-1","turnItemId":"item-1","answer":{"type":"decline"}})));
    }

    #[test]
    fn only_the_servers_own_failure_words_reach_the_card() {
        assert_eq!(failure_message("This secret request was already answered."), "This secret request was already answered.");
        assert_eq!(failure_message("decode failed near hunter2"), "Could not answer the request. Try again.");
    }
}
