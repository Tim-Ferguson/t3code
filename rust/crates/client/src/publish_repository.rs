//! The rules of "Publish repository": which hosts the dialog offers, which are ready, what the
//! repository field starts with and when it may be sent. Port of the `PUBLISH_PROVIDER_OPTIONS`,
//! `getPublishProviderReadiness` and `PublishRepositoryDialog` logic of `GitActionsControl.tsx`.
use serde_json::{Value, json};
use t3_contracts::{SourceControlDiscoveryStatus, SourceControlProviderAuthStatus, SourceControlProviderDiscoveryItem, SourceControlProviderKind};

pub const PUBLISH: &str = "sourceControl.publishRepository";
pub const STEPS: [&str; 3] = ["Provider", "Repository", "Summary"];

/// A host the dialog can publish to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    pub kind: SourceControlProviderKind,
    pub label: &'static str,
    pub host: &'static str,
    pub path_placeholder: &'static str,
}

/// In the original's order, which is also the tie-break between hosts that are equally ready.
pub const OPTIONS: [Host; 5] = [
    Host { kind: SourceControlProviderKind::Forgejo, label: "Forgejo / Gitea", host: "your server", path_placeholder: "owner/repo" },
    Host { kind: SourceControlProviderKind::Github, label: "GitHub", host: "github.com", path_placeholder: "owner/repo" },
    Host { kind: SourceControlProviderKind::Gitlab, label: "GitLab", host: "gitlab.com", path_placeholder: "group/project" },
    Host { kind: SourceControlProviderKind::Bitbucket, label: "Bitbucket", host: "bitbucket.org", path_placeholder: "workspace/repository" },
    Host { kind: SourceControlProviderKind::AzureDevops, label: "Azure DevOps", host: "dev.azure.com", path_placeholder: "project/repository" },
];

pub fn host_of(kind: SourceControlProviderKind) -> Host {
    OPTIONS.iter().copied().find(|option| option.kind == kind).unwrap_or(OPTIONS[0])
}

/// Whether the host can be published to now, and when it cannot, what to tell the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    pub ready: bool,
    pub hint: Option<String>,
}

pub fn readiness(kind: SourceControlProviderKind, discovered: &[SourceControlProviderDiscoveryItem]) -> Readiness {
    let not_ready = |hint: String| Readiness { ready: false, hint: Some(hint) };
    let Some(item) = discovered.iter().find(|item| item.kind == kind) else {
        return not_ready("Provider status unavailable. Open Settings -> Source Control and rescan.".into());
    };
    if item.status != SourceControlDiscoveryStatus::Available {
        return not_ready(item.install_hint.clone());
    }
    if item.auth.status == SourceControlProviderAuthStatus::Unauthenticated {
        return not_ready(item.auth.detail.clone().unwrap_or_else(|| format!("{} is not authenticated. Open Settings -> Source Control for setup guidance.", item.label)));
    }
    Readiness { ready: true, hint: None }
}

/// The hosts, ready ones first, each group by label.
pub fn sorted(discovered: &[SourceControlProviderDiscoveryItem]) -> Vec<Host> {
    let mut options = OPTIONS.to_vec();
    options.sort_by(|left, right| {
        let (left_ready, right_ready) = (readiness(left.kind, discovered).ready, readiness(right.kind, discovered).ready);
        right_ready.cmp(&left_ready).then_with(|| left.label.cmp(right.label))
    });
    options
}

/// The host the dialog is on: the chosen one while it is ready, else the first ready one, else the
/// chosen one, else GitHub.
pub fn current(selected: Option<SourceControlProviderKind>, discovered: &[SourceControlProviderDiscoveryItem]) -> SourceControlProviderKind {
    if let Some(selected) = selected.filter(|selected| readiness(*selected, discovered).ready) {
        return selected;
    }
    sorted(discovered).into_iter().find(|option| readiness(option.kind, discovered).ready).map(|option| option.kind).or(selected).unwrap_or(SourceControlProviderKind::Github)
}

/// What the repository field holds before it is edited: the signed-in account and a slash.
pub fn prefill(kind: SourceControlProviderKind, discovered: &[SourceControlProviderDiscoveryItem]) -> String {
    discovered.iter().find(|item| item.kind == kind).and_then(|item| item.auth.account.clone()).map(|account| format!("{account}/")).unwrap_or_default()
}

/// The host shown before the field. A Forgejo server is whichever one is signed in.
pub fn host(kind: SourceControlProviderKind, discovered: &[SourceControlProviderDiscoveryItem]) -> String {
    let fallback = host_of(kind).host.to_owned();
    if kind != SourceControlProviderKind::Forgejo {
        return fallback;
    }
    discovered.iter().find(|item| item.kind == kind).and_then(|item| item.auth.host.clone()).unwrap_or(fallback)
}

/// An owner and a name, both not empty, are what can be sent.
pub fn repository_complete(repository: &str) -> bool {
    let mut parts = repository.trim().split('/');
    let owner = parts.next().unwrap_or("").trim();
    let name = parts.collect::<Vec<_>>().join("/");
    !owner.is_empty() && !name.trim().is_empty()
}

pub fn request(cwd: &str, kind: SourceControlProviderKind, repository: &str, private: bool, remote: &str, https: bool) -> Value {
    json!({
        "cwd": cwd,
        "provider": kind.as_str(),
        "repository": repository.trim(),
        "visibility": if private { "private" } else { "public" },
        "remoteName": if remote.trim().is_empty() { "origin" } else { remote.trim() },
        "protocol": if https { "https" } else { "ssh" },
    })
}

/// The summary step's wording for a finished publish.
pub fn outcome(result: &Value, provider_label: &str) -> (&'static str, String) {
    if result["status"] == "pushed" {
        ("Repository published", format!("{} is now live on {provider_label}.", result["branch"].as_str().unwrap_or("The branch")))
    } else {
        ("Repository created", format!("Remote \"{}\" is set up. Make a commit and push it to share your code.", result["remoteName"].as_str().unwrap_or("origin")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: &str, status: &str, auth: &str, account: Option<&str>) -> SourceControlProviderDiscoveryItem {
        let some = |value: Option<&str>| match value {
            Some(value) => json!({"_tag":"Some","value":value}),
            None => json!({"_tag":"None"}),
        };
        serde_json::from_value(json!({
            "kind": kind, "label": kind, "status": status, "version": some(None), "installHint": format!("install {kind}"), "detail": some(None),
            "auth": {"status": auth, "account": some(account), "host": some(None), "detail": some(None)}
        }))
        .unwrap()
    }

    #[test]
    fn readiness_explains_what_is_missing() {
        let found = [item("github", "available", "authenticated", Some("carol")), item("gitlab", "missing", "unknown", None), item("bitbucket", "available", "unauthenticated", None)];
        assert!(readiness(SourceControlProviderKind::Github, &found).ready);
        assert_eq!(readiness(SourceControlProviderKind::Gitlab, &found).hint.as_deref(), Some("install gitlab"));
        assert_eq!(readiness(SourceControlProviderKind::Bitbucket, &found).hint.as_deref(), Some("bitbucket is not authenticated. Open Settings -> Source Control for setup guidance."));
        assert_eq!(readiness(SourceControlProviderKind::Forgejo, &found).hint.as_deref(), Some("Provider status unavailable. Open Settings -> Source Control and rescan."));
    }

    #[test]
    fn the_dialog_starts_on_the_first_ready_host_and_prefills_the_account() {
        let found = [item("github", "available", "authenticated", Some("carol")), item("gitlab", "available", "authenticated", Some("dana"))];
        assert_eq!(current(None, &found), SourceControlProviderKind::Github);
        assert_eq!(current(Some(SourceControlProviderKind::Gitlab), &found), SourceControlProviderKind::Gitlab);
        assert_eq!(current(Some(SourceControlProviderKind::Bitbucket), &found), SourceControlProviderKind::Github, "a choice that is not ready gives way");
        assert_eq!(prefill(SourceControlProviderKind::Gitlab, &found), "dana/");
        assert_eq!(sorted(&found).iter().map(|option| option.label).collect::<Vec<_>>(), ["GitHub", "GitLab", "Azure DevOps", "Bitbucket", "Forgejo / Gitea"]);
        assert_eq!(current(None, &[]), SourceControlProviderKind::Github);
    }

    #[test]
    fn a_repository_needs_an_owner_and_a_name() {
        for (text, complete) in [("carol/", false), ("/repo", false), ("carol", false), ("carol/repo", true), (" carol / group/repo ", true)] {
            assert_eq!(repository_complete(text), complete, "{text:?}");
        }
        assert_eq!(request("/w", SourceControlProviderKind::Github, " carol/repo ", true, " ", false)["remoteName"], "origin");
    }
}
