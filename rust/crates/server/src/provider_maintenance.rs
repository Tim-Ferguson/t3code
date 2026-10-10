//! Who owns a provider executable and what could update it (providerMaintenance.ts), the semver gates
//! it relies on (shared/semver.ts) and the bundled compatibility policies (providerCompatibility.ts).
//! A package manager is only named when the executable's real path proves it created the install, so an
//! update never runs a tool against an install it did not make.
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::OnceLock,
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Darwin,
    Win32,
}
impl Platform {
    pub fn host() -> Self {
        if cfg!(windows) {
            Self::Win32
        } else if cfg!(target_os = "macos") {
            Self::Darwin
        } else {
            Self::Linux
        }
    }
}

// ---- shared/semver.ts ----

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}
fn split_dash(value: &str) -> (&str, Option<&str>) {
    // JS `split("-", 2)` keeps only the first two pieces.
    let mut parts = value.split('-');
    (parts.next().unwrap_or(""), parts.next())
}
pub fn normalize_semver(version: &str) -> String {
    let (main, prerelease) = split_dash(version.trim());
    let mut segments: Vec<String> = main
        .split('.')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    while !segments.is_empty() && segments.len() < 3 {
        segments.push("0".into());
    }
    match prerelease {
        Some(prerelease) if !prerelease.is_empty() => format!("{}-{prerelease}", segments.join(".")),
        _ => segments.join("."),
    }
}
struct Semver {
    numbers: [f64; 3],
    prerelease: Vec<String>,
}
fn parse_semver(value: &str) -> Option<Semver> {
    let normalized = normalize_semver(value);
    let normalized = normalized.strip_prefix('v').unwrap_or(&normalized);
    let (main, prerelease) = split_dash(normalized);
    let segments: Vec<&str> = main.split('.').collect();
    if segments.len() != 3 || !segments.iter().all(|segment| digits(segment)) {
        return None;
    }
    let mut numbers = [0.0; 3];
    for (slot, segment) in numbers.iter_mut().zip(&segments) {
        *slot = segment.parse::<f64>().ok().filter(|number| number.is_finite())?;
    }
    let prerelease = prerelease
        .map(|prerelease| {
            prerelease
                .split('.')
                .map(str::trim)
                .filter(|segment| !segment.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Some(Semver { numbers, prerelease })
}
fn compare_identifier(left: &str, right: &str) -> std::cmp::Ordering {
    match (digits(left), digits(right)) {
        (true, true) => left
            .parse::<f64>()
            .unwrap_or(0.0)
            .partial_cmp(&right.parse::<f64>().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal),
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => crate::workspace_entries::collate(left, right),
    }
}
/// Sign of `compareSemverVersions(left, right)`.
pub fn compare_semver(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (Some(a), Some(b)) = (parse_semver(left), parse_semver(right)) else {
        return crate::workspace_entries::collate(left, right);
    };
    for (x, y) in a.numbers.iter().zip(&b.numbers) {
        if x != y {
            return x.partial_cmp(y).unwrap_or(Equal);
        }
    }
    match (a.prerelease.is_empty(), b.prerelease.is_empty()) {
        (true, true) => return Equal,
        (true, false) => return Greater,
        (false, true) => return Less,
        _ => {}
    }
    for index in 0..a.prerelease.len().max(b.prerelease.len()) {
        match (a.prerelease.get(index), b.prerelease.get(index)) {
            (None, _) => return Less,
            (_, None) => return Greater,
            (Some(x), Some(y)) => match compare_identifier(x, y) {
                Equal => {}
                other => return other,
            },
        }
    }
    Equal
}
/// `satisfiesSemverRange`: comparator groups joined by `||`, prereleases compare as their release.
pub fn satisfies_range(raw_version: &str, range: &str) -> bool {
    static VERSION: OnceLock<Regex> = OnceLock::new();
    static COMPARATOR: OnceLock<Regex> = OnceLock::new();
    let version = VERSION.get_or_init(|| {
        Regex::new(r"^([0-9]+)(?:\.([0-9]+))?(?:\.([0-9]+))?(?:-[0-9A-Za-z.-]+)?$").unwrap()
    });
    let comparator =
        COMPARATOR.get_or_init(|| Regex::new(r"^(\^|>=|>|<=|<|=)?\s*v?([0-9]+(?:\.[0-9]+){0,2})$").unwrap());
    let trimmed = raw_version.trim();
    let Some(captures) = version.captures(trimmed.strip_prefix('v').unwrap_or(trimmed)) else {
        return false;
    };
    let number = |text: Option<regex::Match>| {
        text.and_then(|text| text.as_str().parse::<f64>().ok()).unwrap_or(0.0)
    };
    let version = [number(captures.get(1)), number(captures.get(2)), number(captures.get(3))];
    range.split("||").any(|group| {
        let comparators: Vec<&str> = group.split_whitespace().collect();
        !comparators.is_empty()
            && comparators.iter().all(|text| {
                let Some(captures) = comparator.captures(text.trim()) else {
                    return false;
                };
                let target: Vec<f64> = captures[2]
                    .split('.')
                    .map(|part| part.parse::<f64>().unwrap_or(0.0))
                    .collect();
                let target = [target[0], *target.get(1).unwrap_or(&0.0), *target.get(2).unwrap_or(&0.0)];
                let compared = version
                    .iter()
                    .zip(&target)
                    .find(|(x, y)| x != y)
                    .map(|(x, y)| if x > y { 1 } else { -1 })
                    .unwrap_or(0);
                match captures.get(1).map(|operator| operator.as_str()).unwrap_or("=") {
                    "^" => {
                        if compared < 0 {
                            false
                        } else if target[0] > 0.0 {
                            version[0] == target[0]
                        } else if target[1] > 0.0 {
                            version[0] == 0.0 && version[1] == target[1]
                        } else {
                            version[0] == 0.0 && version[1] == 0.0 && version[2] == target[2]
                        }
                    }
                    ">=" => compared >= 0,
                    ">" => compared > 0,
                    "<=" => compared <= 0,
                    "<" => compared < 0,
                    _ => compared == 0,
                }
            })
    })
}

// ---- providerCompatibility.ts ----

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Policy {
    driver: String,
    t3_code_range: String,
    recommended_range: Option<String>,
    recommended_version: Option<String>,
    ranges: Vec<PolicyRange>,
}
#[derive(Deserialize)]
struct PolicyRange {
    range: String,
    status: String,
}
fn bundled_policies() -> &'static [Policy] {
    static POLICIES: OnceLock<Vec<Policy>> = OnceLock::new();
    POLICIES.get_or_init(|| {
        serde_json::from_str(include_str!("provider-compatibility.json"))
            .expect("bundled compatibility policies")
    })
}
/// `resolveProviderCompatibility` over the bundled policies, for this server's own version.
pub fn compatibility_advisory(driver: &str, version: Option<&str>, t3_version: &str) -> Option<Value> {
    let policy = bundled_policies()
        .iter()
        .find(|policy| policy.driver == driver && satisfies_range(t3_version, &policy.t3_code_range))?;
    let unprefixed = version.map(|version| version.strip_prefix('v').unwrap_or(version));
    let stable: Option<String> = unprefixed.map(|version| match driver {
        "cursor" => {
            static BUILD: OnceLock<Regex> = OnceLock::new();
            let build = BUILD.get_or_init(|| Regex::new(r"^([0-9]{4}\.[0-9]{2}\.[0-9]{2})-[a-f0-9]+$").unwrap());
            build.replace(version, "$1").into_owned()
        }
        "antigravity" => {
            static RELEASE: OnceLock<Regex> = OnceLock::new();
            let release = RELEASE.get_or_init(|| Regex::new(r"^agy_acp_server_([0-9]+\.[0-9]+\.[0-9]+)$").unwrap());
            release.replace(version, "$1").into_owned()
        }
        _ => version.to_owned(),
    });
    static STABLE: OnceLock<Regex> = OnceLock::new();
    let stable_pattern = STABLE.get_or_init(|| Regex::new(r"^[0-9]+\.[0-9]+\.[0-9]+$").unwrap());
    let status = stable
        .as_deref()
        .filter(|stable| stable_pattern.is_match(stable))
        .and_then(|stable| policy.ranges.iter().find(|entry| satisfies_range(stable, &entry.range)))
        .map(|entry| entry.status.as_str())
        .unwrap_or("unknown");
    let message = match status {
        "broken" => Some("This provider version is known to be incompatible with this T3 Code release."),
        "unsupported" => Some("This provider version is outside the supported range for this T3 Code release."),
        "graceful" => Some("This provider version has limited compatibility with this T3 Code release."),
        _ => None,
    };
    let recommendation = policy.recommended_version.as_ref().or(policy.recommended_range.as_ref());
    Some(json!({
        "status": status,
        "message": message.map(|message| match recommendation {
            Some(recommendation) => format!("{message} Use {recommendation}."),
            None => message.to_owned(),
        }),
        "recommendedVersion": policy.recommended_version,
        "recommendedRange": policy.recommended_range,
    }))
}

// ---- providerMaintenance.ts ----

#[derive(Clone, Debug, PartialEq)]
pub struct UpdateAction {
    pub command: String,
    pub executable: String,
    pub args: Vec<String>,
    pub lock_key: String,
    /// Extra environment for the updater, on top of the server's own (e.g. `CODEX_HOME`).
    pub env: Option<BTreeMap<String, String>>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Capabilities {
    pub provider: String,
    pub package_name: Option<String>,
    pub update: Option<UpdateAction>,
    /// `None`: the registry entry for `package_name` is authoritative. `Some(None)`: the installer was
    /// asked and did not know.
    pub latest_version: Option<Option<String>>,
}
impl Capabilities {
    pub fn manual(provider: &str, package_name: Option<&str>) -> Self {
        Self { provider: provider.into(), package_name: package_name.map(str::to_owned), update: None, latest_version: None }
    }
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "provider": self.provider,
            "packageName": self.package_name,
            "update": self.update.as_ref().map(|update| {
                let mut value = json!({"command":update.command,"executable":update.executable,"args":update.args,"lockKey":update.lock_key});
                if let Some(env) = &update.env {
                    value["env"] = json!(env);
                }
                value
            }),
        });
        if let Some(latest) = &self.latest_version {
            value["latestVersion"] = json!(latest);
        }
        value
    }
}
fn quote_shell_word(word: &str, platform: Platform) -> String {
    static POSIX: OnceLock<Regex> = OnceLock::new();
    static WINDOWS: OnceLock<Regex> = OnceLock::new();
    let safe = if platform == Platform::Win32 {
        WINDOWS.get_or_init(|| Regex::new(r"^[A-Za-z0-9_./:\\@=-]+$").unwrap())
    } else {
        POSIX.get_or_init(|| Regex::new(r"^[A-Za-z0-9_./:@=-]+$").unwrap())
    };
    if safe.is_match(word) {
        return word.into();
    }
    if platform == Platform::Win32 {
        let doubled: String = word
            .chars()
            .flat_map(|c| if matches!(c, '\'' | '\u{2018}' | '\u{2019}') { vec![c, c] } else { vec![c] })
            .collect();
        format!("'{doubled}'")
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}
pub struct CapabilityInput<'a> {
    pub provider: &'a str,
    pub package_name: Option<&'a str>,
    pub update_executable: Option<&'a str>,
    pub update_args: Vec<String>,
    pub update_lock_key: Option<&'a str>,
    /// Shown to the user instead of `<executable> <args>`; for a bare tool name like `brew`.
    pub update_command: Option<String>,
    pub platform: Platform,
    pub env: Option<BTreeMap<String, String>>,
    pub latest_version: Option<Option<String>>,
}
impl<'a> CapabilityInput<'a> {
    pub fn new(provider: &'a str, package_name: &'a str, executable: &'a str, args: &[&str], lock_key: &'a str, platform: Platform) -> Self {
        Self {
            provider,
            package_name: Some(package_name),
            update_executable: Some(executable),
            update_args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            update_lock_key: Some(lock_key),
            update_command: None,
            platform,
            env: None,
            latest_version: None,
        }
    }
    pub fn build(self) -> Capabilities {
        let update = match (self.update_executable, self.update_lock_key) {
            (Some(executable), Some(lock_key)) => {
                let command = self.update_command.unwrap_or_else(|| {
                    let quoted = quote_shell_word(executable, self.platform);
                    // Windows terminals default to PowerShell, where a quoted executable needs `&`.
                    let first = if self.platform == Platform::Win32 && quoted != executable { format!("& {quoted}") } else { quoted };
                    std::iter::once(first)
                        .chain(self.update_args.iter().map(|arg| quote_shell_word(arg, self.platform)))
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                Some(UpdateAction { command, executable: executable.into(), args: self.update_args.clone(), lock_key: lock_key.into(), env: self.env })
            }
            _ => None,
        };
        Capabilities { provider: self.provider.into(), package_name: self.package_name.map(str::to_owned), update, latest_version: self.latest_version }
    }
}
/// Pin only package-manager actions this server owns, preserving prefix, scripts, env and lock.
pub fn targeted_update(capabilities: &Capabilities, version: &str) -> Option<UpdateAction> {
    static VERSION: OnceLock<Regex> = OnceLock::new();
    static LOCK: OnceLock<Regex> = OnceLock::new();
    if !VERSION.get_or_init(|| Regex::new(r"^[0-9]+\.[0-9]+\.[0-9]+$").unwrap()).is_match(version) {
        return None;
    }
    let update = capabilities.update.as_ref()?;
    let package = capabilities.package_name.as_deref()?;
    if !LOCK
        .get_or_init(|| Regex::new(r"^(?:npm-global:|bun-global$|pnpm-global$|vite-plus-global$|yarn-global$|volta$)").unwrap())
        .is_match(&update.lock_key)
    {
        return None;
    }
    let latest = format!("{package}@latest");
    let index = update.args.iter().position(|arg| *arg == latest || arg == package)?;
    let mut args = update.args.clone();
    let previous = std::mem::replace(&mut args[index], format!("{package}@{version}"));
    let command = match update.command.rfind(&previous) {
        None => update.command.clone(),
        Some(at) => format!("{}{package}@{version}{}", &update.command[..at], &update.command[at + previous.len()..]),
    };
    Some(UpdateAction { args, command, ..update.clone() })
}

pub fn normalize_command_path(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}
fn is_vite_plus(path: &str) -> bool {
    normalize_command_path(path).contains("/.vite-plus/bin/")
}
fn is_bun(path: &str) -> bool {
    normalize_command_path(path).contains("/.bun/bin/")
}
fn is_yarn(path: &str) -> bool {
    static YARN: OnceLock<Regex> = OnceLock::new();
    YARN.get_or_init(|| Regex::new(r"/yarn/(?:data/)?global/node_modules/").unwrap()).is_match(&normalize_command_path(path))
}
fn is_mise(path: &str) -> bool {
    let normalized = normalize_command_path(path);
    normalized.contains("/mise/installs/") || normalized.contains("/mise/shims/")
}
fn is_pnpm(path: &str) -> bool {
    let normalized = normalize_command_path(path);
    ["/.local/share/pnpm/", "/library/pnpm/", "/local/share/pnpm/", "/appdata/local/pnpm/", "/pnpm/global/"]
        .iter()
        .any(|segment| normalized.contains(segment))
}
/// The npm global prefix that owns a package, from `<prefix>/lib/node_modules/<pkg>/…`. A project-local
/// `node_modules` is not a global install.
pub fn npm_global_prefix(real_command_path: &str, package_name: &str) -> Option<String> {
    static MISE: OnceLock<Regex> = OnceLock::new();
    let slash_path = real_command_path.replace('\\', "/");
    let normalized = slash_path.to_lowercase();
    let segment = format!("/lib/node_modules/{}/", package_name.to_lowercase());
    let index = normalized.rfind(&segment)?;
    let before = &normalized[..index];
    if before.contains("/node_modules/") {
        return None;
    }
    let mise = MISE.get_or_init(|| Regex::new(r"/mise/installs/([^/]+)/[^/]+$").unwrap());
    if mise.captures(before).is_some_and(|tool| &tool[1] != "node") {
        return None;
    }
    Some(if index == 0 { "/".into() } else { slash_path.get(..index)?.to_owned() })
}
#[derive(Clone, Debug, PartialEq)]
pub struct Homebrew {
    pub cask: bool,
    pub name: String,
    pub prefix: String,
}
pub fn homebrew_ownership(real_command_path: &str) -> Option<Homebrew> {
    static KEG: OnceLock<Regex> = OnceLock::new();
    let keg = KEG.get_or_init(|| Regex::new(r"(?i)^(.*)/(cellar|caskroom)/([^/]+)/[^/]+/").unwrap());
    let path = real_command_path.replace('\\', "/");
    let captures = keg.captures(&path)?;
    Some(Homebrew { cask: captures[2].eq_ignore_ascii_case("caskroom"), name: captures[3].into(), prefix: captures[1].into() })
}
/// Cask versions may carry a build suffix after a comma (`1.2.3,456`).
pub fn homebrew_latest_version(info_json: &str, ownership: &Homebrew) -> Option<String> {
    let info: Value = serde_json::from_str(info_json).ok()?;
    let raw = if ownership.cask {
        info["casks"][0]["version"].as_str()?.split(',').next()?.to_owned()
    } else {
        info["formulae"][0]["versions"]["stable"].as_str()?.to_owned()
    };
    non_empty(&raw)
}
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[derive(Clone)]
pub struct Native {
    pub args: Vec<String>,
    /// Paths the provider's own installer owns.
    pub is_command_path: Option<fn(&str) -> bool>,
    pub env: Option<BTreeMap<String, String>>,
}
#[derive(Clone)]
pub struct Definition {
    pub provider: &'static str,
    pub npm_package_name: &'static str,
    /// The provider's own updater (`claude update`): used for paths its installer owns, and otherwise only
    /// when no package manager is proven.
    pub native: Option<Native>,
}
pub fn is_codex_standalone(path: &str) -> bool {
    normalize_command_path(path).contains("/packages/standalone/")
}
pub fn is_claude_native(path: &str) -> bool {
    let normalized = normalize_command_path(path);
    normalized.ends_with("/.local/bin/claude") || normalized.ends_with("/.local/bin/claude.exe") || normalized.contains("/.local/share/claude/")
}
/// `codex update` replaces the standalone tree under the shared `CODEX_HOME`.
pub fn codex_definition(shared_home: &str) -> Definition {
    Definition {
        provider: "codex",
        npm_package_name: "@openai/codex",
        native: Some(Native { args: vec!["update".into()], is_command_path: Some(is_codex_standalone), env: Some(BTreeMap::from([("CODEX_HOME".to_owned(), shared_home.to_owned())])) }),
    }
}
/// `grok update` finds the installer that owns the binary itself, so the resolved executable is its own updater,
/// whatever the path (`GrokDriver.ts` `UPDATE`). It installs under `GROK_HOME`, so it runs with the instance's own
/// environment: `env` is that, and it stays in memory (it is never part of an advisory or a state). With no
/// executable there is nothing to update.
pub fn grok_definition(env: BTreeMap<String, String>) -> Definition {
    Definition {
        provider: "grok",
        npm_package_name: "@xai-official/grok",
        native: Some(Native { args: vec!["update".into()], is_command_path: Some(|_| true), env: Some(env).filter(|env| !env.is_empty()) }),
    }
}
/// A driver with no updater and no package: its version advisory is always manual.
pub fn manual_definition(provider: &'static str) -> Definition {
    Definition { provider, npm_package_name: "", native: None }
}
/// `PiDriver.ts` `UPDATE`: Pi's own updater (`pi update --self`) covers its installer and the npm, pnpm, yarn and bun
/// globals, and no path is one only its installer owns.
pub fn pi_definition() -> Definition {
    Definition { provider: "pi", npm_package_name: "@earendil-works/pi-coding-agent", native: Some(Native { args: vec!["update".into(), "--self".into()], is_command_path: None, env: None }) }
}
pub fn claude_definition() -> Definition {
    Definition {
        provider: "claudeAgent",
        npm_package_name: "@anthropic-ai/claude-code",
        native: Some(Native { args: vec!["update".into()], is_command_path: Some(is_claude_native), env: None }),
    }
}
pub fn is_opencode_native(path: &str) -> bool {
    let normalized = normalize_command_path(path);
    normalized.ends_with("/.opencode/bin/opencode") || normalized.ends_with("/.opencode/bin/opencode.exe")
}
/// OpenCode 1.x ships as `opencode-ai` and 2.x as `@opencode/cli`; an install is only ever updated within its own
/// package, since 2.x converts the shared database in place (`openCodeUpdateFor`).
pub fn opencode_definition(v2: bool) -> Definition {
    Definition {
        provider: "opencode",
        npm_package_name: if v2 { "@opencode/cli" } else { "opencode-ai" },
        native: Some(Native { args: vec!["upgrade".into()], is_command_path: Some(is_opencode_native), env: None }),
    }
}

// ---- resolveCommandPath (shared/shell.ts) ----

#[cfg(unix)]
fn can_execute(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
fn can_execute(_: &Path) -> bool {
    true
}
fn windows_extensions(env: &HashMap<String, String>) -> Vec<String> {
    let fallback = vec![".COM".to_owned(), ".EXE".into(), ".BAT".into(), ".CMD".into()];
    let Some(raw) = env.get("PATHEXT").filter(|raw| !raw.is_empty()) else {
        return fallback;
    };
    let mut parsed: Vec<String> = vec![];
    for entry in raw.split(';').map(str::trim).filter(|entry| !entry.is_empty()) {
        let entry = if entry.starts_with('.') { entry.to_uppercase() } else { format!(".{}", entry.to_uppercase()) };
        if !parsed.contains(&entry) {
            parsed.push(entry);
        }
    }
    if parsed.is_empty() { fallback } else { parsed }
}
fn is_executable_file(path: &Path, platform: Platform, extensions: &[String]) -> bool {
    if !std::fs::metadata(path).is_ok_and(|info| info.is_file()) {
        return false;
    }
    if platform == Platform::Win32 {
        return path.extension().is_some_and(|extension| extensions.contains(&format!(".{}", extension.to_string_lossy().to_uppercase())));
    }
    can_execute(path)
}
/// The executable `command` names: an explicit path must be one, a bare name is searched on `PATH`.
pub fn resolve_command_path(command: &str, env: &HashMap<String, String>, platform: Platform) -> Option<String> {
    let extensions = if platform == Platform::Win32 { windows_extensions(env) } else { vec![] };
    let candidates: Vec<String> = if platform != Platform::Win32 {
        vec![command.to_owned()]
    } else {
        let extension = Path::new(command).extension().map(|extension| format!(".{}", extension.to_string_lossy()));
        match extension.filter(|extension| extensions.contains(&extension.to_uppercase())) {
            Some(extension) => {
                let stem = &command[..command.len() - extension.len()];
                let mut all = vec![command.to_owned(), format!("{stem}{}", extension.to_uppercase()), format!("{stem}{}", extension.to_lowercase())];
                all.dedup();
                all
            }
            None => std::iter::once(command.to_owned()).chain(extensions.iter().flat_map(|extension| [format!("{command}{extension}"), format!("{command}{}", extension.to_lowercase())])).collect(),
        }
    };
    if command.contains('/') || command.contains('\\') {
        return candidates.into_iter().find(|candidate| is_executable_file(Path::new(candidate), platform, &extensions));
    }
    let path = env.get("PATH").or_else(|| env.get("Path")).or_else(|| env.get("path"))?;
    let delimiter = if platform == Platform::Win32 { ';' } else { ':' };
    let mut seen: Vec<&str> = vec![];
    for entry in path.split(delimiter) {
        let entry = entry.trim().trim_matches('"');
        if entry.is_empty() || seen.contains(&entry) {
            continue;
        }
        seen.push(entry);
        for candidate in &candidates {
            let candidate = Path::new(entry).join(candidate);
            if is_executable_file(&candidate, platform, &extensions) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

// ---- ownership resolution ----

pub struct Context {
    pub resolved_command_path: String,
    pub real_command_path: String,
    pub env: HashMap<String, String>,
    pub platform: Platform,
}

/// Run `brew <args>` for its stdout; None on failure, timeout or oversized output.
async fn run_homebrew(brew: &str, args: &[&str], env: &HashMap<String, String>) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new(brew)
        .args(args)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let collected = tokio::time::timeout(Duration::from_secs(10), async {
        let mut bytes = vec![];
        (&mut stdout).take(256 * 1024 + 1).read_to_end(&mut bytes).await.ok()?;
        let status = child.wait().await.ok()?;
        (status.success() && bytes.len() <= 256 * 1024).then_some(bytes)
    })
    .await
    .ok()??;
    Some(String::from_utf8_lossy(&collected).into_owned())
}
fn is_volta_package(context: &Context, package: &str) -> bool {
    let shim = Path::new(&normalize_command_path(&context.real_command_path))
        .file_name()
        .map(|name| name.to_string_lossy().trim_end_matches(".exe").to_owned());
    if shim.as_deref() != Some("volta-shim") {
        return false;
    }
    let Some(home) = Path::new(&context.resolved_command_path).parent().and_then(Path::parent) else {
        return false;
    };
    let mut directory = home.join("tools").join("image").join("packages");
    for part in package.split('/') {
        directory.push(part);
    }
    directory.exists()
}
fn is_mise_wrapper(context: &Context) -> bool {
    static MISE: OnceLock<Regex> = OnceLock::new();
    let path = Path::new(&context.real_command_path);
    if std::fs::metadata(path).map_or(true, |info| info.len() > 16 * 1024) {
        return false;
    }
    let script = std::fs::read_to_string(path).unwrap_or_default();
    script.starts_with("#!") && MISE.get_or_init(|| Regex::new(r"\bmise\s+(?:x|exec)\b").unwrap()).is_match(&script)
}
/// POSIX npm links `<prefix>/bin/<cmd>` into the package; Windows writes `.cmd` shims beside `node_modules`.
fn npm_prefix(context: &Context, package: &str) -> Option<String> {
    if let Some(prefix) = npm_global_prefix(&context.real_command_path, package) {
        return Some(prefix);
    }
    if context.platform != Platform::Win32 {
        return None;
    }
    let shim_dir = Path::new(&context.resolved_command_path).parent()?;
    let mut manifest = shim_dir.join("node_modules");
    for part in package.split('/') {
        manifest.push(part);
    }
    manifest.join("package.json").exists().then(|| shim_dir.to_string_lossy().into_owned())
}
fn plain(definition: &Definition, executable: &str, args: &[&str], lock_key: &str, platform: Platform) -> Capabilities {
    CapabilityInput::new(definition.provider, definition.npm_package_name, executable, args, lock_key, platform).build()
}
/// Derive update capabilities from where the executable lives. An unproven install falls back to the
/// provider's own updater, which detects its installer itself, and stays manual-only without one.
pub async fn resolve_with_context(definition: &Definition, context: Option<&Context>) -> Capabilities {
    let package = definition.npm_package_name;
    let manual = Capabilities::manual(definition.provider, Some(package));
    let Some(context) = context else {
        return manual;
    };
    let platform = context.platform;
    let paths = [context.resolved_command_path.as_str(), context.real_command_path.as_str()];
    let native = match &definition.native {
        Some(native) => {
            let lock_key = format!("{}-native", definition.provider);
            let mut input = CapabilityInput::new(definition.provider, package, &context.resolved_command_path, &[], &lock_key, platform);
            input.update_args = native.args.clone();
            input.env = native.env.clone();
            input.build()
        }
        None => manual.clone(),
    };
    if definition.native.as_ref().and_then(|native| native.is_command_path).is_some_and(|owns| paths.iter().any(|path| owns(path))) {
        return native;
    }
    // A `node_modules` path not proven below belongs to another package or a project.
    let fallback = if paths.iter().any(|path| normalize_command_path(path).contains("/node_modules/")) { manual.clone() } else { native };
    if paths.iter().any(|path| is_vite_plus(path)) {
        return plain(definition, "vp", &["i", "-g", package], "vite-plus-global", platform);
    }
    let latest = format!("{package}@latest");
    if paths.iter().any(|path| is_bun(path)) {
        return plain(definition, "bun", &["i", "-g", &latest], "bun-global", platform);
    }
    if paths.iter().any(|path| is_pnpm(path)) {
        return plain(definition, "pnpm", &["add", "-g", &latest], "pnpm-global", platform);
    }
    if paths.iter().any(|path| is_yarn(path)) {
        return plain(definition, "yarn", &["global", "add", &latest], "yarn-global", platform);
    }
    if is_volta_package(context, package) {
        return plain(definition, "volta", &["install", &latest], "volta", platform);
    }
    // npm's proof names the package, so it outranks a keg the path merely passes through.
    if let Some(prefix) = npm_prefix(context, package) {
        // npm 12 blocks install scripts by default and still exits 0, so allow this one package's.
        return plain(
            definition,
            "npm",
            &["install", "-g", "--prefix", &prefix, &format!("--allow-scripts={package}"), &latest],
            &format!("npm-global:{}", normalize_command_path(&prefix)),
            platform,
        );
    }
    if paths.iter().any(|path| is_mise(path)) || is_mise_wrapper(context) {
        return manual;
    }
    if let Some(homebrew) = homebrew_ownership(&context.real_command_path) {
        if !homebrew.cask && homebrew.name.eq_ignore_ascii_case("mise") {
            return manual;
        }
        let keg_brew = Path::new(&homebrew.prefix).join("bin").join("brew");
        let brew = if keg_brew.exists() { Some(keg_brew.to_string_lossy().into_owned()) } else { resolve_command_path("brew", &context.env, platform) };
        let Some(brew) = brew else {
            return fallback;
        };
        // A keg-shaped path is only Homebrew's if it sits under the prefix of the `brew` that would upgrade it.
        let prefix = run_homebrew(&brew, &["--prefix"], &context.env).await.and_then(|text| non_empty(&text));
        let real_prefix = prefix.map(|prefix| std::fs::canonicalize(&prefix).map(|path| path.to_string_lossy().into_owned()).unwrap_or(prefix));
        if real_prefix.is_none_or(|prefix| normalize_command_path(&prefix) != normalize_command_path(&homebrew.prefix)) {
            return fallback;
        }
        let args: Vec<&str> = if homebrew.cask { vec!["upgrade", "--cask", &homebrew.name] } else { vec!["upgrade", &homebrew.name] };
        // Homebrew lags npm by hours on every release, so compare against what `brew upgrade` delivers.
        let info = run_homebrew(&brew, &["info", "--json=v2", &homebrew.name], &context.env).await;
        let mut input = CapabilityInput::new(definition.provider, package, &brew, &args, "homebrew", platform);
        input.update_command = Some(std::iter::once("brew").chain(args.iter().copied()).collect::<Vec<_>>().join(" "));
        input.latest_version = Some(info.and_then(|info| homebrew_latest_version(&info, &homebrew)));
        return input.build();
    }
    fallback
}
/// Locate the configured executable, follow symlinks, and derive its capabilities. A binary that cannot
/// be found yields the no-context answer.
pub async fn resolve_capabilities(definition: &Definition, binary_path: Option<&str>, env: &HashMap<String, String>, platform: Platform) -> Capabilities {
    let Some(binary) = binary_path.and_then(non_empty) else {
        return resolve_with_context(definition, None).await;
    };
    let Some(resolved) = resolve_command_path(&binary, env, platform) else {
        return resolve_with_context(definition, None).await;
    };
    let Ok(real) = std::fs::canonicalize(&resolved) else {
        return resolve_with_context(definition, None).await;
    };
    let context = Context { resolved_command_path: resolved, real_command_path: real.to_string_lossy().into_owned(), env: env.clone(), platform };
    resolve_with_context(definition, Some(&context)).await
}

// ---- advisories ----

pub const UPDATE_TOAST_MESSAGE: &str = "Install the update now or review provider settings.";
pub fn version_advisory(driver_capabilities: &Capabilities, current: Option<&str>, latest: Option<&str>, checked_at: Option<&str>) -> Value {
    let (status, message) = match (current, latest) {
        (Some(current), Some(latest)) if compare_semver(current, latest).is_lt() => ("behind_latest", Some(UPDATE_TOAST_MESSAGE)),
        (Some(_), Some(_)) => ("current", None),
        _ => ("unknown", None),
    };
    json!({
        "status": status,
        "currentVersion": current,
        "latestVersion": latest,
        "updateCommand": driver_capabilities.update.as_ref().map(|update| &update.command),
        "canUpdate": driver_capabilities.update.is_some(),
        "canInstallVersion": targeted_update(driver_capabilities, "0.0.0").is_some(),
        "checkedAt": checked_at,
        "message": message,
    })
}
/// JavaScript's `encodeURIComponent`.
pub fn encode_uri_component(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}
/// `https://registry.npmjs.org/<encoded package>/latest`
pub fn registry_latest_url(base: &str, package: &str) -> String {
    format!("{}/{}/latest", base.trim_end_matches('/'), encode_uri_component(package))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixtures() -> Vec<Value> {
        include_str!("../tests/fixtures/provider-maintenance.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
    fn platform(name: &str) -> Platform {
        match name {
            "win32" => Platform::Win32,
            "darwin" => Platform::Darwin,
            _ => Platform::Linux,
        }
    }
    fn sign(order: std::cmp::Ordering) -> i64 {
        order as i64
    }
    #[test]
    fn semver_and_compatibility_match_the_original_source() {
        let mut checked = 0;
        for fixture in fixtures() {
            match fixture["kind"].as_str().unwrap() {
                "compare" => assert_eq!(sign(compare_semver(fixture["left"].as_str().unwrap(), fixture["right"].as_str().unwrap())), fixture["expected"].as_i64().unwrap(), "{fixture}"),
                "range" => assert_eq!(satisfies_range(fixture["version"].as_str().unwrap(), fixture["range"].as_str().unwrap()), fixture["expected"].as_bool().unwrap(), "{fixture}"),
                "compatibility" => assert_eq!(
                    compatibility_advisory(fixture["driver"].as_str().unwrap(), fixture["version"].as_str(), fixture["t3"].as_str().unwrap()).unwrap_or(Value::Null),
                    fixture["expected"],
                    "{fixture}"
                ),
                _ => continue,
            }
            checked += 1;
        }
        assert!(checked > 700);
    }
    #[test]
    fn commands_ownership_parsing_and_advisories_match_the_original_source() {
        for fixture in fixtures() {
            match fixture["kind"].as_str().unwrap() {
                "homebrewInfo" => {
                    let ownership = Homebrew { cask: fixture["ownership"]["kind"] == "cask", name: fixture["ownership"]["name"].as_str().unwrap().into(), prefix: fixture["ownership"]["prefix"].as_str().unwrap().into() };
                    assert_eq!(homebrew_latest_version(fixture["infoJson"].as_str().unwrap(), &ownership), fixture["expected"].as_str().map(str::to_owned), "{fixture}");
                }
                "homebrewOwnership" => {
                    let actual = homebrew_ownership(fixture["real"].as_str().unwrap());
                    let expected = &fixture["expected"];
                    assert_eq!(actual.as_ref().map(|actual| (actual.cask, actual.name.as_str(), actual.prefix.as_str())), expected.as_object().map(|expected| (expected["kind"] == "cask", expected["name"].as_str().unwrap(), expected["prefix"].as_str().unwrap())), "{fixture}");
                }
                "npmPrefix" => assert_eq!(npm_global_prefix(fixture["real"].as_str().unwrap(), fixture["packageName"].as_str().unwrap()), fixture["expected"].as_str().map(str::to_owned), "{fixture}"),
                "capabilities" => {
                    let input = &fixture["input"];
                    let args: Vec<&str> = input["updateArgs"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
                    let built = CapabilityInput::new(input["provider"].as_str().unwrap(), input["packageName"].as_str().unwrap(), input["updateExecutable"].as_str().unwrap(), &args, input["updateLockKey"].as_str().unwrap(), platform(fixture["platform"].as_str().unwrap())).build();
                    assert_eq!(built.to_json(), fixture["expected"], "{fixture}");
                }
                "targeted" => {
                    let capabilities = capabilities_from_json(&fixture["capabilities"]);
                    let actual = targeted_update(&capabilities, fixture["version"].as_str().unwrap());
                    let projected = actual.map(|action| json!({"command":action.command,"executable":action.executable,"args":action.args,"lockKey":action.lock_key}));
                    assert_eq!(projected.unwrap_or(Value::Null), fixture["expected"], "{fixture}");
                }
                "advisory" => {
                    let capabilities = capabilities_from_json(&fixture["capabilities"]);
                    assert_eq!(version_advisory(&capabilities, fixture["currentVersion"].as_str(), fixture["latestVersion"].as_str(), fixture["checkedAt"].as_str()), fixture["expected"], "{fixture}");
                }
                _ => {}
            }
        }
    }
    fn capabilities_from_json(value: &Value) -> Capabilities {
        Capabilities {
            provider: value["provider"].as_str().unwrap().into(),
            package_name: value["packageName"].as_str().map(str::to_owned),
            update: value["update"].as_object().map(|update| UpdateAction {
                command: update["command"].as_str().unwrap().into(),
                executable: update["executable"].as_str().unwrap().into(),
                args: update["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap().into()).collect(),
                lock_key: update["lockKey"].as_str().unwrap().into(),
                env: None,
            }),
            latest_version: None,
        }
    }
    fn definition(name: &str, root: &str) -> Definition {
        let native = |args: &[&str], is_command_path: Option<fn(&str) -> bool>, env: Option<BTreeMap<String, String>>| Native { args: args.iter().map(|arg| (*arg).to_owned()).collect(), is_command_path, env };
        match name {
            "codex" => Definition { provider: "codex", npm_package_name: "@openai/codex", native: Some(native(&["update"], Some(is_codex_standalone), Some(BTreeMap::from([("CODEX_HOME".to_owned(), format!("{root}/codexhome"))])))) },
            "claude" => claude_definition(),
            "plain" => Definition { provider: "codex", npm_package_name: "@example/package-tool", native: Some(native(&["self-update"], None, None)) },
            _ => Definition { provider: "codex", npm_package_name: "@example/package-tool", native: None },
        }
    }
    #[tokio::test]
    async fn ownership_of_fake_installs_matches_the_original_source() {
        let mut checked = 0;
        for fixture in fixtures().into_iter().filter(|fixture| fixture["kind"] == "resolve") {
            let directory = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(directory.path()).unwrap().to_string_lossy().into_owned();
            let replace = |text: &str| text.replace("$ROOT", &root);
            for file in fixture["files"].as_array().unwrap() {
                let destination = Path::new(&root).join(file["path"].as_str().unwrap());
                if file["directory"] == true {
                    std::fs::create_dir_all(&destination).unwrap();
                    continue;
                }
                std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
                if let Some(target) = file["link"].as_str() {
                    std::os::unix::fs::symlink(target, &destination).unwrap();
                } else {
                    std::fs::write(&destination, replace(file["contents"].as_str().unwrap())).unwrap();
                    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(file["mode"].as_u64().unwrap() as u32)).unwrap();
                }
            }
            let env: HashMap<String, String> = fixture["env"].as_object().unwrap().iter().map(|(key, value)| (key.clone(), replace(value.as_str().unwrap()))).collect();
            let definition = definition(fixture["definition"].as_str().unwrap(), &root);
            let actual = resolve_capabilities(&definition, Some(&replace(fixture["binaryPath"].as_str().unwrap())), &env, Platform::host()).await;
            let expected = fixture["expected"].to_string().replace("$ROOT", &root).replace("$root", &root.to_lowercase());
            assert_eq!(actual.to_json(), serde_json::from_str::<Value>(&expected).unwrap(), "{}", fixture["label"]);
            checked += 1;
        }
        assert!(checked >= 28);
    }
}
