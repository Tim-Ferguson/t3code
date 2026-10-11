//! What a workspace's git remotes say about its repository: `RepositoryIdentityResolver`
//! (`apps/server/src/project/RepositoryIdentityResolver.ts`). The identity a client groups
//! checkouts by is read from `git remote -v` at the repository's top level, and kept so a minute's
//! worth of shell builds spawns no git: a resolved identity for fifteen minutes, an absent one for
//! one, until a caller asks to `refresh`.
//!
//! Two caches, as the original has: the working directory's repository root, then that root's
//! identity. The identity is refined once, inside the cache, for a self-hosted Forgejo remote that
//! the host name does not give away.
use crate::vcs_cache::TtlCache;
use futures_util::future::BoxFuture;
use regex::Regex;
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::Duration,
};
use t3_contracts::{
    GitRemoteSource, RepositoryIdentity, RepositoryIdentityLocator, RepositoryOrigin, TrimmedNonEmptyString, trim_wire_string,
};
use t3_vcs::{Clock, SystemClock, remote_refs::normalize_git_remote_url};

const CACHE_CAPACITY: usize = 512;
const POSITIVE_TTL: Duration = Duration::from_secs(15 * 60);
const NEGATIVE_TTL: Duration = Duration::from_secs(60);
/// `ProcessRunner`'s default: a probe that has not answered by now is as good as a failed one.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// What a git probe answered: its exit code and standard output.
pub struct ProbeOutput {
    pub code: i32,
    pub stdout: String,
}

/// Runs git with these arguments (after `-C <directory>`), or answers nothing when it could not be
/// run or did not finish. Replaced in tests to count the spawns.
pub type Probe = Arc<dyn Fn(Vec<String>) -> BoxFuture<'static, Option<ProbeOutput>> + Send + Sync>;

/// Adds what only the hosting account can tell (`server.ts` `layerRepositoryIdentityResolver`).
pub type Refine = Arc<dyn Fn(RepositoryIdentity) -> BoxFuture<'static, RepositoryIdentity> + Send + Sync>;

/// Spawns `git` as the original's process runner does: no shell, the output captured, the process
/// ended with the request that dropped it.
pub fn system_probe() -> Probe {
    Arc::new(|args| {
        Box::pin(async move {
            let mut command = tokio::process::Command::new("git");
            command.args(&args).stdin(std::process::Stdio::null()).kill_on_drop(true);
            let output = tokio::time::timeout(PROBE_TIMEOUT, command.output()).await.ok()?.ok()?;
            Some(ProbeOutput { code: output.status.code()?, stdout: String::from_utf8_lossy(&output.stdout).into_owned() })
        })
    })
}

pub struct RepositoryIdentityResolver {
    probe: Probe,
    refine: Option<Refine>,
    roots: TtlCache<String, Option<String>, ()>,
    identities: TtlCache<String, Option<RepositoryIdentity>, ()>,
}

impl RepositoryIdentityResolver {
    pub fn new(probe: Probe, refine: Option<Refine>) -> Self {
        Self::with_clock(probe, refine, Arc::new(SystemClock))
    }

    pub fn with_clock(probe: Probe, refine: Option<Refine>, clock: Arc<dyn Clock>) -> Self {
        Self {
            probe,
            refine,
            roots: TtlCache::new(CACHE_CAPACITY, clock.clone()),
            identities: TtlCache::new(CACHE_CAPACITY, clock),
        }
    }

    /// The identity of the repository `cwd` is in, or none when it is in no repository or its
    /// repository has no remote. `refresh` forgets what is held for the directory and its root first.
    pub async fn resolve(&self, cwd: &str, refresh: bool) -> Option<RepositoryIdentity> {
        let ttl = |held: &Result<Option<String>, ()>| match held {
            Ok(Some(_)) => POSITIVE_TTL,
            _ => NEGATIVE_TTL,
        };
        if refresh {
            self.roots.invalidate(&cwd.to_owned());
        }
        let root = self.roots.get(cwd.to_owned(), async { Ok(self.top_level(cwd).await) }, ttl).await.ok()??;
        if refresh {
            self.identities.invalidate(&root);
        }
        let identity_ttl = |held: &Result<Option<RepositoryIdentity>, ()>| match held {
            Ok(Some(_)) => POSITIVE_TTL,
            _ => NEGATIVE_TTL,
        };
        self.identities.get(root.clone(), async { Ok(self.identity_at(&root).await) }, identity_ttl).await.ok()?
    }

    async fn top_level(&self, cwd: &str) -> Option<String> {
        let output = (self.probe)(vec!["-C".into(), cwd.into(), "rev-parse".into(), "--show-toplevel".into()]).await?;
        top_level_of(&output)
    }

    async fn identity_at(&self, root: &str) -> Option<RepositoryIdentity> {
        let output = (self.probe)(vec!["-C".into(), root.into(), "remote".into(), "-v".into()]).await?;
        let identity = identity_from_remotes(&output, root)?;
        Some(match &self.refine {
            Some(refine) => refine(identity).await,
            None => identity,
        })
    }
}

/// The repository root a `git rev-parse --show-toplevel` answered, if it answered one.
pub fn top_level_of(output: &ProbeOutput) -> Option<String> {
    let candidate = trim_wire_string(&output.stdout);
    (output.code == 0 && !candidate.is_empty()).then(|| candidate.to_owned())
}

/// `git remote -v` at `root`, as an identity: the primary remote's repository, and the `origin` it
/// is a fork of when that is another repository.
pub fn identity_from_remotes(output: &ProbeOutput, root: &str) -> Option<RepositoryIdentity> {
    if output.code != 0 {
        return None;
    }
    let remotes = parse_remote_fetch_urls(&output.stdout);
    let (name, url) = pick_primary_remote(&remotes)?;
    build_identity(name, url, remotes.get("origin").map(String::as_str), root)
}

/// `parseRemoteFetchUrls`: the fetch URL of each remote; a name given twice keeps its last.
fn parse_remote_fetch_urls(stdout: &str) -> HashMap<String, String> {
    static LINE: OnceLock<Regex> = OnceLock::new();
    let line = LINE.get_or_init(|| Regex::new(r"^(\S+)\s+(\S+)\s+\((fetch|push)\)$").unwrap());
    let mut remotes = HashMap::new();
    for text in stdout.split('\n') {
        let trimmed = trim_wire_string(text);
        if trimmed.is_empty() {
            continue;
        }
        let Some(captures) = line.captures(trimmed) else { continue };
        if &captures[3] != "fetch" || captures[1].is_empty() || captures[2].is_empty() {
            continue;
        }
        remotes.insert(captures[1].to_owned(), captures[2].to_owned());
    }
    remotes
}

/// `pickPrimaryRemote`: `upstream`, then `origin`, then the first by name in the process locale.
fn pick_primary_remote(remotes: &HashMap<String, String>) -> Option<(&str, &str)> {
    for preferred in ["upstream", "origin"] {
        if let Some(url) = remotes.get(preferred).filter(|url| !url.is_empty()) {
            return Some((preferred, url));
        }
    }
    let collator = collator();
    // JavaScript's sort is stable and a Map holds a name once, so ties only arise between names the
    // collator calls equal; the first inserted wins there, which a name order cannot reproduce, so
    // equal names fall back to code point order.
    remotes
        .iter()
        .min_by(|left, right| collator.compare(left.0, right.0).then_with(|| left.0.cmp(right.0)))
        .map(|(name, url)| (name.as_str(), url.as_str()))
}

fn collator() -> icu_collator::CollatorBorrowed<'static> {
    let preferences: icu_collator::CollatorPreferences = crate::workspace_entries::locale_from_environment().into();
    icu_collator::Collator::try_new(preferences, Default::default()).expect("compiled ICU collation data")
}

/// `repositoryPathOf`: what follows the host in a canonical key.
fn repository_path_of(canonical_key: &str) -> String {
    canonical_key.split('/').skip(1).collect::<Vec<_>>().join("/")
}

fn text(value: &str) -> Option<TrimmedNonEmptyString> {
    TrimmedNonEmptyString::new(value).ok()
}

fn build_identity(remote_name: &str, remote_url: &str, origin_url: Option<&str>, root: &str) -> Option<RepositoryIdentity> {
    let canonical_key = normalize_git_remote_url(remote_url);
    let provider = crate::source_control_detect::detect_provider_from_remote_url(remote_url);
    let path = repository_path_of(&canonical_key);
    let segments: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();
    let origin = origin_url.and_then(|origin_url| {
        let origin_key = normalize_git_remote_url(origin_url);
        if origin_key == canonical_key {
            return None;
        }
        let display_name = repository_path_of(&origin_key);
        Some(RepositoryOrigin { canonical_key: text(&origin_key)?, display_name: if display_name.is_empty() { None } else { text(&display_name) } })
    });
    Some(RepositoryIdentity {
        canonical_key: text(&canonical_key)?,
        locator: RepositoryIdentityLocator { source: GitRemoteSource::GitRemote, remote_name: text(remote_name)?, remote_url: text(remote_url)? },
        web_url: None,
        root_path: text(root),
        display_name: if path.is_empty() { None } else { text(&path) },
        provider: provider.and_then(|provider| text(provider.kind.as_str())),
        owner: segments.first().and_then(|owner| text(owner)),
        name: segments.last().and_then(|name| text(name)),
        origin,
    })
}

/// The refinement of `server.ts`: a Forgejo remote that its host name does not show as one is
/// asked of the hosting account, which also knows where its web pages are.
pub fn forgejo_refine(host: crate::source_control_host::Host) -> Refine {
    Arc::new(move |identity| {
        let host = host.clone();
        Box::pin(async move { refine_forgejo(host, identity).await })
    })
}

async fn refine_forgejo(host: crate::source_control_host::Host, identity: RepositoryIdentity) -> RepositoryIdentity {
    use crate::{source_control_provider::ProviderContext, source_control_registry::SourceControlRegistry};
    use t3_contracts::{SourceControlProviderInfo, SourceControlProviderKind};
    let Some(remote) = crate::source_control_forgejo::parse_remote(identity.locator.remote_url.as_str()) else { return identity };
    let Some(root) = identity.root_path.as_ref().map(|root| root.as_str().to_owned()) else { return identity };
    if identity.provider.as_ref().is_some_and(|provider| !matches!(provider.as_str(), "unknown" | "forgejo")) {
        return identity;
    }
    let context = ProviderContext {
        provider: SourceControlProviderInfo { kind: SourceControlProviderKind::Unknown, name: "Unknown".into(), base_url: String::new() },
        remote_name: identity.locator.remote_name.as_str().to_owned(),
        remote_url: identity.locator.remote_url.as_str().to_owned(),
        requested_host: None,
    };
    let Ok(handle) = SourceControlRegistry::new(host).resolve_handle(&root, Some(context)).await else { return identity };
    let Some(provider) = handle.context.map(|context| context.provider).filter(|provider| provider.kind == SourceControlProviderKind::Forgejo) else { return identity };
    let base_url = provider.base_url.trim_end_matches('/').to_owned();
    let base_path = url::Url::parse(&base_url).map(|url| url.path().trim_matches('/').to_owned()).unwrap_or_default();
    let path = if !remote.ssh && !base_path.is_empty() && remote.path.starts_with(&format!("{base_path}/")) { remote.path[base_path.len() + 1..].to_owned() } else { remote.path.clone() };
    RepositoryIdentity { provider: text("forgejo"), web_url: text(&format!("{base_url}/{path}")), ..identity }
}
