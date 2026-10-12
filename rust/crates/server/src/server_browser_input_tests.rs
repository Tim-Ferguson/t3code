//! What an agent's actions send to the page (apps/server/src/preview/ServerBrowserPage.test.ts): which events a click, a keystroke, a drag or a
//! selection become, what each waits for, and what it reports when its target is not there. The page is played by `server_browser_double`.
use super::*;
use crate::server_browser_page::tests::rig as make_rig;
use serde_json::json;
use std::sync::Mutex;
use t3_contracts::PreviewKeyModifier;

fn script(rig: &crate::server_browser_page::tests::Rig, method: &str, value: Value) {
    rig.double.on_evaluate(&format!(".{method}("), value);
}

fn kinds(rig: &crate::server_browser_page::tests::Rig, method: &str) -> Vec<(String, Value)> {
    rig.browser.commands_named(method).into_iter().map(|command| (command.params["type"].as_str().unwrap_or_default().to_owned(), command.params)).collect()
}

fn recording_pointer() -> (PointerReporter, Arc<Mutex<Vec<(Point, PointerPhase)>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    (Arc::new(move |point, phase| {
        log.lock().unwrap().push((point, phase));
        Box::pin(async {})
    }), seen)
}

fn click(input: Value) -> PreviewAutomationClickInput {
    serde_json::from_value(input).unwrap()
}

#[test]
fn keys_are_the_us_keyboards() {
    let enter = key_definition("Enter", false).unwrap();
    assert_eq!((enter.key.as_str(), enter.code.as_str(), enter.key_code, enter.text.as_deref()), ("Enter", "Enter", 13, Some("\r")));
    let a = key_definition("a", false).unwrap();
    assert_eq!((a.key.as_str(), a.code.as_str(), a.key_code, a.text.as_deref()), ("a", "KeyA", 65, Some("a")));
    // Shift gives the capital and the shifted symbol, and a symbol written shifted is itself.
    assert_eq!(key_definition("a", true).unwrap().key, "A");
    let one = key_definition("1", true).unwrap();
    assert_eq!((one.key.as_str(), one.code.as_str(), one.key_code), ("!", "Digit1", 49));
    assert_eq!(key_definition("!", false).unwrap().code, "Digit1");
    let slash = key_definition("/", false).unwrap();
    assert_eq!((slash.code.as_str(), slash.key_code), ("Slash", 191));
    assert_eq!(key_definition("?", false).unwrap().code, "Slash");
    assert_eq!(key_definition("Space", false).unwrap().text.as_deref(), Some(" "));
    assert_eq!((key_definition("F5", false).unwrap().key_code, key_definition("ArrowDown", false).unwrap().key_code), (116, 40));
    assert_eq!(key_definition("Shift", false).unwrap().location, 1);
    // An unknown name is none, and a character the layout lacks is typed as itself.
    assert!(key_definition("Hyper", false).is_none() && key_definition("F13", false).is_none());
    assert_eq!(key_definition("é", false).unwrap().text.as_deref(), Some("é"));
    assert_eq!((editing_command("c", 2), editing_command("C", 4), editing_command("a", 4), editing_command("a", 2), editing_command("z", 4 | 8), editing_command("z", 4)), (Some(vec!["copy"]), Some(vec!["copy"]), Some(vec!["selectAll"]), None, Some(vec!["redo"]), Some(vec!["undo"])));
    assert_eq!(editing_command("c", 0), None);
}

#[tokio::test]
async fn a_chord_presses_its_modifiers_then_the_key_and_lets_go_in_reverse() {
    let rig = make_rig().await;
    rig.page.press(&PreviewAutomationPressInput { key: Some("a".into()), modifiers: Some(vec![PreviewKeyModifier::Control, PreviewKeyModifier::Shift]), ..Default::default() }).await.unwrap();
    let events: Vec<(String, String, u64)> = rig.browser.commands_named("Input.dispatchKeyEvent").iter().map(|command| (command.params["type"].as_str().unwrap().to_owned(), command.params["key"].as_str().unwrap().to_owned(), command.params["modifiers"].as_u64().unwrap())).collect();
    assert_eq!(
        events,
        [
            ("rawKeyDown".into(), "Control".into(), 2),
            ("rawKeyDown".into(), "Shift".into(), 10),
            // With Control held, the key is a shortcut and types nothing.
            ("rawKeyDown".into(), "A".into(), 10),
            ("keyUp".into(), "A".into(), 10),
            ("keyUp".into(), "Shift".into(), 2),
            ("keyUp".into(), "Control".into(), 0),
        ]
    );
    // A plain key types its text, and the key `+` is itself.
    rig.page.press_keys("Enter").await.unwrap();
    rig.page.press_keys("Shift++").await.unwrap();
    let sent = rig.browser.commands_named("Input.dispatchKeyEvent");
    assert_eq!((sent[6].params["type"].clone(), sent[6].params["text"].clone()), (json!("keyDown"), json!("\r")));
    assert_eq!(sent.iter().filter(|command| command.params["key"] == "+").count(), 2);
    assert_eq!(sent.last().unwrap().params["type"], "keyUp");
    // Select-all as the platform's modifier, which carries the command where the platform needs one.
    rig.page.press_keys("ControlOrMeta+A").await.unwrap();
    let all = rig.browser.commands_named("Input.dispatchKeyEvent");
    let chord_key = all.iter().rev().find(|command| command.params["key"] == "A" && command.params["type"] == "rawKeyDown").unwrap();
    assert_eq!(chord_key.params["modifiers"], if cfg!(target_os = "macos") { 4 } else { 2 });
    assert_eq!(chord_key.params.get("commands").is_some(), cfg!(target_os = "macos"));
    let unknown = rig.page.press_keys("Hyper+A").await.unwrap_err();
    assert_eq!(unknown.message, "Unknown key: \"Hyper\"");
}

#[tokio::test]
async fn a_click_goes_to_the_point_the_page_gives_after_the_pointer_has_shown_it() {
    let rig = make_rig().await;
    script(&rig, "probe", json!({"status": "ok", "x": 10.5, "y": 20}));
    let (pointer, seen) = recording_pointer();
    let point = rig.page.click(&click(json!({"locator": "role=button[name='Save']"})), &pointer).await.unwrap();
    assert_eq!(point, Point { x: 10.5, y: 20.0 });
    assert_eq!(*seen.lock().unwrap(), [(Point { x: 10.5, y: 20.0 }, PointerPhase::Click)]);
    let order: Vec<String> = kinds(&rig, "Input.dispatchMouseEvent").into_iter().map(|(kind, _)| kind).collect();
    assert_eq!(order, ["mouseMoved", "mousePressed", "mouseReleased"]);
    // The page was asked to scroll to the element and to check it is enabled and not covered.
    let asked = rig.browser.commands_named("Runtime.evaluate").into_iter().map(|command| command.params["expression"].as_str().unwrap().to_owned()).find(|expression| expression.contains(".probe(")).unwrap();
    assert!(asked.contains(r#"{"scroll":true,"enabled":true,"hit":true}"#), "{asked}");
    // Coordinates need no element at all.
    let (pointer, _) = recording_pointer();
    let point = rig.page.click(&click(json!({"x": 5, "y": 6, "button": "middle"})), &pointer).await.unwrap();
    assert_eq!(point, Point { x: 5.0, y: 6.0 });
    let pressed = kinds(&rig, "Input.dispatchMouseEvent").into_iter().filter(|(kind, _)| kind == "mousePressed").last().unwrap().1;
    assert_eq!((pressed["button"].clone(), pressed["buttons"].clone(), pressed["x"].clone()), (json!("middle"), json!(4), json!(5.0)));
}

#[tokio::test(start_paused = true)]
async fn an_action_waits_for_its_target_and_says_what_it_waited_on() {
    let rig = make_rig().await;
    // The page never finds it.
    let missing = rig.page.click(&click(json!({"selector": "#gone", "timeoutMs": 450})), &no_pointer()).await.unwrap_err();
    assert_eq!((missing.tag.as_str(), missing.message.as_str()), ("PreviewAutomationTimeoutError", "locator.click: Timeout 450ms exceeded.\n  - waiting for element"));
    assert!(kinds(&rig, "Input.dispatchMouseEvent").is_empty());
    // It finds it, but something covers it, and the last reason it gave is the one reported.
    script(&rig, "probe", json!({"status": "retry", "reason": "<div#overlay> intercepts pointer events"}));
    let covered = rig.page.hover(&serde_json::from_value(json!({"selector": "#save", "timeoutMs": 300})).unwrap(), &no_pointer()).await.unwrap_err();
    assert_eq!(covered.message, "locator.hover: Timeout 300ms exceeded.\n  - <div#overlay> intercepts pointer events");
    assert!(kinds(&rig, "Input.dispatchMouseEvent").is_empty());
    // A ref that is not the page's is refused before the page is asked at all.
    let stale = rig.page.click(&click(json!({"locator": "aria-ref=e1"})), &no_pointer()).await.unwrap_err();
    assert_eq!(stale, OpError::stale_ref());
}

#[tokio::test]
async fn a_click_that_opens_a_dialog_returns_before_the_pages_reply() {
    let rig = make_rig().await;
    script(&rig, "probe", json!({"status": "ok", "x": 1, "y": 1}));
    // The release of the button never returns, as when the page's handler opens a dialog and waits.
    let held = rig.double.hold("Input.dispatchMouseEvent", 2);
    let session = rig.page.session_id().to_owned();
    let browser = rig.browser.clone();
    let (input, pointer) = (click(json!({"x": 1, "y": 1})), no_pointer());
    let clicking = rig.page.click(&input, &pointer);
    let opening = async {
        held.notified().await;
        browser.push(json!({"method": "Page.javascriptDialogOpening", "sessionId": session, "params": {"type": "alert", "message": "Saved", "defaultPrompt": "", "url": "http://x/"}}));
    };
    let (clicked, ()) = tokio::time::timeout(crate::test_support::HANG_BOUND, async { tokio::join!(clicking, opening) }).await.unwrap();
    clicked.unwrap();
    rig.double.release(&rig.browser, "Input.dispatchMouseEvent");
}

#[tokio::test]
async fn a_drag_is_one_gesture_from_source_to_target_and_a_native_drag_is_dropped() {
    let rig = make_rig().await;
    let (pointer, seen) = recording_pointer();
    rig.double.on_evaluate("\"#source\"", json!({"status": "ok", "x": 10, "y": 10}));
    rig.double.on_evaluate("\"#target\"", json!({"status": "ok", "x": 110, "y": 50}));
    let drag: PreviewAutomationDragInput = serde_json::from_value(json!({"source": "#source", "target": "#target"})).unwrap();
    rig.page.drag(&drag, &pointer).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), [(Point { x: 10.0, y: 10.0 }, PointerPhase::Move), (Point { x: 110.0, y: 50.0 }, PointerPhase::Move)]);
    let moves: Vec<(String, f64, f64)> = kinds(&rig, "Input.dispatchMouseEvent").into_iter().map(|(kind, params)| (kind, params["x"].as_f64().unwrap(), params["y"].as_f64().unwrap())).collect();
    assert_eq!(moves.first().unwrap(), &("mouseMoved".to_owned(), 10.0, 10.0));
    assert_eq!(moves[1].0, "mousePressed");
    assert_eq!(moves[3], ("mouseMoved".to_owned(), 60.0, 30.0));
    assert_eq!(moves.last().unwrap(), &("mouseReleased".to_owned(), 110.0, 50.0));
    assert!(rig.browser.commands_named("Input.dispatchDragEvent").is_empty());
    // A page whose element starts a native drag gets the drag events the protocol keeps back until a drop.
    let session = rig.page.session_id().to_owned();
    let held = rig.double.hold("Input.dispatchMouseEvent", 5);
    let browser = rig.browser.clone();
    let pointer = no_pointer();
    let dragging = rig.page.drag(&drag, &pointer);
    let intercepting = async {
        held.notified().await;
        browser.push(json!({"method": "Input.dragIntercepted", "sessionId": session, "params": {"data": {"items": [{"mimeType": "text/plain", "data": "x"}], "dragOperationsMask": 1}}}));
        rig.double.release(&browser, "Input.dispatchMouseEvent");
    };
    tokio::time::timeout(crate::test_support::HANG_BOUND, async { tokio::join!(dragging, intercepting) }).await.unwrap().0.unwrap();
    let dropped: Vec<String> = rig.browser.commands_named("Input.dispatchDragEvent").iter().map(|command| command.params["type"].as_str().unwrap().to_owned()).collect();
    assert_eq!(dropped, ["dragEnter", "dragOver", "drop"]);
}

#[tokio::test]
async fn typing_writes_to_the_focused_text_control_after_clearing_it_when_asked() {
    let rig = make_rig().await;
    let typing = |input: Value| serde_json::from_value::<PreviewAutomationTypeInput>(input).unwrap();
    // Into the focused element: it takes the text only if it is an enabled text control.
    script(&rig, "focusEditable", json!(true));
    rig.page.type_text(&typing(json!({"text": "héllo"}))).await.unwrap();
    assert_eq!(rig.browser.commands_named("Input.insertText")[0].params, json!({"text": "héllo"}));
    let focus = rig.browser.commands_named("Runtime.evaluate").into_iter().map(|command| command.params["expression"].as_str().unwrap().to_owned()).find(|expression| expression.contains(".focusEditable(")).unwrap();
    assert!(focus.ends_with("focusEditable(null)"), "{focus}");
    assert!(rig.browser.commands_named("Input.dispatchKeyEvent").is_empty());
    // Clearing first selects everything and deletes it, then types.
    rig.page.type_text(&typing(json!({"text": "x", "clear": true}))).await.unwrap();
    let keys: Vec<String> = rig.browser.commands_named("Input.dispatchKeyEvent").iter().filter(|command| command.params["type"] != "keyUp").map(|command| command.params["key"].as_str().unwrap().to_owned()).collect();
    assert_eq!(keys, [if cfg!(target_os = "macos") { "Meta" } else { "Control" }, "A", "Delete"]);
    assert_eq!(rig.browser.commands_named("Input.insertText").len(), 2);
}

#[tokio::test]
async fn typing_with_clear_into_a_locator_fills_it_and_a_control_that_is_not_text_is_refused() {
    let rig = make_rig().await;
    let typing = |input: Value| serde_json::from_value::<PreviewAutomationTypeInput>(input).unwrap();
    script(&rig, "fill", json!(true));
    rig.page.type_text(&typing(json!({"text": "hello", "clear": true, "locator": "textarea"}))).await.unwrap();
    assert!(rig.browser.commands_named("Input.insertText").is_empty());
    let rig = make_rig().await;
    script(&rig, "fill", json!(false));
    let refused = rig.page.type_text(&typing(json!({"text": "hello", "clear": true, "locator": "#checkbox"}))).await.unwrap_err();
    assert_eq!((refused.tag.as_str(), refused.message.as_str()), ("PreviewAutomationTargetNotEditableError", "The target is not an enabled text field, so no text was typed."));
    let rig = make_rig().await;
    script(&rig, "focusEditable", json!(false));
    assert_eq!(rig.page.type_text(&typing(json!({"text": "x"}))).await.unwrap_err().tag, "PreviewAutomationTargetNotEditableError");
    assert!(rig.browser.commands_named("Input.insertText").is_empty());
}

#[tokio::test(start_paused = true)]
async fn selecting_maps_each_entry_to_an_option_by_value_then_label_and_waits_for_missing_ones() {
    let rig = make_rig().await;
    let select = |input: Value| serde_json::from_value::<PreviewAutomationSelectInput>(input).unwrap();
    script(&rig, "selectInfo", json!([{"value": "s", "label": "Small"}, {"value": "m", "label": "Medium"}, {"value": "l", "label": "Large"}]));
    script(&rig, "selectOptions", json!(["m", "l"]));
    let selected = rig.page.select(&select(json!({"locator": "select", "values": ["Medium", " Large"]}))).await.unwrap();
    assert_eq!(selected, ["m", "l"]);
    let chosen = rig.browser.commands_named("Runtime.evaluate").into_iter().map(|command| command.params["expression"].as_str().unwrap().to_owned()).find(|expression| expression.contains(".selectOptions(")).unwrap();
    assert!(chosen.ends_with(r#"selectOptions("select",["m","l"])"#), "{chosen}");
    // An option the page does not have waits, as Playwright does, and then times out.
    let missing = rig.page.select(&select(json!({"selector": "select", "values": ["Huge"], "timeoutMs": 250}))).await.unwrap_err();
    assert_eq!((missing.tag.as_str(), missing.message.as_str()), ("PreviewAutomationTimeoutError", "locator.selectOption: Timeout 250ms exceeded."));
}

#[tokio::test]
async fn selecting_in_something_that_is_not_a_select_says_so() {
    let rig = make_rig().await;
    script(&rig, "selectInfo", Value::Null);
    let error = rig.page.select(&serde_json::from_value(json!({"locator": "div", "values": ["a"]})).unwrap()).await.unwrap_err();
    assert_eq!(error.tag, "PreviewAutomationTargetNotEditableError");
}

#[tokio::test]
async fn files_are_given_to_a_file_input_by_path_and_other_elements_are_refused() {
    let rig = make_rig().await;
    script(&rig, "fileInputState", json!("notInput"));
    let refused = rig.page.set_input_files(Some("div"), &["/tmp/a.png".into()], None).await.unwrap_err();
    assert_eq!(refused.tag, "PreviewAutomationTargetNotEditableError");
    assert!(rig.browser.commands_named("DOM.setFileInputFiles").is_empty());
    // With no locator the call is about the page's open picker, not an input.
    assert!(!rig.page.set_input_files(None, &["/tmp/a.png".into()], None).await.unwrap());
}

#[tokio::test]
async fn scrolling_asks_the_page_to_scroll_the_viewport_or_a_container() {
    let rig = make_rig().await;
    script(&rig, "scrollBy", json!(true));
    rig.page.scroll(&serde_json::from_value(json!({"deltaY": 300.5})).unwrap()).await.unwrap();
    rig.page.scroll(&serde_json::from_value(json!({"deltaX": -10, "locator": ".list"})).unwrap()).await.unwrap();
    let calls: Vec<String> = rig.browser.commands_named("Runtime.evaluate").into_iter().map(|command| command.params["expression"].as_str().unwrap().to_owned()).filter(|expression| expression.contains(".scrollBy(")).collect();
    assert!(calls[0].ends_with("scrollBy(null,0.0,300.5)") && calls[1].ends_with(r#"scrollBy(".list",-10.0,0.0)"#), "{calls:?}");
}
