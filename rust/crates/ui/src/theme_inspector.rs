//! Editor state stays in Rust; the shared Rust WASM utility owns DOM operations.
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use t3_client::themes::{Appearance, Colors, inspector};
pub fn use_inspector(
    mut selected: Signal<Option<String>>,
    mut armed: Signal<bool>,
    mut advanced: Signal<bool>,
    mut query: Signal<String>,
    mut count: Signal<Option<usize>>,
    mut error: Signal<Option<String>>,
    colors: Signal<BTreeMap<Appearance, Colors>>,
    mode: Signal<Appearance>,
) {
    let service = use_context::<crate::themes::Themes>();
    let id = use_hook(|| format!("theme-inspector-{}", uuid::Uuid::new_v4()));
    let mount_id = id.clone();
    let mut ready = use_signal(|| false);
    let mut eval = use_signal(|| None::<document::Eval>);
    use_future(move || {
        let config = {
            let catalog = service.catalog.peek();
            json!({"roles":catalog.data.roles,"variables":catalog.data.variables})
        };
        let mount_id = mount_id.clone();
        async move {
            let base = crate::terminal_pane::SURFACE.to_string();
            let args = json!({"id":mount_id,"base":base,"wasm":format!("{base}/t3_terminal_bg.wasm"),"config":config});
            let mut bridge = document::eval(&format!(
                "const args={args};\n{}",
                include_str!("../assets/theme_inspector_abi.js")
            ));
            eval.set(Some(bridge));
            loop {
                let event: Value = match bridge.recv().await {
                    Ok(event) => event,
                    Err(cause) => {
                        error.set(Some(format!("Theme inspector disconnected: {cause}")));
                        break;
                    }
                };
                match event["type"].as_str() {
                    Some("ready") => ready.set(true),
                    Some("role") => {
                        if let Some(role) = event["role"].as_str() {
                            let visible = inspector::family(role)
                                .map(|family| family.role.clone())
                                .unwrap_or_else(|| role.into());
                            if !["canvas", "accent"].contains(&visible.as_str()) {
                                advanced.set(true);
                                query.set(String::new());
                            }
                            selected.set(Some(visible.clone()));
                            let _ = bridge.send(json!({"type":"reveal","role":visible}));
                        }
                    }
                    Some("disarmed") => armed.set(false),
                    Some("cancel") => {
                        selected.set(None);
                        armed.set(false);
                        count.set(None);
                    }
                    Some("count") => count.set(event["count"].as_u64().map(|count| count as usize)),
                    Some("error") => {
                        error.set(event["message"].as_str().map(str::to_owned));
                        break;
                    }
                    _ => {}
                }
            }
            ready.set(false);
            eval.set(None);
            let _ = bridge.send(json!({"type":"dispose"}));
        }
    });
    let roles = use_context::<crate::themes::Themes>()
        .catalog
        .peek()
        .data
        .roles
        .clone();
    use_effect(move || {
        let selected = selected.read();
        let advanced = *advanced.read();
        let mode = *mode.read();
        let colors = colors.read();
        let armed = *armed.read();
        if *ready.read() {
            if let Some(bridge) = *eval.read() {
                let roles = inspector::highlight_roles(
                    selected.as_deref(),
                    advanced,
                    &colors[&mode],
                    &roles,
                );
                if roles.is_empty() || armed {
                    count.set(None);
                }
                let _ = bridge.send(json!({"type":"selection","roles":roles,"armed":armed}));
            }
        }
    });
    use_drop(move || {
        if let Some(bridge) = *eval.peek() {
            let _ = bridge.send(json!({"type":"dispose"}));
        }
        let id = json!(id);
        document::eval(&format!(
            "const slot=window.__t3RustThemeInspectors?.get({id});if(slot){{slot.disposed=true;slot.surface?.dispose();}}"
        ));
    });
}
