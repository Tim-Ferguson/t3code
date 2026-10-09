//! What a user types to name a pull request: a URL, a CLI checkout command, or a number. Port of
//! `parsePullRequestReference` from `pullRequestReference.ts`.
use regex::Regex;
use std::sync::LazyLock;

struct Patterns {
    forgejo_url: Regex,
    forgejo_cli: Regex,
    github_url: Regex,
    gitlab_url: Regex,
    azure_url: Regex,
    number: Regex,
    github_cli: Regex,
    gitlab_cli: Regex,
    azure_cli: Regex,
}
static PATTERNS: LazyLock<Patterns> = LazyLock::new(|| {
    let re = |pattern: &str| Regex::new(pattern).expect("source pattern");
    Patterns {
        forgejo_url: re(r"(?i)^https?://[^/\s]+/(?:[^/\s]+/)+[^/\s]+/pulls/(\d+)(?:[/?#].*)?$"),
        forgejo_cli: re(r"(?i)^tea\s+(?:pr|pulls)\s+checkout\s+(.+)$"),
        github_url: re(r"(?i)^https://github\.com/[^/\s]+/[^/\s]+/pull/(\d+)(?:[/?#].*)?$"),
        gitlab_url: re(r"(?i)^https://[^/\s]*gitlab[^/\s]*/.+/-/merge_requests/(\d+)(?:[/?#].*)?$"),
        azure_url: re(r"(?i)^https://(?:dev\.azure\.com/[^/\s]+/[^/\s]+|[^/\s]+\.visualstudio\.com/[^/\s]+)/_git/[^/\s]+/pullrequest/(\d+)(?:[/?#].*)?$"),
        number: re(r"^#?(\d+)$"),
        github_cli: re(r"(?i)^gh\s+pr\s+checkout\s+(.+)$"),
        gitlab_cli: re(r"(?i)^glab\s+mr\s+checkout\s+(.+)$"),
        azure_cli: re(r"(?i)^az\s+repos\s+pr\s+checkout\s+(.+)$"),
    }
});

fn azure_checkout_reference(args: &str) -> Option<String> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    for (index, part) in parts.iter().enumerate() {
        if *part == "--id" || *part == "-i" {
            return parts.get(index + 1).map(|part| (*part).to_owned());
        }
        if let Some(rest) = part.strip_prefix("--id=") {
            return (!rest.is_empty()).then(|| rest.to_owned());
        }
    }
    parts.iter().find(|part| !part.starts_with('-')).map(|part| (*part).to_owned())
}

/// The reference to send: a URL as typed, or the bare number; `None` for text that names none.
pub fn parse(input: &str) -> Option<String> {
    let patterns = &*PATTERNS;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let capture = |regex: &Regex| regex.captures(trimmed).and_then(|found| found.get(1)).map(|found| found.as_str().trim().to_owned());
    let normalized = capture(&patterns.forgejo_cli)
        .or_else(|| capture(&patterns.github_cli))
        .or_else(|| capture(&patterns.gitlab_cli))
        .or_else(|| capture(&patterns.azure_cli).and_then(|args| azure_checkout_reference(&args)))
        .unwrap_or_else(|| trimmed.to_owned());
    if normalized.is_empty() {
        return None;
    }
    if [&patterns.forgejo_url, &patterns.github_url, &patterns.gitlab_url, &patterns.azure_url].iter().any(|regex| regex.captures(&normalized).and_then(|found| found.get(1)).is_some_and(|found| !found.as_str().is_empty())) {
        return Some(normalized);
    }
    patterns.number.captures(&normalized).and_then(|found| found.get(1)).map(|found| found.as_str().to_owned())
}

pub const EMPTY: &str = "Paste a pull request URL, checkout command, or enter 123 / #123.";
pub const INVALID: &str = "Use a pull request URL, checkout command, 123, or #123.";
