//! Terminal-style prompt recall for the composer. Port of `composerPromptHistory.ts`: ArrowUp on an
//! empty composer walks back through the thread's sent prompts, ArrowDown walks forward and empties the
//! composer past the newest. History is per thread and text only, derived from the thread's user
//! messages on every keypress, so there is nothing to persist.
use crate::rollback::recallable_prompt;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub prompt: String,
}

/// Active recall. `entry_id` is resolved against the current entries on every step, so a server
/// acknowledgement replacing an optimistic message cannot move the position; `recalled` is the text put in
/// the composer, and once the composer no longer matches it the person has edited or sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub entry_id: String,
    pub recalled: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub position: Option<Position>,
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Backward,
    Forward,
}

/// Oldest first. Consecutive identical prompts collapse into the newest one (`HISTCONTROL=ignoredups`);
/// an image-only send has no text and is skipped. `messages` is the projection's message rows.
pub fn entries(messages: &[Value]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for message in messages {
        if message["role"] != "user" {
            continue;
        }
        let prompt = recallable_prompt(message["text"].as_str().unwrap_or_default());
        if prompt.is_empty() {
            continue;
        }
        let id = message["id"].as_str().unwrap_or_default().to_owned();
        match entries.last_mut() {
            Some(last) if last.prompt == prompt => last.id = id,
            _ => entries.push(Entry { id, prompt }),
        }
    }
    entries
}

/// Prefer the id; a collapse can retire the recalled id while the same text lives on under a newer one.
fn active(entries: &[Entry], position: &Position) -> Option<usize> {
    entries.iter().position(|entry| entry.id == position.entry_id).or_else(|| entries.iter().rposition(|entry| entry.prompt == position.recalled))
}

/// `None` when the key should fall through to normal caret movement. Backward starts only from an empty
/// composer and stops at the oldest entry; forward past the newest empties the composer and ends browsing.
pub fn step(direction: Direction, entries: &[Entry], position: Option<&Position>, current: &str) -> Option<Step> {
    let at = position.filter(|position| position.recalled == current).and_then(|position| active(entries, position));
    let recalled = |entry: &Entry| Step { position: Some(Position { entry_id: entry.id.clone(), recalled: entry.prompt.clone() }), prompt: entry.prompt.clone() };
    match direction {
        Direction::Backward => {
            if at.is_none() && !current.is_empty() {
                return None;
            }
            let index = match at {
                Some(0) => return None,
                Some(at) => at - 1,
                None => entries.len().checked_sub(1)?,
            };
            Some(recalled(&entries[index]))
        }
        Direction::Forward => {
            let at = at?;
            Some(entries.get(at + 1).map(recalled).unwrap_or(Step { position: None, prompt: String::new() }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(id: &str, text: &str) -> Value {
        json!({"id":id,"role":"user","text":text})
    }

    #[test]
    fn history_is_the_text_the_person_sent_with_consecutive_repeats_collapsed() {
        let messages = [user("m1", "first"), json!({"id":"a1","role":"assistant","text":"reply"}), user("m2", "second"), user("m3", "second"), user("m4", "   "), user("m5", "PLEASE IMPLEMENT THIS PLAN:\n# x"), user("m6", "Ultrathink:\nthird")];
        let found = entries(&messages);
        assert_eq!(found.iter().map(|entry| (entry.id.as_str(), entry.prompt.as_str())).collect::<Vec<_>>(), [("m1", "first"), ("m3", "second"), ("m6", "third")]);
    }

    #[test]
    fn arrow_up_walks_back_from_an_empty_composer_and_arrow_down_walks_forward_to_an_empty_one() {
        let found = entries(&[user("m1", "first"), user("m2", "second")]);
        // Only an empty composer starts to recall; a draft is left to the caret.
        assert_eq!(step(Direction::Backward, &found, None, "typed"), None);
        let newest = step(Direction::Backward, &found, None, "").unwrap();
        assert_eq!(newest.prompt, "second");
        let oldest = step(Direction::Backward, &found, newest.position.as_ref(), "second").unwrap();
        assert_eq!(oldest.prompt, "first");
        assert_eq!(step(Direction::Backward, &found, oldest.position.as_ref(), "first"), None, "stops at the oldest");
        let forward = step(Direction::Forward, &found, oldest.position.as_ref(), "first").unwrap();
        assert_eq!(forward.prompt, "second");
        let past = step(Direction::Forward, &found, forward.position.as_ref(), "second").unwrap();
        assert_eq!((past.prompt.as_str(), past.position), ("", None), "past the newest the composer empties and browsing ends");
        // An edited recall ends browsing: the next backward step falls through, forward has nowhere to go.
        assert_eq!(step(Direction::Backward, &found, newest.position.as_ref(), "second, edited"), None);
        assert_eq!(step(Direction::Forward, &found, newest.position.as_ref(), "second, edited"), None);
        // Nothing sent, nothing to recall.
        assert_eq!(step(Direction::Backward, &[], None, ""), None);
    }
}
