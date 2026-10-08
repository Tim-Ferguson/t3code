//! Physical key enum mirrors the pinned Ghostty W3C-aligned ABI.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyInput {
    pub code: String,
    pub key: String,
    #[serde(default)]
    pub shift_key: bool,
    #[serde(default)]
    pub ctrl_key: bool,
    #[serde(default)]
    pub alt_key: bool,
    #[serde(default)]
    pub meta_key: bool,
    #[serde(default)]
    pub caps_lock: bool,
    #[serde(default)]
    pub num_lock: bool,
    #[serde(default)]
    pub repeat: bool,
    #[serde(default)]
    pub is_composing: bool,
    #[serde(default)]
    pub release: bool,
    #[serde(default)]
    pub layout_character: Option<String>,
}
const CODES: &[&str] = &[
    "Unidentified",
    "Backquote",
    "Backslash",
    "BracketLeft",
    "BracketRight",
    "Comma",
    "Digit0",
    "Digit1",
    "Digit2",
    "Digit3",
    "Digit4",
    "Digit5",
    "Digit6",
    "Digit7",
    "Digit8",
    "Digit9",
    "Equal",
    "IntlBackslash",
    "IntlRo",
    "IntlYen",
    "KeyA",
    "KeyB",
    "KeyC",
    "KeyD",
    "KeyE",
    "KeyF",
    "KeyG",
    "KeyH",
    "KeyI",
    "KeyJ",
    "KeyK",
    "KeyL",
    "KeyM",
    "KeyN",
    "KeyO",
    "KeyP",
    "KeyQ",
    "KeyR",
    "KeyS",
    "KeyT",
    "KeyU",
    "KeyV",
    "KeyW",
    "KeyX",
    "KeyY",
    "KeyZ",
    "Minus",
    "Period",
    "Quote",
    "Semicolon",
    "Slash",
    "AltLeft",
    "AltRight",
    "Backspace",
    "CapsLock",
    "ContextMenu",
    "ControlLeft",
    "ControlRight",
    "Enter",
    "MetaLeft",
    "MetaRight",
    "ShiftLeft",
    "ShiftRight",
    "Space",
    "Tab",
    "Convert",
    "KanaMode",
    "NonConvert",
    "Delete",
    "End",
    "Help",
    "Home",
    "Insert",
    "PageDown",
    "PageUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "ArrowUp",
    "NumLock",
    "Numpad0",
    "Numpad1",
    "Numpad2",
    "Numpad3",
    "Numpad4",
    "Numpad5",
    "Numpad6",
    "Numpad7",
    "Numpad8",
    "Numpad9",
    "NumpadAdd",
    "NumpadBackspace",
    "NumpadClear",
    "NumpadClearEntry",
    "NumpadComma",
    "NumpadDecimal",
    "NumpadDivide",
    "NumpadEnter",
    "NumpadEqual",
    "NumpadMemoryAdd",
    "NumpadMemoryClear",
    "NumpadMemoryRecall",
    "NumpadMemoryStore",
    "NumpadMemorySubtract",
    "NumpadMultiply",
    "NumpadParenLeft",
    "NumpadParenRight",
    "NumpadSubtract",
    "NumpadSeparator",
    "NumpadArrowUp",
    "NumpadArrowDown",
    "NumpadArrowRight",
    "NumpadArrowLeft",
    "NumpadBegin",
    "NumpadHome",
    "NumpadEnd",
    "NumpadInsert",
    "NumpadDelete",
    "NumpadPageUp",
    "NumpadPageDown",
    "Escape",
    "F1",
    "F2",
    "F3",
    "F4",
    "F5",
    "F6",
    "F7",
    "F8",
    "F9",
    "F10",
    "F11",
    "F12",
    "F13",
    "F14",
    "F15",
    "F16",
    "F17",
    "F18",
    "F19",
    "F20",
    "F21",
    "F22",
    "F23",
    "F24",
    "F25",
    "Fn",
    "FnLock",
    "PrintScreen",
    "ScrollLock",
    "Pause",
    "BrowserBack",
    "BrowserFavorites",
    "BrowserForward",
    "BrowserHome",
    "BrowserRefresh",
    "BrowserSearch",
    "BrowserStop",
    "Eject",
    "LaunchApp1",
    "LaunchApp2",
    "LaunchMail",
    "MediaPlayPause",
    "MediaSelect",
    "MediaStop",
    "MediaTrackNext",
    "MediaTrackPrevious",
    "Power",
    "Sleep",
    "AudioVolumeDown",
    "AudioVolumeMute",
    "AudioVolumeUp",
    "WakeUp",
    "Copy",
    "Cut",
    "Paste",
];
pub fn key_for_code(code: &str) -> u32 {
    CODES.iter().position(|v| *v == code).unwrap_or(0) as u32
}
impl KeyInput {
    pub fn mods(&self) -> u32 {
        u32::from(self.shift_key)
            | u32::from(self.ctrl_key) << 1
            | u32::from(self.alt_key) << 2
            | u32::from(self.meta_key) << 3
            | u32::from(self.caps_lock) << 4
            | u32::from(self.num_lock) << 5
    }
    pub fn consumed_mods(&self) -> u32 {
        u32::from(
            self.shift_key
                && !self.ctrl_key
                && !self.alt_key
                && !self.meta_key
                && self.key.chars().count() == 1,
        )
    }
    pub fn unshifted(&self) -> u32 {
        if self.key.chars().count() != 1 {
            return 0;
        }
        if let Some(layout) = self
            .layout_character
            .as_ref()
            .filter(|v| v.chars().count() == 1)
        {
            return layout.chars().next().unwrap() as u32;
        }
        let ch = self.key.chars().next().unwrap();
        if ch.is_ascii_uppercase() {
            return ch.to_ascii_lowercase() as u32;
        }
        if self.shift_key {
            if let Some(i) = "!@#$%^&*()~_+{}|:\"<>?".chars().position(|v| v == ch) {
                return "1234567890`-=[]\\;'.,/".chars().nth(i).unwrap() as u32;
            }
            let lower: String = ch.to_lowercase().collect();
            if lower != self.key && lower.chars().count() == 1 {
                return lower.chars().next().unwrap() as u32;
            }
            return 0;
        }
        ch as u32
    }
}
