//! Finding out which host tools are installed and signed in (port of
//! `SourceControlProviderDiscovery.ts` and `SourceControlDiscovery.ts`). A CLI-backed host is
//! probed with `--version` and then an auth command; what each prints is read by the host's own
//! `parse_auth`. Other hosts (GitHub with a saved token, Bitbucket's API) probe themselves.
//!
//! The same probes refine an unrecognised remote: a self-hosted GitLab on an ordinary-looking
//! host name is recognised by `glab` being signed in to it.
use crate::{source_control_host::Host, source_control_provider::ProviderContext};
use futures_util::future::BoxFuture;
use regex::Regex;
use std::{sync::{Arc, OnceLock}, time::Duration};
use t3_contracts::{
    SourceControlDiscoveryResult, SourceControlDiscoveryStatus, SourceControlProviderAuth,
    SourceControlProviderAuthStatus, SourceControlProviderDiscoveryItem, SourceControlProviderInfo,
    SourceControlProviderKind, VcsDiscoveryItem, VcsDriverKindWire,
};
use t3_vcs::VcsError;

/// Most provider CLIs answer `--version` in well under a second, so a short budget keeps
/// discovery snappy. Specs whose CLI is known to be slower raise it.
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_MAX_OUTPUT_BYTES: usize = 8_000;

pub struct AuthProbeInput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

type ParseAuth = Arc<dyn Fn(&AuthProbeInput) -> SourceControlProviderAuth + Send + Sync>;
type RefineUnknownRemote =
    Arc<dyn Fn(&ProviderContext, &AuthProbeInput) -> Option<SourceControlProviderInfo> + Send + Sync>;

pub struct CliSpec {
    pub kind: SourceControlProviderKind,
    pub label: &'static str,
    pub install_hint: &'static str,
    pub executable: &'static str,
    pub version_args: Vec<&'static str>,
    pub auth_args: Vec<&'static str>,
    pub remote_refinement_args: Option<Vec<&'static str>>,
    pub probe_timeout: Option<Duration>,
    pub parse_auth: ParseAuth,
    pub refine_unknown_remote: Option<RefineUnknownRemote>,
}

/// A host that probes itself (GitHub's token sources, Bitbucket's API, Forgejo's two CLIs).
pub trait ManagedProbe: Send + Sync {
    fn kind(&self) -> SourceControlProviderKind;
    fn probe<'a>(&'a self, cwd: &'a str) -> BoxFuture<'a, SourceControlProviderDiscoveryItem>;
    fn refine_unknown_remote<'a>(
        &'a self,
        _cwd: &'a str,
        _context: &'a ProviderContext,
    ) -> BoxFuture<'a, Option<SourceControlProviderInfo>> {
        Box::pin(async { None })
    }
}

/// A host with no executable: its sign-in is read from its API.
pub trait ApiProbe: Send + Sync {
    fn kind(&self) -> SourceControlProviderKind;
    fn label(&self) -> &'static str;
    fn install_hint(&self) -> &'static str;
    fn probe_auth(&self) -> BoxFuture<'_, t3_contracts::SourceControlProviderAuth>;
}

pub enum DiscoverySpec {
    Cli(CliSpec),
    Managed(Arc<dyn ManagedProbe>),
    Api(Arc<dyn ApiProbe>),
}

fn ansi_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(concat!(
            r"[\x1B\x{9B}][\[\]()#;?]*",
            r"(?:(?:(?:(?:;[-a-zA-Z\d/#&.:=?%@~_]+)*|[a-zA-Z\d]+(?:;[-a-zA-Z\d/#&.:=?%@~_]*)*)?\x07)",
            r"|(?:(?:\d{1,4}(?:;\d{0,4})*)?[\dA-PR-TZcf-ntqry=><~]))"
        ))
        .unwrap()
    })
}

/// The first line with something on it, terminal escape sequences removed.
pub fn first_non_empty_line(text: &str) -> Option<String> {
    let stripped = ansi_pattern().replace_all(text, "");
    stripped
        .split('\n')
        .map(|line| t3_contracts::trim_wire_string(line.trim_end_matches('\r')))
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// A VCS process failure's message, as `detailFromCause` reads it off an `Error`.
pub fn detail_from_error(error: &VcsError) -> Option<String> {
    let message = error.to_string();
    let trimmed = message.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn non_blank(value: Option<&str>) -> Option<String> {
    let trimmed = t3_contracts::trim_wire_string(value.unwrap_or(""));
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub fn provider_auth(
    status: SourceControlProviderAuthStatus,
    account: Option<&str>,
    host: Option<&str>,
    detail: Option<&str>,
) -> SourceControlProviderAuth {
    SourceControlProviderAuth {
        status,
        account: non_blank(account),
        host: non_blank(host),
        detail: non_blank(detail),
        accounts: None,
    }
}

pub fn unknown_auth(detail: Option<&str>) -> SourceControlProviderAuth {
    provider_auth(SourceControlProviderAuthStatus::Unknown, None, None, detail)
}

/// Stdout and stderr, whichever have text, joined by a newline.
pub fn combined_auth_output(input: &AuthProbeInput) -> String {
    [&input.stdout, &input.stderr]
        .into_iter()
        .filter(|entry| !entry.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Non-empty lines, minus the ones that would print a token or its scopes.
fn sanitized_auth_lines(text: &str) -> Vec<String> {
    static TOKEN_LINE: OnceLock<Regex> = OnceLock::new();
    let token_line = TOKEN_LINE.get_or_init(|| Regex::new(r"(?i)^[-\s]*token(?:\s+scopes?)?:").unwrap());
    text.split('\n')
        .map(|entry| t3_contracts::trim_wire_string(entry.trim_end_matches('\r')))
        .filter(|line| !line.is_empty() && !token_line.is_match(line))
        .map(str::to_string)
        .collect()
}

pub fn first_safe_auth_line(text: &str) -> Option<String> {
    sanitized_auth_lines(text).into_iter().next()
}

/// The first line that is a host name (with an optional port) once leading decoration is gone.
pub fn parse_cli_host(text: &str) -> Option<String> {
    static LEADING: OnceLock<Regex> = OnceLock::new();
    static HOST: OnceLock<Regex> = OnceLock::new();
    let leading = LEADING.get_or_init(|| Regex::new(r"(?i)^[^a-z0-9]+").unwrap());
    let host = HOST.get_or_init(|| Regex::new(r"(?i)^[a-z0-9][a-z0-9.-]*(?::\d+)?$").unwrap());
    sanitized_auth_lines(text)
        .into_iter()
        .map(|line| leading.replace(&line, "").into_owned())
        .find(|line| host.is_match(line))
}

/// The first capture group of the first pattern that matches with something in it.
pub fn match_first(text: &str, patterns: &[&Regex]) -> Option<String> {
    patterns.iter().find_map(|pattern| {
        let value = pattern.captures(text)?.get(1)?.as_str().trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

fn missing_item(spec: &CliSpec, detail: Option<String>) -> SourceControlProviderDiscoveryItem {
    SourceControlProviderDiscoveryItem {
        kind: spec.kind,
        label: spec.label.into(),
        executable: Some(spec.executable.into()),
        status: SourceControlDiscoveryStatus::Missing,
        version: None,
        install_hint: spec.install_hint.into(),
        detail,
        auth: unknown_auth(Some("Hosting integration command was not found on the server PATH.")),
    }
}

async fn run_probe(
    host: &Host,
    operation: &str,
    spec: &CliSpec,
    args: &[&str],
    cwd: &str,
    allow_non_zero_exit: bool,
) -> Result<t3_vcs::VcsProcessOutput, VcsError> {
    let mut input = host.cli(operation, spec.executable, args.iter().copied(), cwd);
    input.allow_non_zero_exit = allow_non_zero_exit;
    input.timeout = Some(spec.probe_timeout.unwrap_or(DEFAULT_PROBE_TIMEOUT));
    input.max_output_bytes = Some(PROBE_MAX_OUTPUT_BYTES);
    input.append_truncation_marker = true;
    host.process().run(input).await
}

/// `--version`, then the auth command. A CLI that is not there reads as missing; one that
/// cannot be asked about its login reads as `unknown` with the reason.
pub async fn probe_cli(host: &Host, spec: &CliSpec, cwd: &str) -> SourceControlProviderDiscoveryItem {
    let version = match run_probe(host, "source-control.discovery.probe", spec, &spec.version_args, cwd, false).await {
        Ok(result) => first_non_empty_line(&result.stdout).or_else(|| first_non_empty_line(&result.stderr)),
        Err(error) => return missing_item(spec, detail_from_error(&error)),
    };
    let auth = match run_probe(host, "source-control.discovery.auth", spec, &spec.auth_args, cwd, true).await {
        Ok(result) => (spec.parse_auth)(&AuthProbeInput {
            stdout: result.stdout,
            stderr: result.stderr,
            exit_code: result.exit_code,
        }),
        Err(error) => unknown_auth(detail_from_error(&error).as_deref()),
    };
    SourceControlProviderDiscoveryItem {
        kind: spec.kind,
        label: spec.label.into(),
        executable: Some(spec.executable.into()),
        status: SourceControlDiscoveryStatus::Available,
        version,
        install_hint: spec.install_hint.into(),
        detail: None,
        auth,
    }
}

pub async fn probe_source_control_provider(
    host: &Host,
    spec: &DiscoverySpec,
    cwd: &str,
) -> SourceControlProviderDiscoveryItem {
    match spec {
        DiscoverySpec::Cli(spec) => probe_cli(host, spec, cwd).await,
        DiscoverySpec::Managed(probe) => probe.probe(cwd).await,
        DiscoverySpec::Api(probe) => SourceControlProviderDiscoveryItem {
            kind: probe.kind(),
            label: probe.label().into(),
            executable: None,
            status: SourceControlDiscoveryStatus::Available,
            version: None,
            install_hint: probe.install_hint().into(),
            detail: None,
            auth: probe.probe_auth().await,
        },
    }
}

/// Asks every host that can recognise a remote whether it recognises this one. The first yes
/// wins; a host whose CLI fails to answer says no.
pub async fn refine_unknown_remote_provider(
    host: &Host,
    specs: &[DiscoverySpec],
    cwd: &str,
    context: Option<ProviderContext>,
) -> Option<ProviderContext> {
    let context = context?;
    if context.provider.kind != SourceControlProviderKind::Unknown {
        return Some(context);
    }
    for spec in specs {
        let refined = match spec {
            DiscoverySpec::Managed(probe) => probe.refine_unknown_remote(cwd, &context).await,
            DiscoverySpec::Api(_) => None,
            DiscoverySpec::Cli(spec) => {
                let Some(refine) = &spec.refine_unknown_remote else {
                    continue;
                };
                let args = spec.remote_refinement_args.as_ref().unwrap_or(&spec.auth_args);
                match run_probe(host, "source-control.discovery.refine-unknown-remote", spec, args, cwd, true).await {
                    Ok(result) => refine(
                        &context,
                        &AuthProbeInput {
                            stdout: result.stdout,
                            stderr: result.stderr,
                            exit_code: result.exit_code,
                        },
                    ),
                    Err(_) => None,
                }
            }
        };
        if let Some(provider) = refined {
            return Some(ProviderContext { provider, ..context });
        }
    }
    Some(context)
}

struct VcsProbe {
    kind: VcsDriverKindWire,
    label: &'static str,
    executable: &'static str,
    implemented: bool,
    install_hint: &'static str,
}

const VCS_PROBES: [VcsProbe; 2] = [
    VcsProbe {
        kind: VcsDriverKindWire::Git,
        label: "Git",
        executable: "git",
        implemented: true,
        install_hint: "Install Git from https://git-scm.com/downloads or with your package manager.",
    },
    VcsProbe {
        kind: VcsDriverKindWire::Jj,
        label: "Jujutsu",
        executable: "jj",
        implemented: false,
        install_hint: "Install Jujutsu with `brew install jj` or from https://github.com/jj-vcs/jj.",
    },
];

async fn probe_vcs(host: &Host, probe: &VcsProbe) -> VcsDiscoveryItem {
    let mut input = host.cli("source-control.discovery.probe", probe.executable, ["--version"], host.cwd());
    input.timeout = Some(DEFAULT_PROBE_TIMEOUT);
    input.max_output_bytes = Some(PROBE_MAX_OUTPUT_BYTES);
    input.append_truncation_marker = true;
    let (status, version, detail) = match host.process().run(input).await {
        Ok(result) => (
            SourceControlDiscoveryStatus::Available,
            first_non_empty_line(&result.stdout).or_else(|| first_non_empty_line(&result.stderr)),
            None,
        ),
        Err(error) => (SourceControlDiscoveryStatus::Missing, None, detail_from_error(&error)),
    };
    VcsDiscoveryItem {
        kind: probe.kind,
        implemented: probe.implemented,
        label: probe.label.into(),
        executable: Some(probe.executable.into()),
        status,
        version,
        install_hint: probe.install_hint.into(),
        detail,
    }
}

/// Probes the version control tools and every registered host, concurrently.
pub async fn discover(host: &Host, specs: &[DiscoverySpec]) -> SourceControlDiscoveryResult {
    let cwd = host.cwd().to_string_lossy().into_owned();
    let (version_control_systems, source_control_providers) = futures_util::future::join(
        futures_util::future::join_all(VCS_PROBES.iter().map(|probe| probe_vcs(host, probe))),
        futures_util::future::join_all(specs.iter().map(|spec| probe_source_control_provider(host, spec, &cwd))),
    )
    .await;
    SourceControlDiscoveryResult {
        version_control_systems,
        source_control_providers,
    }
}
