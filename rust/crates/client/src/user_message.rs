//! Who a user message came from, when it was not the person at the keyboard: a scheduled task, or
//! another agent (a delegated task's wake among them). Port of `resolveUserMessagePresentation`
//! (`packages/client-runtime/src/userMessage.ts`) and the attribution line of `MessagesTimeline.tsx`.
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attribution {
    /// "Sent by automation": a scheduled task wrote it, which the line names when it is known.
    Automation { task_id: Option<String> },
    /// "Sent by another agent": the thread that sent it, when it was one.
    Agent { sender_thread: Option<String> },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presentation {
    pub text: String,
    pub attribution: Option<Attribution>,
}

/// The message as the reader sees it. Older scheduled messages kept their attribution in the prompt
/// itself, as a first line that is not part of what was meant.
pub fn present(item: &Value) -> Presentation {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    static MESSAGE_ID: OnceLock<Regex> = OnceLock::new();
    let text = item["text"].as_str().unwrap_or_default();
    let agent = item["createdBy"] == "agent";
    let task = item["scheduledTaskId"].as_str().filter(|id| !id.is_empty());
    let (text, automation) = if task.is_some() {
        (text.to_owned(), Some(task.map(str::to_owned)))
    } else {
        let prefix = PREFIX.get_or_init(|| Regex::new(r"^\[Triggered by schedule task: [^\r\n]+\]\r?\n\r?\n").expect("legacy prefix")).find(text);
        let id = item["messageId"].as_str().or_else(|| item["id"].as_str()).unwrap_or_default();
        let legacy = MESSAGE_ID.get_or_init(|| Regex::new(r"^scheduled-task-message:(.+):\d+:(?:scheduled|manual)$").expect("legacy id")).captures(id).map(|captures| captures[1].to_owned());
        match prefix {
            Some(prefix) if legacy.is_some() || agent => (text[prefix.end()..].to_owned(), Some(legacy)),
            _ => (text.to_owned(), None),
        }
    };
    let attribution = match automation {
        Some(task_id) => Some(Attribution::Automation { task_id }),
        None if agent => Some(Attribution::Agent { sender_thread: item["senderThreadId"].as_str().filter(|id| !id.is_empty()).map(str::to_owned) }),
        None => None,
    };
    Presentation { text, attribution }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_message_is_attributed_to_what_wrote_it() {
        // Typed by the person: nothing to say.
        assert_eq!(present(&json!({"text":"hi","createdBy":"user"})), Presentation { text: "hi".into(), attribution: None });
        // A delegated task's wake, as the recorded session's parent receives it: written by the server for an agent.
        let wake = json!({"text":"Delegated task t1 reached a terminal state. Use task_status with taskId t1 to read the result.","createdBy":"agent","creationSource":"server"});
        assert_eq!(present(&wake).attribution, Some(Attribution::Agent { sender_thread: None }));
        assert_eq!(present(&json!({"text":"x","createdBy":"agent","senderThreadId":"other"})).attribution, Some(Attribution::Agent { sender_thread: Some("other".into()) }));
        // A scheduled task names itself.
        assert_eq!(present(&json!({"text":"run","createdBy":"user","scheduledTaskId":"task-1"})).attribution, Some(Attribution::Automation { task_id: Some("task-1".into()) }));
    }

    #[test]
    fn an_older_scheduled_message_keeps_its_attribution_in_the_first_line() {
        let prompt = "[Triggered by schedule task: Nightly]\n\nrun the checks";
        let by_id = present(&json!({"text":prompt,"createdBy":"user","messageId":"scheduled-task-message:task-7:1700:scheduled"}));
        assert_eq!((by_id.text.as_str(), by_id.attribution), ("run the checks", Some(Attribution::Automation { task_id: Some("task-7".into()) })));
        // Written by an agent, the prefix is the same attribution without a task to link.
        let by_agent = present(&json!({"text":prompt,"createdBy":"agent","messageId":"m"}));
        assert_eq!((by_agent.text.as_str(), by_agent.attribution), ("run the checks", Some(Attribution::Automation { task_id: None })));
        // Typed by the person, a first line that looks like it is just their words.
        assert_eq!(present(&json!({"text":prompt,"createdBy":"user","messageId":"m"})), Presentation { text: prompt.into(), attribution: None });
    }
}
