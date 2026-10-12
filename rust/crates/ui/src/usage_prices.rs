//! The Usage page's editor of custom model prices and model mappings (`UsagePriceOverrides.tsx`): a table with a row for each model
//! that has a custom price or a mapping, a model's usage counting as the model it maps to. Edits are staged in the table and saved
//! together as one `server.updateSettings` patch; a failed save keeps them, says why, and offers a retry. The rules are
//! `t3_client::usage_prices`; this draws them.
//!
//! The original applies the edits to the environments picked in an "Apply to" menu; this page reads the one environment the window
//! is connected to (`usage_page.rs`), so that is the one the table edits, and its name stands where the menu is.
use crate::{
    provider_manage::follow_config,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use std::collections::BTreeSet;
use t3_client::{
    connection::ConnectionStatus,
    usage_prices::{self as logic, Draft, FIELDS, Plan, Target},
};
use t3_contracts::AuthEnvironmentScope;

/// The environment this window is connected to as the editor's one destination (`priceTargetsAtom`).
fn target_of(state: Store<UiModel>) -> Target {
    let config = state.config().read().clone();
    let settings = &config["settings"];
    let capabilities = &config["environment"]["capabilities"];
    let destination = state.destination().read().clone();
    let label = destination.as_ref().and_then(|id| state.environments().read().records.get(id).map(|record| record.label.clone())).unwrap_or_else(|| destination.as_ref().map(ToString::to_string).unwrap_or_default());
    let connected = *state.status().read() == ConnectionStatus::Connected;
    let settings_loaded = settings.is_object();
    let unavailable = logic::unavailable_reason(connected, settings_loaded, capabilities["usagePriceOverrides"] == true, connected_allows(state, AuthEnvironmentScope::SettingsWrite));
    Target {
        environment_id: destination.map(|id| id.to_string()).unwrap_or_default(),
        label,
        prices: if settings_loaded { logic::read_prices(settings) } else { None },
        aliases: if settings_loaded && capabilities["usageModelAliases"] == true { logic::read_aliases(settings) } else { None },
        unavailable: unavailable.map(str::to_owned),
    }
}

/// How the last save went.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Saving,
    Saved,
    Failed(String),
}
/// The edits a save sent and what became of them (`SaveAttempt`). A failed one keeps its edits for the retry.
#[derive(Debug, Clone, PartialEq)]
struct Attempt {
    drafts: Vec<Draft>,
    outcome: Outcome,
}

/// Edits custom model prices and mappings. `initial_model` opens with a new row for that model, unless it already has a row.
/// `models` are the models the page has usage for, offered while typing.
#[component]
pub(crate) fn Editor(state: Store<UiModel>, transport: TransportHandle, models: Vec<String>, initial_model: Option<String>, onclose: EventHandler<()>) -> Element {
    let target = target_of(state);
    let selected = std::slice::from_ref(&target);
    // A model that already has a custom price or mapping is edited in its existing row.
    let mut drafts = use_signal({
        let target = target.clone();
        move || match &initial_model {
            Some(model) if !target.prices.as_ref().is_some_and(|prices| prices.contains_key(model)) && !target.aliases.as_ref().is_some_and(|aliases| aliases.contains_key(model)) => vec![Draft::new_row("new:initial", model.clone())],
            _ => vec![],
        }
    });
    let mut attempt = use_signal(|| None::<Attempt>);
    let mut pending = use_signal(|| false);
    let mut next_row = use_signal(|| 0u32);
    // The new row to focus once it is in the table.
    let mut focus_row = use_signal(|| None::<String>);

    let custom_models: Vec<String> = target.prices.iter().flat_map(|prices| prices.keys()).chain(target.aliases.iter().flat_map(|aliases| aliases.keys())).cloned().collect::<BTreeSet<_>>().into_iter().collect();
    let all_models: Vec<String> = custom_models.iter().chain(models.iter()).cloned().collect::<BTreeSet<_>>().into_iter().collect();
    let current = drafts.read().clone();
    let attempted = attempt.read().clone();
    // Keep new rows in place while a successful save publishes the updated settings.
    let new_models: BTreeSet<String> = current.iter().filter(|draft| draft.is_new).map(|draft| draft.model.trim().to_owned()).collect();
    let rows: Vec<Draft> = custom_models
        .iter()
        .filter(|model| attempted.is_none() || !new_models.contains(*model))
        .map(|model| current.iter().find(|draft| draft.id == format!("model:{model}")).cloned().unwrap_or_else(|| Draft { id: format!("model:{model}"), model: model.clone(), ..Draft::default() }))
        .chain(current.iter().filter(|draft| draft.is_new).cloned())
        .collect();
    let staged: Vec<Draft> = current.iter().filter(|draft| !logic::is_empty_draft(draft)).cloned().collect();
    let mut errors = logic::table_errors(selected, &staged);
    for draft in staged.iter().filter(|draft| draft.is_new) {
        let model = draft.model.trim();
        if model.is_empty() {
            errors.insert(draft.id.clone(), "Enter a model ID.".to_owned());
        } else if attempted.is_none() && (custom_models.iter().any(|custom| custom == model) || current.iter().any(|other| other.id != draft.id && other.model.trim() == model)) {
            errors.insert(draft.id.clone(), "This model already has a row. Edit its prices there.".to_owned());
        }
    }
    let failed = matches!(attempted.as_ref().map(|attempt| &attempt.outcome), Some(Outcome::Failed(_)));
    let busy = pending();
    let locked = busy || failed;
    let has_changes = !staged.is_empty();

    let update = use_callback(move |draft: Draft| {
        drafts.with_mut(|list| match list.iter_mut().find(|entry| entry.id == draft.id) {
            Some(entry) => *entry = draft,
            None => list.push(draft),
        });
        attempt.set(None);
    });
    let update_or_drop = use_callback(move |draft: Draft| {
        if !draft.is_new && draft.values.is_empty() && draft.alias.is_none() {
            drafts.with_mut(|list| list.retain(|entry| entry.id != draft.id));
            attempt.set(None);
        } else {
            update.call(draft);
        }
    });
    // An existing row edited back to its saved values has nothing left to save.
    let matches_saved = |row: &Draft, cell: &logic::Cell, value: &str| !row.is_new && cell.placeholder != "Mixed" && cell.placeholder != "Unavailable" && value == cell.value;
    let edit_cell = use_callback(move |(row, field, value): (Draft, logic::Field, String)| {
        let target = target_of(state);
        let mut values = row.values.clone();
        values.insert(field.key, value.clone());
        if matches_saved(&row, &logic::price_cell(std::slice::from_ref(&target), &row.model, field), &value) {
            values.remove(field.key);
        }
        update_or_drop.call(Draft { values, ..row });
    });
    let edit_alias = use_callback(move |(row, value): (Draft, String)| {
        let target = target_of(state);
        let saved = matches_saved(&row, &logic::alias_cell(std::slice::from_ref(&target), &row.model), &value);
        update_or_drop.call(Draft { alias: if saved { None } else { Some(value) }, ..row });
    });
    let discard = move |_| {
        drafts.set(vec![]);
        attempt.set(None);
    };
    let save = {
        let transport = transport.clone();
        use_callback(move |retry: bool| {
            if *pending.peek() {
                return;
            }
            let target = target_of(state);
            let edits = match (retry, attempt.peek().clone()) {
                (true, Some(previous)) => previous.drafts,
                _ => drafts.peek().iter().filter(|draft| !logic::is_empty_draft(draft)).cloned().collect(),
            };
            if edits.is_empty() {
                return;
            }
            let (changes, edit_errors) = logic::table_changes(&target, &edits);
            // An environment that cannot be written to reports a failure; so does one whose edits are not valid.
            let mut destination = target.clone();
            destination.unavailable = target.unavailable.clone().or_else(|| edit_errors.values().next().cloned());
            let plan = logic::plan_write(&destination, &changes);            pending.set(true);
            attempt.set(Some(Attempt { drafts: edits, outcome: Outcome::Saving }));
            let transport = transport.clone();
            spawn(async move {
                let result = match plan {
                    Plan::Fail(reason) => Err(reason),
                    Plan::Nothing => Ok(()),
                    Plan::Patch(patch) => match runtime::request_value(transport.clone(), state, "server.updateSettings", patch).await {
                        Ok(_) => {
                            follow_config(&transport, state).await;
                            Ok(())
                        }
                        Err(_) => Err(logic::SAVE_FAILED.to_owned()),
                    },
                };
                pending.set(false);
                match result {
                    Ok(()) => {
                        attempt.with_mut(|attempt| {
                            if let Some(attempt) = attempt {
                                attempt.outcome = Outcome::Saved;
                            }
                        });
                        drafts.set(vec![]);
                    }
                    Err(reason) => attempt.with_mut(|attempt| {
                        if let Some(attempt) = attempt {
                            attempt.outcome = Outcome::Failed(reason);
                        }
                    }),
                }
            });
        })
    };
    let close = move || {
        if !*pending.peek() {
            onclose.call(());
        }
    };
    let destination_label = target.label.clone();
    let save_disabled = busy || (!has_changes && !failed) || (!failed && !errors.is_empty());
    let save_label = if busy { "Saving\u{2026}" } else if failed { "Retry failed saves" } else { "Save changes" };
    // The rows' inputs, built with the cells they read.
    let table_rows: Vec<Element> = rows
        .iter()
        .map(|row| {
            let alias_cell = logic::alias_cell(selected, &row.model);
            let alias_text = row.alias.clone().unwrap_or_else(|| alias_cell.value.clone());
            let alias = alias_text.trim().to_owned();
            let row_error = errors.get(&row.id).cloned().filter(|_| !row.model.trim().is_empty() || row.values.values().any(|value| !value.is_empty()));
            let model_label = if row.model.is_empty() { "new model".to_owned() } else { row.model.clone() };
            let invalid = !row.model.trim().is_empty() && errors.contains_key(&row.id);
            let cells: Vec<(logic::Field, String, &'static str)> = FIELDS
                .iter()
                .map(|field| {
                    let cell = logic::price_cell(selected, &row.model, *field);
                    let typed = row.values.get(field.key).cloned().unwrap_or_else(|| if row.is_new { String::new() } else { cell.value.clone() });
                    let placeholder = if row.is_new { if field.optional { "Input rate" } else { "0.00" } } else { cell.placeholder };
                    (*field, typed, placeholder)
                })
                .collect();
            let remove_label = if row.removed { format!("Undo reset for {}", row.model) } else if row.is_new { "Remove new model".to_owned() } else { format!("Reset price for {} to automatic", row.model) };
            let remove_tip = if row.removed { "Undo reset" } else if row.is_new { "Remove row" } else { "Reset to automatic" };
            let (for_model, for_alias, for_remove) = (row.clone(), row.clone(), row.clone());
            let for_cells = row.clone();
            rsx! {
                tr { key: "{row.id}", "data-row-id": "{row.id}",
                    td {
                        if row.is_new {
                            input { class: "usage-price-input", "aria-label": "New model ID", "aria-invalid": "{invalid}", list: "usage-price-models", placeholder: "Model ID", autocomplete: "off", spellcheck: false, disabled: locked, value: "{row.model}",
                                onmounted: {
                                    let id = row.id.clone();
                                    move |event| {
                                        if focus_row.peek().as_deref() == Some(id.as_str()) {
                                            focus_row.set(None);
                                            let element = event.data();
                                            spawn(async move { let _ = element.set_focus(true).await; });
                                        }
                                    }
                                },
                                oninput: move |event| update.call(Draft { model: event.value(), ..for_model.clone() }) }
                        } else {
                            span { class: if row.removed { "usage-price-model removed" } else { "usage-price-model" }, "{row.model}" }
                        }
                        if let Some(message) = row_error { p { role: "alert", class: "usage-price-error", "{message}" } }
                    }
                    if row.removed {
                        td { colspan: "5", span { class: "muted", "Automatic pricing after saving" } }
                    } else {
                        td {
                            input { class: "usage-price-input", "aria-label": "Map {model_label} to model", list: "usage-alias-models", placeholder: if row.is_new { "Optional" } else { alias_cell.placeholder }, autocomplete: "off", spellcheck: false, disabled: locked, value: "{alias_text}",
                                oninput: move |event| edit_alias.call((for_alias.clone(), event.value())) }
                        }
                        if !alias.is_empty() {
                            td { colspan: "4", span { class: "muted usage-price-model", "Counted as {alias}" } }
                        } else {
                            for (field, typed, placeholder) in cells {
                                td { key: "{field.key}",
                                    input { class: "usage-price-input mono", inputmode: "decimal", "aria-label": "{field.label} price for {model_label}", placeholder, autocomplete: "off", disabled: locked, value: "{typed}",
                                        oninput: { let row = for_cells.clone(); move |event| edit_cell.call((row.clone(), field, event.value())) } }
                                }
                            }
                        }
                    }
                    td {
                        button { r#type: "button", class: "icon-button", "aria-label": "{remove_label}", title: "{remove_tip}", disabled: locked,
                            onclick: move |_| {
                                if for_remove.is_new {
                                    drafts.with_mut(|list| list.retain(|entry| entry.id != for_remove.id));
                                    attempt.set(None);
                                } else if for_remove.removed {
                                    update_or_drop.call(Draft { removed: false, ..for_remove.clone() });
                                } else {
                                    update.call(Draft { removed: true, ..for_remove.clone() });
                                }
                            },
                            if row.is_new { "\u{00d7}" } else { "\u{21ba}" } }
                    }
                }
            }
        })
        .collect();
    let empty_note = if target.prices.is_none() { "Some environment prices are unavailable." } else { "No custom prices or mappings. Add a row to set one." };
    rsx! { div { class: "modal-backdrop", onclick: move |_| close(),
        div { class: "dialog usage-prices-dialog", role: "dialog", "aria-modal": "true", "aria-label": "Custom model prices", tabindex: "-1", onclick: move |event| event.stop_propagation(),
            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); close(); } },
            h2 { "Custom model prices" }
            p { class: "muted", "Prices and mappings apply to all past and future usage on the environment shown." }
            div { class: "usage-prices-head",
                span { class: "usage-prices-apply", span { class: "muted", "Apply to " } strong { "{destination_label}" }
                    if let Some(reason) = target.unavailable.clone() { span { class: "muted", " \u{b7} {reason}" } } }
                span { class: "muted", "USD / million tokens" }
            }
            div { class: "usage-prices-table",
                table { "aria-label": "Custom model prices",
                    thead { tr {
                        th { "Model ID" } th { "Map to" }
                        for field in FIELDS { th { key: "{field.key}", "{field.label}" } }
                        th { button { r#type: "button", class: "icon-button", "aria-label": "Add model price", title: "Add model price", disabled: locked,
                            onclick: move |_| {
                                let id = format!("new:{}", next_row.peek());
                                next_row.with_mut(|next| *next += 1);
                                focus_row.set(Some(id.clone()));
                                update.call(Draft::new_row(id, ""));
                            }, "+" } }
                    } }
                    tbody {
                        if table_rows.is_empty() { tr { td { colspan: "7", class: "muted usage-prices-empty", "{empty_note}" } } }
                        for row in table_rows { {row} }
                    }
                }
            }
            datalist { id: "usage-price-models", for model in all_models.iter().filter(|model| !custom_models.contains(*model)) { option { key: "{model}", value: "{model}" } } }
            datalist { id: "usage-alias-models", for model in all_models.iter().filter(|model| !target.aliases.as_ref().is_some_and(|aliases| aliases.contains_key(*model))) { option { key: "{model}", value: "{model}" } } }
            p { class: "muted usage-prices-note", "Blank cache rates use the input price. Enter 0 for free tokens. A mapped model\u{2019}s usage counts as the model it maps to." }
            if let Some(attempt) = attempted.clone() {
                div { role: "status", class: "usage-prices-status",
                    span { class: "muted", "{destination_label}" }
                    span { class: if matches!(attempt.outcome, Outcome::Failed(_)) { "failure" } else { "muted" },
                        match &attempt.outcome { Outcome::Failed(reason) => format!("Not saved \u{b7} {reason}"), Outcome::Saved => "Saved".to_owned(), Outcome::Saving => "Saving\u{2026}".to_owned() } }
                }
            }
            div { class: "dialog-actions",
                span { class: "muted usage-prices-apply-note", if has_changes { "Changes apply to {destination_label}" } }
                if has_changes { button { r#type: "button", "aria-label": if failed { "Discard pending changes" } else { "Discard changes" }, disabled: busy, onclick: discard, if failed { "Discard pending changes" } else { "Discard changes" } } }
                button { r#type: "button", class: "primary", "aria-label": "{save_label}", disabled: save_disabled, onclick: move |_| save.call(failed), "{save_label}" }
                button { r#type: "button", "aria-label": "Close", disabled: busy, onclick: move |_| close(), "Close" }
            }
        }
    } }
}
