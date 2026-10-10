//! The temporary title a thread gets from its first message, which the server replaces with a
//! generated one: `titleSeed` of `ChatView.tsx` and `truncate` of `@t3tools/shared/String`.

/// `truncate(text, 50)`: trimmed, and cut at 50 UTF-16 units with an ellipsis. Lengths are
/// counted in UTF-16 units because the original's are.
pub fn truncate(text: &str, max_length: usize) -> String {
    let trimmed = text.trim();
    let units: Vec<u16> = trimmed.encode_utf16().collect();
    if units.len() <= max_length {
        return trimmed.to_owned();
    }
    format!("{}...", String::from_utf16_lossy(&units[..max_length]))
}

/// The seed for a message: its text, else the first image, else the first file, else "New thread".
pub fn title_seed(text: &str, first_image: Option<&str>, first_file: Option<&str>) -> String {
    let text = text.trim();
    let seed = if !text.is_empty() {
        text.to_owned()
    } else if let Some(name) = first_image {
        format!("Image: {name}")
    } else if let Some(name) = first_file {
        format!("File: {name}")
    } else {
        "New thread".to_owned()
    };
    truncate(&seed, 50)
}

/// Whether a message to an existing thread puts its seed on the wire (`shouldSendTitleSeed` of
/// `startThreadTurn`): only while the thread's own projection holds no messages, so the server titles it
/// from its first message, or whenever the server resolves command context itself. A fork has no messages
/// of its own until it is first used, so its first message renames it as a new thread's does. With no
/// projection loaded for the thread (`None`) nothing is sent, as the original's `undefined === 0`.
pub fn seed_to_send(seed: String, projection_messages: Option<usize>, server_resolves_context: bool) -> Option<String> {
    (server_resolves_context || projection_messages == Some(0)).then_some(seed)
}

/// Names of the first image and the first file among wire attachments.
pub fn first_attachment_names(attachments: &serde_json::Value) -> (Option<String>, Option<String>) {
    let list = attachments.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let name_of = |kind: &str| list.iter().find(|attachment| attachment["type"] == kind).and_then(|attachment| attachment["name"].as_str()).map(str::to_owned);
    (name_of("image"), name_of("file"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_seed_is_the_message_cut_at_fifty_units() {
        assert_eq!(title_seed("  Fix the login bug  ", None, None), "Fix the login bug");
        let long = "x".repeat(60);
        assert_eq!(title_seed(&long, None, None), format!("{}...", "x".repeat(50)));
        assert_eq!(title_seed(&"x".repeat(50), None, None), "x".repeat(50));
        // An emoji is two units: the cut counts them as the original does.
        let emoji = format!("{}😀", "y".repeat(49));
        assert_eq!(truncate(&emoji, 50), format!("{}\u{fffd}...", "y".repeat(49)));
    }

    #[test]
    fn a_seed_goes_on_the_wire_only_for_a_thread_with_no_messages_or_a_server_that_resolves_context() {
        let seed = || title_seed("go on", None, None);
        assert_eq!(seed_to_send(seed(), Some(0), false).as_deref(), Some("go on"));
        assert_eq!(seed_to_send(seed(), Some(3), false), None);
        assert_eq!(seed_to_send(seed(), None, false), None);
        assert_eq!(seed_to_send(seed(), Some(3), true).as_deref(), Some("go on"));
    }

    #[test]
    fn without_text_the_first_attachment_names_the_thread() {
        assert_eq!(title_seed("", Some("shot.png"), Some("a.txt")), "Image: shot.png");
        assert_eq!(title_seed(" ", None, Some("a.txt")), "File: a.txt");
        assert_eq!(title_seed("", None, None), "New thread");
        let (image, file) = first_attachment_names(&json!([{"type":"file","name":"a.txt"},{"type":"image","name":"b.png"},{"type":"image","name":"c.png"}]));
        assert_eq!((image.as_deref(), file.as_deref()), (Some("b.png"), Some("a.txt")));
    }
}
