//! Whether a key press in a message box sends the message: the rule of the original's
//! `composerSubmissionIntentForKey` for a key no keybinding claimed. The page's shortcut listener
//! has already taken the chords the rules bind (and `composer.sendAlternate`'s Ctrl/Cmd-Enter
//! with them), so what reaches a box is the plain key.
use crate::keybindings::KeyEvent;
use t3_contracts::SendShortcut;

/// The width below which the original treats the window as a phone (`max-sm`): there Enter is a
/// new line and the Send button sends.
pub const MOBILE_MAX_WIDTH: f64 = 640.0;

/// Whether Enter, as pressed, sends `prompt` rather than inserting a line.
///
/// Not while an IME composition owns the key, not on auto-repeat, not with Shift or Alt (a new
/// line), and with the "mod-enter" settings only with Control or Command (for
/// "mod-enter-multiline", only once the message has a second line).
pub fn enter_sends(event: &KeyEvent, shortcut: SendShortcut, prompt: &str, mobile: bool) -> bool {
    if mobile || event.is_composing() || event.repeat || event.key != "Enter" || event.shift || event.alt {
        return false;
    }
    let multiline = prompt.contains(['\n', '\r']);
    let needs_modifier = shortcut == SendShortcut::ModEnter || (shortcut == SendShortcut::ModEnterMultiline && multiline);
    !needs_modifier || event.meta || event.ctrl
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enter() -> KeyEvent {
        KeyEvent { key: "Enter".into(), code: "Enter".into(), ..Default::default() }
    }

    #[test]
    fn plain_enter_sends_and_a_modified_or_busy_enter_does_not() {
        assert!(enter_sends(&enter(), SendShortcut::Enter, "hello", false));
        assert!(!enter_sends(&KeyEvent { shift: true, ..enter() }, SendShortcut::Enter, "hello", false), "Shift-Enter is a new line");
        assert!(!enter_sends(&KeyEvent { alt: true, ..enter() }, SendShortcut::Enter, "hello", false));
        assert!(!enter_sends(&KeyEvent { repeat: true, ..enter() }, SendShortcut::Enter, "hello", false), "a held key sends once");
        assert!(!enter_sends(&KeyEvent { composing: true, ..enter() }, SendShortcut::Enter, "hello", false), "Enter confirms an IME candidate");
        assert!(!enter_sends(&KeyEvent { key_code: 229, ..enter() }, SendShortcut::Enter, "hello", false), "the legacy IME key code too");
        assert!(!enter_sends(&enter(), SendShortcut::Enter, "hello", true), "on a phone Enter is a new line");
        assert!(!enter_sends(&KeyEvent { key: "a".into(), ..Default::default() }, SendShortcut::Enter, "hello", false));
    }

    #[test]
    fn the_send_shortcut_setting_asks_for_a_modifier() {
        let command = KeyEvent { ctrl: true, ..enter() };
        assert!(enter_sends(&command, SendShortcut::Enter, "hello", false), "Ctrl-Enter still sends when plain Enter does");
        assert!(!enter_sends(&enter(), SendShortcut::ModEnter, "hello", false));
        assert!(enter_sends(&command, SendShortcut::ModEnter, "hello", false));
        assert!(enter_sends(&KeyEvent { meta: true, ..enter() }, SendShortcut::ModEnter, "hello", false));
        // One line sends on Enter; once the message has two, Enter adds a third and Ctrl/Cmd sends.
        assert!(enter_sends(&enter(), SendShortcut::ModEnterMultiline, "one line", false));
        assert!(!enter_sends(&enter(), SendShortcut::ModEnterMultiline, "one\ntwo", false));
        assert!(enter_sends(&command, SendShortcut::ModEnterMultiline, "one\ntwo", false));
    }
}
