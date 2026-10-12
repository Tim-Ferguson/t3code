//! What an agent does to a page: pointer, keyboard and form actions (the operations of apps/server/src/preview/ServerBrowserPage.ts).
//!
//! Playwright's actions wait for their target to be present, visible, enabled, still and unobstructed before they act, then act through the
//! protocol's input events. The page's helper script answers those checks (`probe`); the waiting and the events are here.
use crate::server_browser_page::{DEFAULT_TIMEOUT_MS, OpError, OpResult, Page, WAIT_POLL};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use t3_contracts::{PreviewAutomationClickInput, PreviewAutomationDragInput, PreviewAutomationHoverInput, PreviewAutomationPressInput, PreviewAutomationScrollInput, PreviewAutomationSelectInput, PreviewAutomationTypeInput, PreviewMouseButton};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerPhase {
    Move,
    Click,
}

/// Shows the agent's pointer to viewers before an action lands at that point; it returns once the pointer has had time to get there.
pub type PointerReporter = Arc<dyn Fn(Point, PointerPhase) -> BoxFuture<'static, ()> + Send + Sync>;

pub fn no_pointer() -> PointerReporter {
    Arc::new(|_, _| Box::pin(async {}))
}

fn timeout_of(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS))
}

/// The state an action waits on; `scroll` brings the target into view first.
struct Probe {
    scroll: bool,
    enabled: bool,
    hit: bool,
}

impl Page {
    /// Waits until `selector` names one element an action can land on, and returns the point it will hit.
    async fn actionable_point(&self, action: &str, selector: &str, probe: Probe, timeout: Duration) -> OpResult<Point> {
        let deadline = Instant::now() + timeout;
        loop {
            let probed = self.runtime("probe", &[json!(selector), json!({"scroll": probe.scroll, "enabled": probe.enabled, "hit": probe.hit})], true).await?;
            let last = match probed["status"].as_str() {
                Some("ok") => return Ok(Point { x: probed["x"].as_f64().unwrap_or_default(), y: probed["y"].as_f64().unwrap_or_default() }),
                Some("retry") => probed["reason"].as_str().unwrap_or("element is not ready").to_owned(),
                _ => "waiting for element".to_owned(),
            };
            if Instant::now() >= deadline {
                return Err(OpError::timeout(format!("locator.{action}: Timeout {}ms exceeded.\n  - {last}", timeout.as_millis())));
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    async fn mouse(&self, kind: &str, point: Point, button: &str, buttons: u32, click_count: u32) -> OpResult<()> {
        self.send("Input.dispatchMouseEvent", json!({"type": kind, "x": point.x, "y": point.y, "button": button, "buttons": buttons, "clickCount": click_count, "modifiers": 0})).await?;
        Ok(())
    }

    /// A click: the pointer moves there, and each press and release of a multiple click is its own pair of events.
    async fn click_at(&self, point: Point, button: PreviewMouseButton, click_count: u8) -> OpResult<()> {
        let (name, mask) = match button {
            PreviewMouseButton::Left => ("left", 1),
            PreviewMouseButton::Right => ("right", 2),
            PreviewMouseButton::Middle => ("middle", 4),
        };
        self.mouse("mouseMoved", point, "none", 0, 0).await?;
        for count in 1..=u32::from(click_count.max(1)) {
            self.mouse("mousePressed", point, name, mask, count).await?;
            self.mouse("mouseReleased", point, name, 0, count).await?;
        }
        Ok(())
    }

    /// A click whose handler opens a dialog does not finish until the dialog is resolved, so it returns as soon as the dialog opens; status
    /// then reports it.
    pub async fn click(&self, input: &PreviewAutomationClickInput, pointer: &PointerReporter) -> OpResult<Point> {
        let timeout = timeout_of(input.timeout_ms);
        let selector = input.target().map(|selector| self.target_selector(selector)).transpose()?;
        let point = match &selector {
            // Waits for the whole of Playwright's checks, so the click lands on the control and not on what covers it.
            Some(selector) => self.actionable_point("click", selector, Probe { scroll: true, enabled: true, hit: true }, timeout).await?,
            None => Point { x: input.x.unwrap_or_default(), y: input.y.unwrap_or_default() },
        };
        pointer(point, PointerPhase::Click).await;
        let mut dialogs = self.dialog_count();
        let before = *dialogs.borrow_and_update();
        let page = self.clone();
        let (button, count) = (input.button.unwrap_or(PreviewMouseButton::Left), input.click_count.unwrap_or(1));
        let clicked = tokio::spawn(async move { page.click_at(point, button, count).await });
        tokio::select! {
            result = clicked => match result {
                Ok(result) => result?,
                Err(error) => return Err(OpError::execution(error.to_string())),
            },
            // The click goes on in the background; its outcome is the dialog.
            _ = dialogs.wait_for(|count| *count != before) => {}
        }
        Ok(point)
    }

    /// Hovering by locator moves the real mouse, so CSS `:hover` and pointer events both apply.
    pub async fn hover(&self, input: &PreviewAutomationHoverInput, pointer: &PointerReporter) -> OpResult<()> {
        let timeout = timeout_of(input.timeout_ms);
        let selector = input.target().map(|selector| self.target_selector(selector)).transpose()?;
        let point = match &selector {
            Some(selector) => self.actionable_point("hover", selector, Probe { scroll: true, enabled: false, hit: true }, timeout).await?,
            None => Point { x: input.x.unwrap_or_default(), y: input.y.unwrap_or_default() },
        };
        pointer(point, PointerPhase::Move).await;
        self.mouse("mouseMoved", point, "none", 0, 0).await
    }

    /// Drags one element onto another. A page that starts a native drag gets it dropped with the protocol's drag events.
    pub async fn drag(&self, input: &PreviewAutomationDragInput, pointer: &PointerReporter) -> OpResult<()> {
        let timeout = timeout_of(input.timeout_ms);
        let source = self.target_selector(input.source.as_deref().unwrap_or_default())?;
        let target = self.target_selector(input.target.as_deref().unwrap_or_default())?;
        let from = self.actionable_point("dragTo", &source, Probe { scroll: true, enabled: false, hit: true }, timeout).await?;
        pointer(from, PointerPhase::Move).await;
        let to = self.actionable_point("dragTo", &target, Probe { scroll: true, enabled: false, hit: false }, timeout).await?;
        // The cursor travels with the drag; the page sees one continuous gesture.
        pointer(to, PointerPhase::Move).await;
        self.clear_drag();
        self.mouse("mouseMoved", from, "none", 0, 0).await?;
        self.mouse("mousePressed", from, "left", 1, 1).await?;
        // Intermediate moves start a drag that needs more than one event to begin.
        for step in 1..=4 {
            let fraction = f64::from(step) / 4.0;
            self.mouse("mouseMoved", Point { x: from.x + (to.x - from.x) * fraction, y: from.y + (to.y - from.y) * fraction }, "none", 1, 0).await?;
        }
        match self.take_drag() {
            Some(data) => {
                for kind in ["dragEnter", "dragOver", "drop"] {
                    self.send("Input.dispatchDragEvent", json!({"type": kind, "x": to.x, "y": to.y, "data": data, "modifiers": 0})).await?;
                }
                self.mouse("mouseReleased", to, "left", 0, 1).await
            }
            None => self.mouse("mouseReleased", to, "left", 0, 1).await,
        }
    }

    /// Types into the target, or into the focused element when none is named.
    pub async fn type_text(&self, input: &PreviewAutomationTypeInput) -> OpResult<()> {
        let timeout = timeout_of(input.timeout_ms);
        let text = input.text.as_deref().unwrap_or_default();
        let selector = input.target().map(|selector| self.target_selector(selector)).transpose()?;
        let clear = input.clear.unwrap_or(false);
        if let (Some(selector), true) = (&selector, clear) {
            let deadline = Instant::now() + timeout;
            loop {
                match self.runtime("fill", &[json!(selector), json!(text)], false).await?.as_bool() {
                    Some(true) => return Ok(()),
                    Some(false) => return Err(OpError::not_editable("The target is not an enabled text field, so no text was typed.")),
                    None => {}
                }
                if Instant::now() >= deadline {
                    return Err(OpError::timeout(format!("locator.fill: Timeout {}ms exceeded.", timeout.as_millis())));
                }
                tokio::time::sleep(WAIT_POLL).await;
            }
        }
        // Like the desktop host: an enabled text control or contenteditable that actually takes focus. Anything else would swallow the text or
        // send it to whichever field had focus before.
        let deadline = Instant::now() + timeout;
        let focused = loop {
            let focused = self.runtime("focusEditable", &[selector.as_ref().map_or(Value::Null, |selector| json!(selector))], false).await?;
            if !focused.is_null() || selector.is_none() {
                break focused == json!(true);
            }
            if Instant::now() >= deadline {
                return Err(OpError::timeout(format!("locator.evaluate: Timeout {}ms exceeded.", timeout.as_millis())));
            }
            tokio::time::sleep(WAIT_POLL).await;
        };
        if !focused {
            return Err(OpError::not_editable("The target is not an enabled text field, so no text was typed."));
        }
        if clear {
            self.press_keys("ControlOrMeta+A").await?;
            self.press_keys("Delete").await?;
        }
        self.send("Input.insertText", json!({"text": text})).await?;
        Ok(())
    }

    /// Chooses options in one native `<select>`; each entry matches an option by value first, then by its visible label.
    pub async fn select(&self, input: &PreviewAutomationSelectInput) -> OpResult<Vec<String>> {
        let timeout = timeout_of(input.timeout_ms);
        let selector = self.target_selector(input.target().unwrap_or_default())?;
        let deadline = Instant::now() + timeout;
        loop {
            let info = self.runtime("selectInfo", &[json!(selector)], false).await?;
            if info.is_null() {
                return Err(OpError::not_editable("This element is not a <select>. Click a custom dropdown, then click its option."));
            }
            if let Some(options) = info.as_array() {
                let option = |name: &str, entry: &Value| entry[name].as_str().unwrap_or_default().to_owned();
                let wanted = input.values.clone().unwrap_or_default();
                let values: Vec<String> = wanted
                    .iter()
                    .map(|entry| {
                        options
                            .iter()
                            .find(|candidate| option("value", candidate) == *entry)
                            .or_else(|| options.iter().find(|candidate| option("label", candidate) == entry.trim()))
                            .map_or_else(|| entry.clone(), |found| option("value", found))
                    })
                    .collect();
                // An option that is not there yet may be added by the page; Playwright waits for it.
                if values.iter().all(|value| options.iter().any(|candidate| option("value", candidate) == *value)) {
                    let selected = self.runtime("selectOptions", &[json!(selector), json!(values)], false).await?;
                    return Ok(selected.as_array().map(|selected| selected.iter().filter_map(|value| value.as_str().map(str::to_owned)).collect()).unwrap_or_default());
                }
            }
            if Instant::now() >= deadline {
                return Err(OpError::timeout(format!("locator.selectOption: Timeout {}ms exceeded.", timeout.as_millis())));
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    pub async fn press(&self, input: &PreviewAutomationPressInput) -> OpResult<()> {
        let mut parts: Vec<&str> = input.modifiers.iter().flatten().map(|modifier| modifier.as_str()).collect();
        parts.push(input.key.as_deref().unwrap_or_default());
        self.press_keys(&parts.join("+")).await
    }

    pub async fn scroll(&self, input: &PreviewAutomationScrollInput) -> OpResult<()> {
        let (x, y) = (input.delta_x.unwrap_or(0.0), input.delta_y.unwrap_or(0.0));
        let selector = input.target().map(|selector| self.target_selector(selector)).transpose()?;
        let deadline = Instant::now() + timeout_of(None);
        loop {
            let scrolled = self.runtime("scrollBy", &[selector.as_ref().map_or(Value::Null, |selector| json!(selector)), json!(x), json!(y)], false).await?;
            if !scrolled.is_null() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(OpError::timeout(format!("locator.evaluate: Timeout {DEFAULT_TIMEOUT_MS}ms exceeded.")));
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// Sets files on one file input; false when no locator or selector names one.
    pub async fn set_input_files(&self, selector: Option<&str>, paths: &[String], timeout_ms: Option<u64>) -> OpResult<bool> {
        let Some(selector) = selector else { return Ok(false) };
        let selector = self.target_selector(selector)?;
        let timeout = timeout_of(timeout_ms);
        let deadline = Instant::now() + timeout;
        loop {
            match self.runtime("fileInputState", &[json!(selector)], false).await?.as_str() {
                Some("ok") => break,
                Some("notInput") => return Err(OpError::not_editable("Node is not an <input> element")),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(OpError::timeout(format!("locator.setInputFiles: Timeout {}ms exceeded.", timeout.as_millis())));
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
        let object = self.runtime_object("fileInput", &[json!(selector)]).await?.ok_or_else(|| OpError::execution("The file input went away."))?;
        // TODO(original: any readable file can be given to the page). The paths are not checked against the workspace or any other
        // allowlist; the browser reads each one as the server's own user, and the page can send what it read anywhere it can reach.
        self.send("DOM.setFileInputFiles", json!({"files": paths, "objectId": object})).await?;
        Ok(true)
    }

    /// Gives an open file picker its files.
    pub async fn set_chooser_files(&self, backend_node_id: i64, paths: &[String]) -> OpResult<()> {
        self.send("DOM.setFileInputFiles", json!({"files": paths, "backendNodeId": backend_node_id})).await?;
        Ok(())
    }

    /// The `accept` attribute of the input behind a file picker.
    pub async fn chooser_accept(&self, backend_node_id: i64) -> String {
        let accept = async {
            let resolved = self.send("DOM.resolveNode", json!({"backendNodeId": backend_node_id})).await.ok()?;
            let object = resolved["object"]["objectId"].as_str()?.to_owned();
            let called = self
                .send("Runtime.callFunctionOn", json!({"objectId": object, "functionDeclaration": "function() { return this.getAttribute('accept'); }", "returnByValue": true}))
                .await
                .ok()?;
            called["result"]["value"].as_str().map(str::to_owned)
        };
        accept.await.unwrap_or_default()
    }

    // ---- Keyboard

    /// Presses a key, optionally with modifiers joined by `+`: each modifier goes down, the key goes down and up, the modifiers come up.
    pub async fn press_keys(&self, chord: &str) -> OpResult<()> {
        let mac = cfg!(target_os = "macos");
        // A key that is `+` itself ends the chord with a second `+`.
        let (names, key): (Vec<&str>, &str) = if chord == "+" {
            (Vec::new(), "+")
        } else if let Some(modifiers) = chord.strip_suffix("++") {
            (modifiers.split('+').collect(), "+")
        } else {
            let mut names: Vec<&str> = chord.split('+').collect();
            let key = names.pop().unwrap_or_default();
            (names, key)
        };
        let mut modifiers = 0u32;
        let mut held: Vec<KeyDefinition> = Vec::new();
        for name in names {
            let name = if name == "ControlOrMeta" { if mac { "Meta" } else { "Control" } } else { name };
            let Some(definition) = key_definition(name, modifiers & 8 != 0) else { return Err(OpError::execution(format!("Unknown key: \"{name}\""))) };
            modifiers |= modifier_bit(&definition.key);
            self.dispatch_key("keyDown", &definition, modifiers, false).await?;
            held.push(definition);
        }
        let key = if key == "ControlOrMeta" { if mac { "Meta" } else { "Control" } } else { key };
        let Some(definition) = key_definition(key, modifiers & 8 != 0) else { return Err(OpError::execution(format!("Unknown key: \"{key}\""))) };
        self.dispatch_key("keyDown", &definition, modifiers, true).await?;
        self.dispatch_key("keyUp", &definition, modifiers, false).await?;
        for definition in held.iter().rev() {
            modifiers &= !modifier_bit(&definition.key);
            self.dispatch_key("keyUp", definition, modifiers, false).await?;
        }
        Ok(())
    }

    async fn dispatch_key(&self, kind: &str, definition: &KeyDefinition, modifiers: u32, with_commands: bool) -> OpResult<()> {
        // Typed text only when no shortcut modifier is held, as a keyboard does.
        let text = definition.text.as_ref().filter(|_| kind == "keyDown" && modifiers & 0b0111 == 0);
        let mut params = json!({
            "type": if kind == "keyDown" { if text.is_some() { "keyDown" } else { "rawKeyDown" } } else { "keyUp" },
            "key": definition.key,
            "code": definition.code,
            "windowsVirtualKeyCode": definition.key_code,
            "modifiers": modifiers,
            "location": definition.location,
        });
        if let Some(text) = text {
            params["text"] = json!(text);
            params["unmodifiedText"] = json!(text);
        }
        if with_commands && let Some(commands) = editing_command(&definition.key, modifiers) {
            params["commands"] = json!(commands);
        }
        self.send("Input.dispatchKeyEvent", params).await?;
        Ok(())
    }
}

/// The shortcuts of Apple viewers are no editing shortcut for Linux or headless Chromium, so they carry the command. Ctrl already works natively
/// on Linux, except copy and cut, which headless Chromium only runs as commands.
pub fn editing_command(key: &str, modifiers: u32) -> Option<Vec<&'static str>> {
    let modifier = modifiers & 0b0111;
    if modifier != 2 && modifier != 4 {
        return None;
    }
    let command = match key.to_lowercase().as_str() {
        "c" => "copy",
        "x" => "cut",
        _ if modifier != 4 => return None,
        "a" => "selectAll",
        "z" => {
            if modifiers & 8 != 0 {
                "redo"
            } else {
                "undo"
            }
        }
        _ => return None,
    };
    Some(vec![command])
}

fn modifier_bit(key: &str) -> u32 {
    match key {
        "Alt" => 1,
        "Control" => 2,
        "Meta" => 4,
        "Shift" => 8,
        _ => 0,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyDefinition {
    pub key: String,
    pub code: String,
    pub key_code: u32,
    pub text: Option<String>,
    pub location: u32,
}

fn named(key: &str, code: &str, key_code: u32, text: Option<&str>, location: u32) -> KeyDefinition {
    KeyDefinition { key: key.into(), code: code.into(), key_code, text: text.map(str::to_owned), location }
}

/// The US keyboard's definition of a key, by the name the tools use (`Enter`, `ArrowDown`, a single character, ...).
pub fn key_definition(name: &str, shift: bool) -> Option<KeyDefinition> {
    let definition = match name {
        "Enter" | "Return" => named("Enter", "Enter", 13, Some("\r"), 0),
        "Tab" => named("Tab", "Tab", 9, None, 0),
        "Backspace" => named("Backspace", "Backspace", 8, None, 0),
        "Delete" | "Del" => named("Delete", "Delete", 46, None, 0),
        "Escape" | "Esc" => named("Escape", "Escape", 27, None, 0),
        "Space" | " " => named(" ", "Space", 32, Some(" "), 0),
        "ArrowLeft" | "Left" => named("ArrowLeft", "ArrowLeft", 37, None, 0),
        "ArrowUp" | "Up" => named("ArrowUp", "ArrowUp", 38, None, 0),
        "ArrowRight" | "Right" => named("ArrowRight", "ArrowRight", 39, None, 0),
        "ArrowDown" | "Down" => named("ArrowDown", "ArrowDown", 40, None, 0),
        "Home" => named("Home", "Home", 36, None, 0),
        "End" => named("End", "End", 35, None, 0),
        "PageUp" => named("PageUp", "PageUp", 33, None, 0),
        "PageDown" => named("PageDown", "PageDown", 34, None, 0),
        "Insert" => named("Insert", "Insert", 45, None, 0),
        "Shift" => named("Shift", "ShiftLeft", 16, None, 1),
        "Control" | "Ctrl" => named("Control", "ControlLeft", 17, None, 1),
        "Alt" => named("Alt", "AltLeft", 18, None, 1),
        "Meta" | "Cmd" | "Command" => named("Meta", "MetaLeft", 91, None, 1),
        "CapsLock" => named("CapsLock", "CapsLock", 20, None, 0),
        function if function.len() >= 2 && function.starts_with('F') && function[1..].chars().all(|c| c.is_ascii_digit()) => {
            let number: u32 = function[1..].parse().ok()?;
            if !(1..=12).contains(&number) {
                return None;
            }
            named(function, function, 111 + number, None, 0)
        }
        other => {
            let mut chars = other.chars();
            let (Some(character), None) = (chars.next(), chars.next()) else { return None };
            return Some(character_definition(character, shift));
        }
    };
    Some(definition)
}

fn character_definition(character: char, shift: bool) -> KeyDefinition {
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    const PUNCTUATION: [(char, char, &str, u32); 11] = [
        ('`', '~', "Backquote", 192),
        ('-', '_', "Minus", 189),
        ('=', '+', "Equal", 187),
        ('[', '{', "BracketLeft", 219),
        (']', '}', "BracketRight", 221),
        ('\\', '|', "Backslash", 220),
        (';', ':', "Semicolon", 186),
        ('\'', '"', "Quote", 222),
        (',', '<', "Comma", 188),
        ('.', '>', "Period", 190),
        ('/', '?', "Slash", 191),
    ];
    let text = |key: char| Some(key.to_string());
    if character.is_ascii_alphabetic() {
        let key = if shift { character.to_ascii_uppercase() } else { character };
        return KeyDefinition { key: key.to_string(), code: format!("Key{}", character.to_ascii_uppercase()), key_code: u32::from(character.to_ascii_uppercase() as u8), text: text(key), location: 0 };
    }
    if character.is_ascii_digit() {
        let digit = character as u8 - b'0';
        let key = if shift { SHIFTED_DIGITS.as_bytes()[digit as usize] as char } else { character };
        return KeyDefinition { key: key.to_string(), code: format!("Digit{digit}"), key_code: u32::from(48 + digit), text: text(key), location: 0 };
    }
    if let Some(index) = SHIFTED_DIGITS.find(character) {
        return KeyDefinition { key: character.to_string(), code: format!("Digit{index}"), key_code: 48 + index as u32, text: text(character), location: 0 };
    }
    for (plain, shifted, code, key_code) in PUNCTUATION {
        if character == plain || character == shifted {
            let key = if character == shifted || shift { shifted } else { plain };
            return KeyDefinition { key: key.to_string(), code: code.into(), key_code, text: text(key), location: 0 };
        }
    }
    KeyDefinition { key: character.to_string(), code: String::new(), key_code: 0, text: text(character), location: 0 }
}

#[cfg(test)]
#[path = "server_browser_input_tests.rs"]
mod tests;
