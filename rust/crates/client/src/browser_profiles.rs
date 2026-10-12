//! The browser profiles of Settings, Integrations: the person's own profiles beside the two built-in ones, each keeping its cookies and
//! logins apart (`BrowserProfilesSetting` of `IntegrationsSettings.tsx`). The list lives in the client settings (`browserProfiles`); these
//! are the edits the settings page makes to it.
use serde_json::{Value, json};

/// `BROWSER_PROFILE_MAX_COUNT`: how many profiles of their own a person may have.
pub const MAX_COUNT: usize = 24;
/// `BROWSER_PROFILE_NAME_MAX_LENGTH`.
pub const NAME_MAX_LENGTH: usize = 48;
/// The built-in profile every tab opens under unless another is chosen.
pub const DEFAULT_ID: &str = "default";

/// A name for a new profile that no other profile has: `base`, else `base 2`, `base 3`...
pub fn unique_name(base: &str, taken: &[String]) -> String {
    let mut name = base.to_owned();
    let mut index = 2;
    while taken.contains(&name) {
        name = format!("{base} {index}");
        index += 1;
    }
    name
}

/// The profiles after a blank one is added, or none when the person is at the limit.
pub fn added(profiles: &[Value], id: &str, taken_names: &[String]) -> Option<Vec<Value>> {
    if profiles.len() >= MAX_COUNT {
        return None;
    }
    let mut next = profiles.to_vec();
    next.push(json!({"id": id, "name": unique_name("New profile", taken_names), "kind": "persistent"}));
    Some(next)
}

/// The profiles after one is renamed; none when the new name is empty. A name is trimmed and cut at the limit.
pub fn renamed(profiles: &[Value], id: &str, name: &str) -> Option<Vec<Value>> {
    let name: String = name.trim().chars().take(NAME_MAX_LENGTH).collect();
    if name.is_empty() {
        return None;
    }
    Some(
        profiles
            .iter()
            .cloned()
            .map(|mut profile| {
                if profile["id"] == id {
                    profile["name"] = json!(name);
                }
                profile
            })
            .collect(),
    )
}

/// The change to the settings when a profile is removed: the list without it, and the default moved to the built-in one when it
/// was the default (a default pointing at nothing would open tabs under a profile that is not there).
pub fn removal(profiles: &[Value], default_id: &str, id: &str) -> Value {
    let remaining: Vec<Value> = profiles.iter().filter(|profile| profile["id"] != id).cloned().collect();
    let mut change = json!({"browserProfiles": remaining});
    if default_id == id {
        change["browserDefaultProfileId"] = json!(DEFAULT_ID);
    }
    change
}

/// What the "Clear cookies and cache" of a profile says it did.
pub fn cleared_title(name: &str) -> String {
    format!("Cleared {name}\u{2019}s cookies and cache")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, name: &str) -> Value {
        json!({"id": id, "name": name, "kind": "persistent"})
    }

    #[test]
    fn a_new_profile_takes_a_name_nobody_has_and_stops_at_the_limit() {
        assert_eq!(unique_name("New profile", &[]), "New profile");
        assert_eq!(unique_name("New profile", &["New profile".into(), "New profile 2".into()]), "New profile 3");
        let one = added(&[], "profile-1", &["Default".into(), "Incognito".into()]).unwrap();
        assert_eq!(one, [profile("profile-1", "New profile")]);
        let full: Vec<Value> = (0..MAX_COUNT).map(|index| profile(&format!("p{index}"), &format!("Profile {index}"))).collect();
        assert_eq!(added(&full, "one-more", &[]), None);
    }

    #[test]
    fn a_rename_is_trimmed_cut_and_never_empty() {
        let profiles = [profile("a", "One"), profile("b", "Two")];
        assert_eq!(renamed(&profiles, "b", "  Work  ").unwrap()[1]["name"], "Work");
        assert_eq!(renamed(&profiles, "a", &"x".repeat(60)).unwrap()[0]["name"].as_str().unwrap().chars().count(), NAME_MAX_LENGTH);
        assert_eq!(renamed(&profiles, "a", "   "), None);
        assert_eq!(renamed(&profiles, "missing", "Name").unwrap(), profiles, "an unknown profile changes nothing");
    }

    #[test]
    fn removing_the_default_profile_moves_the_default_to_the_built_in_one() {
        let profiles = [profile("a", "One"), profile("b", "Two")];
        assert_eq!(removal(&profiles, "a", "b"), json!({"browserProfiles": [profile("a", "One")]}));
        assert_eq!(removal(&profiles, "a", "a"), json!({"browserProfiles": [profile("b", "Two")], "browserDefaultProfileId": "default"}));
        assert_eq!(cleared_title("Work"), "Cleared Work\u{2019}s cookies and cache");
    }
}
