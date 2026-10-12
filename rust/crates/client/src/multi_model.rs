//! Starting one message on several models: the rules of `multipleModelSelections` in the original's
//! `ChatComposer.tsx` (the picker's `onToggleModel`, `getSendContext`, `sendDisabledReason`),
//! `ProviderModelPicker.tsx` (the trigger's label) and `ChatView.tsx` (`onSend`'s checks, the
//! per-model provider check, the toasts and the retry key). The form that offers it, and the sends,
//! are `multi_model.rs` of the `ui` crate; nothing here touches the transport.
use crate::{
    models::{ANTIGRAVITY_DEFAULT_MODEL, ModelOption},
    new_thread::InstanceModelOption,
};
use t3_contracts::{
    ModelSelection, ProviderInstanceId, ServerConfig, ServerProvider, ServerProviderAuthStatus,
    ServerProviderAvailability, ServerProviderState, TrimmedNonEmptyString,
};

/// Shown when the server cannot prepare a worktree for each thread (`ChatView`'s `onSend`).
pub const UPDATE_SERVER: &str = "Update this server before starting multiple models.";
/// The warning when the form is not a new thread of a Git project with a base branch.
pub const CHOOSE_TITLE: &str = "Choose models and a base branch";
pub const CHOOSE_DETAIL: &str = "Multiple models need a new thread in a Git project. Each gets its own worktree.";
/// Why the send is disabled when every model was unchecked.
pub const SELECT_ONE: &str = "Select at least one model.";
/// What the workspace control says while several models are chosen: it is fixed to a new worktree.
pub const WORKTREE_PER_MODEL: &str = "Each model starts in its own worktree.";
/// The refusal of a second send for a model whose first request may have reached the server.
pub const MAY_HAVE_STARTED: &str = "The previous request may have started. Open its thread to check before sending again.";
/// The question behind "Allow retry".
pub const ALLOW_RETRY_QUESTION: &str = "The previous request may already be running. Check its thread first. Allow another send that could create a duplicate thread?";

/// `capabilities.requiredWorktreeBootstrap`: the picker offers several models only where the server
/// can start a worktree for each thread.
pub fn supported(config: &ServerConfig) -> bool {
    config.environment.capabilities.required_worktree_bootstrap == Some(true)
}

/// `resolveModelPickerSelectedModel`'s slug: the catalogue entry a stored selection stands for (the
/// Antigravity default stands for the model the account defaults to), else what was stored.
pub fn resolved_slug(driver: &str, model: &str, options: &[ModelOption]) -> String {
    if driver == "antigravity" && model == ANTIGRAVITY_DEFAULT_MODEL {
        let available = options.iter().filter(|option| option.slug != ANTIGRAVITY_DEFAULT_MODEL && !option.is_unavailable);
        let mut available: Vec<_> = available.collect();
        let found = available.iter().position(|option| option.aliases.iter().any(|alias| alias == ANTIGRAVITY_DEFAULT_MODEL)).or_else(|| available.iter().position(|option| option.is_default));
        return found.map(|at| available.remove(at).slug.clone()).unwrap_or_else(|| model.to_owned());
    }
    options.iter().find(|option| option.slug == model).map_or_else(|| model.to_owned(), |option| option.slug.clone())
}

fn row_of<'a>(rows: &'a [InstanceModelOption], selection: &ModelSelection) -> Option<&'a InstanceModelOption> {
    let driver = rows.iter().find(|row| row.instance_id == selection.instance_id)?.driver.to_string();
    let options: Vec<ModelOption> = rows.iter().filter(|row| row.instance_id == selection.instance_id).map(|row| row.model.clone()).collect();
    let slug = resolved_slug(&driver, selection.model.as_str(), &options);
    rows.iter().find(|row| row.instance_id == selection.instance_id && row.model.slug == slug)
}

/// The key a selection has in the picker's list: its instance and the resolved slug.
pub fn selection_key(rows: &[InstanceModelOption], selection: &ModelSelection) -> (String, String) {
    let slug = row_of(rows, selection).map_or_else(|| selection.model.to_string(), |row| row.model.slug.clone());
    (selection.instance_id.to_string(), slug)
}

/// What a toggle in the picker leaves.
#[derive(Debug, Clone, PartialEq)]
pub enum Toggled {
    /// Two or more models: the form starts one thread for each.
    Several(Vec<ModelSelection>),
    /// One model left (or none): back to the ordinary single choice.
    Single(ModelSelection),
}

/// `onToggleModel`: adds the model to what is chosen (the single choice, when nothing else is), or removes it; one model
/// left is a single choice again, and the last one cannot be removed.
pub fn toggle(rows: &[InstanceModelOption], several: Option<&[ModelSelection]>, primary: &ModelSelection, instance: &ProviderInstanceId, model: &str) -> Toggled {
    let current: Vec<ModelSelection> = several.map(<[_]>::to_vec).unwrap_or_else(|| vec![primary.clone()]);
    let matches = |selection: &ModelSelection| &selection.instance_id == instance && selection_key(rows, selection).1 == model;
    let next: Vec<ModelSelection> = if current.iter().any(matches) {
        current.iter().filter(|selection| !matches(selection)).cloned().collect()
    } else {
        let mut next = current.clone();
        if let Ok(model) = TrimmedNonEmptyString::new(model) {
            next.push(ModelSelection { instance_id: instance.clone(), model, options: None });
        }
        next
    };
    match next.as_slice() {
        [_, _, ..] => Toggled::Several(next),
        [only] => Toggled::Single(only.clone()),
        [] => Toggled::Single(primary.clone()),
    }
}

/// The options chosen in the form ride on the model the form shows as chosen (`getSendContext`).
pub fn with_primary(several: &[ModelSelection], primary: &ModelSelection) -> Vec<ModelSelection> {
    several.iter().map(|selection| if selection.instance_id == primary.instance_id && selection.model == primary.model { primary.clone() } else { selection.clone() }).collect()
}

/// `getTriggerDisplayModelName`: the short name, without the sub-provider it starts with.
pub fn trigger_name(model: &ModelOption) -> String {
    let name = model.short_name.as_deref().filter(|short| !short.is_empty()).unwrap_or(&model.name);
    strip_leading_qualifier(name, model.sub_provider.as_deref())
}

/// `stripLeadingQualifier`: "Anthropic: Claude" is "Claude" for the sub-provider "Anthropic" (the match ignores case; a separator
/// of `.`, `:`, `/`, `-` or spaces must follow); a name that is only the qualifier stays whole.
fn strip_leading_qualifier(value: &str, qualifier: Option<&str>) -> String {
    let Some(qualifier) = qualifier.map(str::trim).filter(|qualifier| !qualifier.is_empty()) else { return value.to_owned() };
    let (lower_value, lower_qualifier) = (value.to_lowercase(), qualifier.to_lowercase());
    // Lower-casing can change a length; only a prefix that holds in both strings is stripped.
    let Some(rest) = lower_value.strip_prefix(&lower_qualifier).filter(|_| lower_value.len() == value.len()) else { return value.to_owned() };
    let rest_start = value.len() - rest.len();
    let tail = &value[rest_start..];
    let spaces = tail.len() - tail.trim_start().len();
    let after_spaces = &tail[spaces..];
    let stripped = match after_spaces.chars().next() {
        Some('.' | ':' | '/' | '-') => after_spaces[1..].trim_start(),
        _ if spaces > 0 => after_spaces,
        _ => return value.to_owned(),
    };
    let stripped = stripped.trim();
    if stripped.is_empty() { value.to_owned() } else { stripped.to_owned() }
}

fn label_of(rows: &[InstanceModelOption], selection: &ModelSelection) -> String {
    match row_of(rows, selection) {
        Some(row) => format!("{}{}", trigger_name(&row.model), if row.model.is_unavailable { " (Unavailable)" } else { "" }),
        None => selection.model.to_string(),
    }
}

/// The picker button's text while several models are chosen: the first two by name, then how many more.
pub fn trigger_label(rows: &[InstanceModelOption], selections: &[ModelSelection]) -> String {
    if selections.is_empty() {
        return "Choose models".to_owned();
    }
    let names: Vec<String> = selections.iter().take(2).map(|selection| label_of(rows, selection)).collect();
    let more = if selections.len() > 2 { format!(", {} more", selections.len() - 2) } else { String::new() };
    format!("{}{}", names.join(", "), more)
}

/// The button's tooltip and accessible name: every model chosen.
pub fn all_names(rows: &[InstanceModelOption], selections: &[ModelSelection]) -> String {
    let names = selections.iter().map(|selection| label_of(rows, selection)).collect::<Vec<_>>().join(", ");
    if names.is_empty() { "Choose models".to_owned() } else { names }
}

/// Why the send is refused before anything is sent, in the original's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The server cannot prepare a worktree for each thread: an error on the form.
    UpdateServer,
    /// Not a new thread of a Git project with a base branch, or no model chosen: a warning.
    ChooseModelsAndBranch,
}

/// `onSend`'s two checks, which come before any upload.
pub fn refusal(supported: bool, git_repo: bool, base_branch: &str, models: usize) -> Option<Refusal> {
    if !supported {
        Some(Refusal::UpdateServer)
    } else if !git_repo || base_branch.trim().is_empty() || models == 0 {
        Some(Refusal::ChooseModelsAndBranch)
    } else {
        None
    }
}

/// `getAntigravitySendBlockReason`: why a message cannot go to this Antigravity instance and model.
pub fn antigravity_block_reason(provider: &ServerProvider, model: &str) -> Option<String> {
    if provider.driver.as_str() != "antigravity" {
        return None;
    }
    if !provider.installed {
        return Some("Install Antigravity in provider settings before sending.".into());
    }
    if provider.auth.status == ServerProviderAuthStatus::Unauthenticated {
        return Some("Sign in to Antigravity in provider settings before sending.".into());
    }
    let slug = model.trim();
    if slug.is_empty() {
        return Some("Choose an Antigravity model before sending.".into());
    }
    // A restart clears the account status and catalog; session startup checks them.
    if provider.auth.status == ServerProviderAuthStatus::Unknown {
        return None;
    }
    if provider.models.is_empty() {
        return Some("Refresh Antigravity models in provider settings before sending.".into());
    }
    let listed = provider.models.iter().any(|entry| entry.slug.as_str() == slug || entry.aliases.as_ref().and_then(Option::as_ref).is_some_and(|aliases| aliases.iter().any(|alias| alias.as_str() == slug)));
    if provider.status == ServerProviderState::Ready && slug != ANTIGRAVITY_DEFAULT_MODEL && !listed {
        return Some("That Antigravity model is no longer available. Choose another model.".into());
    }
    None
}

/// The per-model check of `onSend`: the instance is enabled, available and ready, and Antigravity can take the model.
pub fn provider_problem(config: &ServerConfig, selection: &ModelSelection) -> Option<String> {
    let unavailable = || Some(format!("Provider for {} is unavailable.", selection.model));
    let Some(provider) = config.providers.0.iter().find(|provider| provider.instance_id == selection.instance_id) else { return unavailable() };
    let enabled = crate::new_thread::instance_enabled(&config.settings, provider);
    let available = provider.availability.flatten() != Some(ServerProviderAvailability::Unavailable);
    if !enabled || !available || provider.status != ServerProviderState::Ready {
        return unavailable();
    }
    antigravity_block_reason(provider, selection.model.as_str())
}

/// `retryKey`: which model of which draft a request was for, to remember the thread it may have started.
pub fn retry_key(environment: &str, draft: &str, selection: &ModelSelection) -> String {
    serde_json::to_string(&(environment, draft, selection.instance_id.as_str(), selection.model.as_str())).expect("a retry key serializes")
}

/// Whether a failed request may still have started its thread. A refusal the server answered did not; a connection that went
/// before the answer came leaves it unknown (the original's `wasBootstrapThreadDeleted`/`NotCreated` mark the first kind).
pub fn request_may_have_started(error: &str) -> bool {
    matches!(error, "Connection closed before the server replied." | "Request was interrupted.")
}

/// The success toast.
pub fn started_title(count: usize) -> String {
    format!("Started {count} {} in background", if count == 1 { "thread" } else { "threads" })
}

/// The failure toast's title.
pub fn failure_title(model: &str) -> String {
    format!("Could not start {model}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn selection(instance: &str, model: &str) -> ModelSelection {
        serde_json::from_value(json!({"instanceId":instance,"model":model})).unwrap()
    }
    fn row(instance: &str, driver: &str, slug: &str, name: &str) -> InstanceModelOption {
        InstanceModelOption {
            instance_id: ProviderInstanceId::new(instance).unwrap(),
            driver: serde_json::from_value(json!(driver)).unwrap(),
            provider_label: instance.into(),
            model: ModelOption { slug: slug.into(), name: name.into(), aliases: vec![], short_name: None, sub_provider: None, badge: None, is_custom: false, is_default: false, is_legacy: false, is_unavailable: false },
        }
    }
    fn rows() -> Vec<InstanceModelOption> {
        vec![row("codex", "codex", "a", "Model A"), row("codex", "codex", "b", "Model B"), row("claude", "claudeAgent", "c", "Model C")]
    }

    #[test]
    fn a_second_model_makes_several_and_the_last_one_left_is_a_single_choice_again() {
        let rows = rows();
        let (instance, other) = (ProviderInstanceId::new("codex").unwrap(), ProviderInstanceId::new("claude").unwrap());
        let primary = selection("codex", "a");
        let Toggled::Several(two) = toggle(&rows, None, &primary, &instance, "b") else { panic!("two models are several") };
        assert_eq!(two, vec![selection("codex", "a"), selection("codex", "b")]);
        let Toggled::Several(three) = toggle(&rows, Some(&two), &primary, &other, "c") else { panic!("three models are several") };
        assert_eq!(three.len(), 3);
        // Removing one of three leaves two; removing one of two leaves a single choice, the other one.
        let Toggled::Several(back_to_two) = toggle(&rows, Some(&three), &primary, &other, "c") else { panic!("two are several") };
        assert_eq!(back_to_two, two);
        assert_eq!(toggle(&rows, Some(&two), &primary, &instance, "a"), Toggled::Single(selection("codex", "b")));
        // The single choice toggled off leaves nothing, so it stays.
        assert_eq!(toggle(&rows, None, &primary, &instance, "a"), Toggled::Single(primary.clone()));
        // A stored name that the catalogue resolves to the same slug is the same model.
        assert_eq!(toggle(&rows, Some(&[selection("codex", "a"), selection("codex", "b")]), &primary, &instance, "b"), Toggled::Single(selection("codex", "a")));
    }

    #[test]
    fn the_button_names_two_models_then_counts_the_rest() {
        let rows = rows();
        assert_eq!(trigger_label(&rows, &[]), "Choose models");
        assert_eq!(trigger_label(&rows, &[selection("codex", "a"), selection("codex", "b")]), "Model A, Model B");
        assert_eq!(trigger_label(&rows, &[selection("codex", "a"), selection("codex", "b"), selection("claude", "c")]), "Model A, Model B, 1 more");
        assert_eq!(all_names(&rows, &[selection("codex", "a"), selection("claude", "c")]), "Model A, Model C");
        // A model the catalogue does not list shows as stored.
        assert_eq!(trigger_label(&rows, &[selection("codex", "gone"), selection("codex", "a")]), "gone, Model A");
        let mut unavailable = rows.clone();
        unavailable[0].model.is_unavailable = true;
        assert_eq!(trigger_label(&unavailable, &[selection("codex", "a")]), "Model A (Unavailable)");
    }

    #[test]
    fn the_sub_provider_a_name_starts_with_is_left_off() {
        let model = |name: &str, short: Option<&str>, sub: Option<&str>| ModelOption { name: name.into(), short_name: short.map(Into::into), sub_provider: sub.map(Into::into), ..rows()[0].model.clone() };
        assert_eq!(trigger_name(&model("Anthropic: Claude", None, Some("Anthropic"))), "Claude");
        assert_eq!(trigger_name(&model("anthropic Claude", None, Some("Anthropic"))), "Claude");
        assert_eq!(trigger_name(&model("Anthropic - Claude", None, Some("Anthropic"))), "Claude");
        assert_eq!(trigger_name(&model("Anthropic", None, Some("Anthropic"))), "Anthropic", "a name that is only the qualifier stays");
        assert_eq!(trigger_name(&model("AnthropicClaude", None, Some("Anthropic"))), "AnthropicClaude", "no separator, no strip");
        assert_eq!(trigger_name(&model("Long name", Some("Short"), None)), "Short", "the short name is preferred");
    }

    #[test]
    fn the_options_of_the_form_ride_on_the_model_it_shows() {
        let mut primary = selection("codex", "a");
        primary.options = serde_json::from_value(json!([{"id":"effort","value":"high"}])).unwrap();
        let merged = with_primary(&[selection("codex", "a"), selection("codex", "b")], &primary);
        assert_eq!(merged[0], primary);
        assert_eq!(merged[1].options, None);
    }

    #[test]
    fn the_checks_before_a_send_come_in_the_originals_order() {
        assert_eq!(refusal(false, false, "", 0), Some(Refusal::UpdateServer));
        assert_eq!(refusal(true, false, "main", 2), Some(Refusal::ChooseModelsAndBranch), "not a Git project");
        assert_eq!(refusal(true, true, "  ", 2), Some(Refusal::ChooseModelsAndBranch), "no base branch");
        assert_eq!(refusal(true, true, "main", 0), Some(Refusal::ChooseModelsAndBranch));
        assert_eq!(refusal(true, true, "main", 2), None);
    }

    #[test]
    fn only_a_lost_answer_leaves_a_request_that_may_have_started() {
        assert!(request_may_have_started("Connection closed before the server replied."));
        assert!(request_may_have_started("Request was interrupted."));
        assert!(!request_may_have_started("Project not found."));
        assert_eq!(started_title(1), "Started 1 thread in background");
        assert_eq!(started_title(3), "Started 3 threads in background");
        assert_eq!(failure_title("gpt"), "Could not start gpt");
        let key = retry_key("env", "draft", &selection("codex", "a"));
        assert_ne!(key, retry_key("env", "draft", &selection("codex", "b")));
        assert_ne!(key, retry_key("env", "other", &selection("codex", "a")));
    }
}
