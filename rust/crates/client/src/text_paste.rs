//! When pasted text becomes a file. Port of `textPaste.ts` (client-runtime): text of 32 KiB or more
//! is attached as `pasted-text.txt` so an agent can read it selectively instead of receiving it
//! inline, unless the reader pastes as plain text (Shift with the paste chord).

/// Counted in characters and in UTF-8 bytes, whichever passes first: character counts understate
/// the cost of text that is mostly multi-byte.
pub const THRESHOLD_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Attachment,
    Inline,
}

/// `pastedTextDisposition`.
pub fn disposition(text: &str, can_attach: bool, bypass_auto_attachment: bool, would_exceed_input_limit: bool) -> Disposition {
    if bypass_auto_attachment || !can_attach || text.is_empty() {
        return Disposition::Inline;
    }
    // JavaScript's `length` counts UTF-16 units.
    if would_exceed_input_limit || text.encode_utf16().count() >= THRESHOLD_BYTES || text.len() >= THRESHOLD_BYTES {
        Disposition::Attachment
    } else {
        Disposition::Inline
    }
}

/// `nextPastedTextFileName`: stable names when a draft holds several folded pastes.
pub fn next_file_name(existing: &[String]) -> String {
    let taken: Vec<String> = existing.iter().map(|name| name.to_lowercase()).collect();
    if !taken.iter().any(|name| name == "pasted-text.txt") {
        return "pasted-text.txt".to_owned();
    }
    (2..)
        .map(|sequence| format!("pasted-text-{sequence}.txt"))
        .find(|candidate| !taken.contains(candidate))
        .expect("an unused name")
}

/// `isPasteAsTextShortcut`: Shift with the platform's paste chord.
pub fn is_paste_as_text_shortcut(key: &str, shift: bool, meta: bool, ctrl: bool, alt: bool, mac: bool) -> bool {
    key.to_lowercase() == "v" && shift && !alt && if mac { meta && !ctrl } else { ctrl && !meta }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The rules agree with the unchanged original on text at, below and above the threshold.
    #[test]
    fn folding_names_and_the_shortcut_match_the_original() {
        let mut checked = 0;
        for line in include_str!("../tests/fixtures/text-paste.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let input = &row["input"];
            match row["fn"].as_str().unwrap() {
                "disposition" => {
                    let got = disposition(input["text"].as_str().unwrap(), input["canAttach"] == true, input["bypassAutoAttachment"] == true, input["wouldExceedInputLimit"] == true);
                    assert_eq!(if got == Disposition::Attachment { "attachment" } else { "inline" }, row["output"], "{} bytes", input["text"].as_str().unwrap().len());
                }
                "name" => {
                    let existing: Vec<String> = input.as_array().unwrap().iter().map(|name| name.as_str().unwrap().to_owned()).collect();
                    assert_eq!(next_file_name(&existing), row["output"].as_str().unwrap(), "{existing:?}");
                }
                "shortcut" => {
                    let flag = |name: &str| input[name] == true;
                    assert_eq!(is_paste_as_text_shortcut(input["key"].as_str().unwrap(), flag("shiftKey"), flag("metaKey"), flag("ctrlKey"), flag("altKey"), flag("mac")), row["output"] == true, "{input}");
                }
                other => panic!("{other}"),
            }
            checked += 1;
        }
        assert!(checked > 250, "{checked}");
    }
}
