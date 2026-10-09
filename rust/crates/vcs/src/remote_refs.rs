//! The helpers of `packages/shared/src/git.ts` that the driver relies on: remote URL
//! normalization (to recognise one remote spelled two ways) and hiding remote refs that a
//! local branch already covers. `git/remoteRefs.ts`' parsers live in `core.rs`.
use std::sync::OnceLock;

use regex::Regex;

use crate::wire::VcsRef;

/// The web spelling of an Azure DevOps repository reached over SSH, or `None` for anything
/// else, so a project cloned either way compares equal to a pull request URL.
fn azure_dev_ops_repository_key(host: &str, segments: &[&str]) -> Option<String> {
    if host != "ssh.dev.azure.com" && host != "vs-ssh.visualstudio.com" {
        return None;
    }
    let [marker, organization, project, repository] = segments else {
        return None;
    };
    if *marker != "v3" || organization.is_empty() || project.is_empty() || repository.is_empty() {
        return None;
    }
    Some(if host == "ssh.dev.azure.com" {
        format!("dev.azure.com/{organization}/{project}/_git/{repository}")
    } else {
        format!("{organization}.visualstudio.com/{project}/_git/{repository}")
    })
}

/// A stable comparison key for a git remote URL.
pub fn normalize_git_remote_url(value: &str) -> String {
    static SCHEME: OnceLock<Regex> = OnceLock::new();
    static SCP: OnceLock<Regex> = OnceLock::new();
    let mut normalized = value.trim().trim_end_matches('/').to_string();
    if normalized
        .get(normalized.len().saturating_sub(4)..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(".git"))
    {
        normalized.truncate(normalized.len() - 4);
    }
    let normalized = normalized.to_lowercase();

    if SCHEME
        .get_or_init(|| Regex::new(r"(?i)^(?:ssh|https?|git)://").unwrap())
        .is_match(&normalized)
    {
        match url::Url::parse(&normalized) {
            Ok(url) => {
                let segments: Vec<&str> = url
                    .path()
                    .split('/')
                    .filter(|segment| !segment.is_empty())
                    .collect();
                let host = url.host_str().unwrap_or("");
                if !host.is_empty() && segments.len() > 1 {
                    return azure_dev_ops_repository_key(host, &segments)
                        .unwrap_or_else(|| format!("{host}/{}", segments.join("/")));
                }
            }
            Err(_) => return normalized,
        }
    }

    let scp = SCP.get_or_init(|| {
        Regex::new(r"(?i)^[a-zA-Z0-9._-]+@([^:/\s]+):([^/\s]+(?:/[^/\s]+)+)$").unwrap()
    });
    if let Some(captures) = scp.captures(&normalized) {
        let (host, path) = (&captures[1], &captures[2]);
        let segments: Vec<&str> = path.split('/').collect();
        return azure_dev_ops_repository_key(host, &segments)
            .unwrap_or_else(|| format!("{host}/{path}"));
    }
    normalized
}

/// `origin/feature/demo` -> `feature/demo`; the name unchanged when it has no remote part.
pub fn derive_local_branch_name_from_remote_ref(branch_name: &str) -> String {
    match branch_name.find('/') {
        Some(separator) if separator > 0 && separator != branch_name.len() - 1 => {
            branch_name[separator + 1..].to_string()
        }
        _ => branch_name.to_string(),
    }
}

fn local_branch_name_candidates(branch_name: &str, remote_name: Option<&str>) -> Vec<String> {
    let mut candidates = Vec::new();
    let first = derive_local_branch_name_from_remote_ref(branch_name);
    if !first.is_empty() {
        candidates.push(first);
    }
    if let Some(remote) = remote_name.filter(|remote| !remote.is_empty()) {
        let prefix = format!("{remote}/");
        if branch_name.len() > prefix.len()
            && let Some(rest) = branch_name.strip_prefix(&prefix)
            && !candidates.iter().any(|candidate| candidate == rest)
        {
            candidates.push(rest.to_string());
        }
    }
    candidates
}

/// Hides `origin/*` remote refs when a local branch of the same name exists.
pub fn dedupe_remote_branches_with_local_matches(refs: &[VcsRef]) -> Vec<VcsRef> {
    let local_names: std::collections::HashSet<&str> = refs
        .iter()
        .filter(|reference| !reference.is_remote)
        .map(|reference| reference.name.as_str())
        .collect();
    refs.iter()
        .filter(|reference| {
            if !reference.is_remote || reference.remote_name.as_deref() != Some("origin") {
                return true;
            }
            !local_branch_name_candidates(&reference.name, reference.remote_name.as_deref())
                .iter()
                .any(|candidate| local_names.contains(candidate.as_str()))
        })
        .cloned()
        .collect()
}

/// A remote name safe to hand to `git remote add`, `fork` when nothing usable remains.
pub fn sanitize_remote_name(value: &str) -> String {
    static RUNS: OnceLock<Regex> = OnceLock::new();
    let runs = RUNS.get_or_init(|| Regex::new(r"[^A-Za-z0-9._-]+").unwrap());
    let sanitized = runs.replace_all(value.trim(), "-");
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "fork".into()
    } else {
        sanitized.into()
    }
}
