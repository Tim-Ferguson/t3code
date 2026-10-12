//! Where a Codex instance keeps its state (`provider/Drivers/CodexHomeLayout.ts`, `provider/CodexManagedHome.ts`).
//!
//! An instance with a shadow home runs Codex with `CODEX_HOME` pointing at the shadow, which holds the instance's own `auth.json` and
//! caches and links everything else to the shared home, so two accounts share sessions, skills and plugins and nothing secret.
//! The links are made again before every launch.
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Component, Path, PathBuf},
};

/// `CodexHomeLayout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeLayout {
    pub auth_overlay: bool,
    pub shared: PathBuf,
    /// What `CODEX_HOME` is set to; `None` leaves Codex its own default.
    pub effective: Option<PathBuf>,
    pub continuation_key: String,
}

const KNOWN_SHARED_DIRECTORIES: [&str; 10] = ["sessions", "archived_sessions", "sqlite", "shell_snapshots", "worktrees", "skills", "plugins", "cache", "logs", "mcp-oauth-locks"];
const PRIVATE_ENTRY_NAMES: [&str; 2] = ["auth.json", "models_cache.json"];
const SHADOW_LOCAL_ENTRY_NAMES: [&str; 3] = ["log", "memories", "tmp"];
const REPLACEABLE_SHARED_RUNTIME_DIRECTORIES: [&str; 1] = ["mcp-oauth-locks"];

fn expand(value: &str, environment: &HashMap<String, String>) -> PathBuf {
    let value = value.trim();
    if value == "~" || value.starts_with("~/") || value.starts_with("~\\") {
        let home = environment.get("HOME").cloned().or_else(|| std::env::var("HOME").ok()).or_else(|| std::env::var("USERPROFILE").ok());
        if let Some(home) = home {
            return PathBuf::from(home).join(value.get(2..).unwrap_or(""));
        }
    }
    PathBuf::from(value)
}

fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

/// `resolveCodexHomeLayout`.
pub fn resolve(home_path: &str, shadow_home_path: &str, environment: &HashMap<String, String>) -> HomeLayout {
    let shared = if home_path.trim().is_empty() {
        let base = environment.get("HOME").cloned().or_else(|| std::env::var("HOME").ok()).unwrap_or_default();
        absolute(PathBuf::from(base).join(".codex"))
    } else {
        absolute(expand(home_path, environment))
    };
    let continuation_key = format!("codex:home:{}", shared.to_string_lossy());
    let shadow = shadow_home_path.trim();
    if shadow.is_empty() {
        return HomeLayout { auth_overlay: false, effective: (!home_path.trim().is_empty()).then(|| shared.clone()), shared, continuation_key };
    }
    HomeLayout { auth_overlay: true, effective: Some(absolute(expand(shadow, environment))), shared, continuation_key }
}

/// `resolveManagedCodexHomeLayout`: a managed instance other than the default one gets a shadow home of its own under the state directory.
pub fn resolve_managed(state_dir: &Path, instance_id: &str, home_path: &str, shadow_home_path: &str, environment: &HashMap<String, String>) -> HomeLayout {
    let shadow = if !shadow_home_path.trim().is_empty() {
        shadow_home_path.trim().to_owned()
    } else if instance_id == "codex" {
        String::new()
    } else {
        state_dir.join("providers").join("codex").join(instance_id).join("shadow").to_string_lossy().into_owned()
    };
    resolve(home_path, &shadow, environment)
}

/// Lexical `path.resolve` of a link's target against the directory the link is in.
fn resolved(directory: &Path, target: &Path) -> PathBuf {
    let joined = if target.is_absolute() { target.to_path_buf() } else { directory.join(target) };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

enum LinkState {
    Missing,
    NotSymlink,
    Symlink(PathBuf),
}

fn filesystem(operation: &str, path: &Path, target: Option<&Path>) -> String {
    let target = target.map_or(String::new(), |target| format!(" to '{}'", target.to_string_lossy()));
    format!("Codex shadow home filesystem operation '{operation}' failed for '{}'{target}.", path.to_string_lossy())
}

fn link_state(link: &Path) -> Result<LinkState, String> {
    match fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::read_link(link).map(LinkState::Symlink).map_err(|_| filesystem("readLink", link, None)),
        Ok(_) => Ok(LinkState::NotSymlink),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LinkState::Missing),
        Err(_) => Err(filesystem("readLink", link, None)),
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}
#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() { std::os::windows::fs::symlink_dir(target, link) } else { std::os::windows::fs::symlink_file(target, link) }
}

fn remove(path: &Path, recursive: bool) -> Result<(), String> {
    let outcome = if recursive { fs::remove_dir_all(path).or_else(|_| fs::remove_file(path)) } else { fs::remove_file(path).or_else(|_| fs::remove_dir(path)) };
    outcome.map_err(|_| filesystem("remove", path, None))
}

fn ensure_symlink(layout: &HomeLayout, effective: &Path, name: &str) -> Result<(), String> {
    let target = layout.shared.join(name);
    let link = effective.join(name);
    let create = || symlink(&target, &link).map_err(|_| filesystem("symlink", &link, Some(&target)));
    match link_state(&link)? {
        LinkState::NotSymlink => {
            if !REPLACEABLE_SHARED_RUNTIME_DIRECTORIES.contains(&name) {
                return Err(format!("Cannot create Codex shadow home entry '{name}' because '{}' already exists and is not a symlink.", link.to_string_lossy()));
            }
            remove(&link, true)?;
            create()
        }
        LinkState::Missing => create(),
        LinkState::Symlink(existing) => {
            if resolved(link.parent().unwrap_or(Path::new("")), &existing) != resolved(Path::new(""), &target) {
                remove(&link, false)?;
                create()?;
            }
            Ok(())
        }
    }
}

/// `materializeCodexShadowHome`: the shadow home exists and links every shared entry that is not private to the instance. A conflict
/// or a filesystem failure is reported as the message the original's error carries.
pub fn materialize(layout: &HomeLayout) -> Result<(), String> {
    if !layout.auth_overlay {
        return Ok(());
    }
    let Some(effective) = layout.effective.as_deref() else { return Ok(()) };
    if layout.shared == effective {
        return Err(format!("Codex shadow home path '{}' must be different from the shared home path '{}'.", effective.to_string_lossy(), layout.shared.to_string_lossy()));
    }
    let make = |path: &Path| fs::create_dir_all(path).map_err(|_| filesystem("makeDirectory", path, None));
    make(&layout.shared)?;
    make(effective)?;
    for directory in KNOWN_SHARED_DIRECTORIES {
        make(&layout.shared.join(directory))?;
    }
    let mut entries: BTreeSet<String> = KNOWN_SHARED_DIRECTORIES.iter().map(|name| (*name).to_owned()).collect();
    for entry in fs::read_dir(&layout.shared).map_err(|_| filesystem("readDirectory", &layout.shared, None))? {
        let name = entry.map_err(|_| filesystem("readDirectory", &layout.shared, None))?.file_name().to_string_lossy().into_owned();
        if !PRIVATE_ENTRY_NAMES.contains(&name.as_str()) && !SHADOW_LOCAL_ENTRY_NAMES.contains(&name.as_str()) {
            entries.insert(name);
        }
    }
    // A private entry that an earlier layout linked to the shared home is unlinked; `auth.json` is checked below instead.
    for name in PRIVATE_ENTRY_NAMES.iter().filter(|name| **name != "auth.json") {
        let path = effective.join(name);
        if matches!(link_state(&path)?, LinkState::Symlink(_)) {
            remove(&path, false)?;
        }
    }
    for name in &entries {
        ensure_symlink(layout, effective, name)?;
    }
    let auth = effective.join("auth.json");
    if matches!(link_state(&auth)?, LinkState::Symlink(_)) {
        return Err(format!("Codex shadow home private entry 'auth.json' at '{}' must be a real file, not a symlink.", auth.to_string_lossy()));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn layout(directory: &Path) -> HomeLayout {
        resolve(&directory.join("shared").to_string_lossy(), &directory.join("shadow").to_string_lossy(), &HashMap::new())
    }

    #[test]
    fn layouts_follow_the_original_rules() {
        let environment = HashMap::from([("HOME".to_owned(), "/home/user".to_owned())]);
        let direct = resolve("", "", &environment);
        assert_eq!((direct.auth_overlay, direct.effective.clone(), direct.continuation_key.as_str()), (false, None, "codex:home:/home/user/.codex"));
        let configured = resolve("~/work/codex", "  ", &environment);
        assert_eq!(configured.effective.as_deref(), Some(Path::new("/home/user/work/codex")));
        let overlay = resolve("~/work/codex", "/shadow", &environment);
        assert_eq!((overlay.auth_overlay, overlay.effective.as_deref(), overlay.continuation_key.as_str()), (true, Some(Path::new("/shadow")), "codex:home:/home/user/work/codex"));
        // A managed instance other than `codex` gets a shadow of its own; the default one shares its home.
        let state = Path::new("/state");
        assert_eq!(resolve_managed(state, "codex", "", "", &environment).auth_overlay, false);
        assert_eq!(resolve_managed(state, "work", "", "", &environment).effective.as_deref(), Some(Path::new("/state/providers/codex/work/shadow")));
        assert_eq!(resolve_managed(state, "work", "", "/mine", &environment).effective.as_deref(), Some(Path::new("/mine")));
    }

    /// The shadow links the shared home's entries and keeps `auth.json` its own; running it again changes nothing, and what blocks it is
    /// reported in the original's words.
    #[test]
    fn a_shadow_home_links_what_is_shared_and_keeps_its_credentials_private() {
        let directory = tempfile::tempdir().unwrap();
        let layout = layout(directory.path());
        fs::create_dir_all(layout.shared.join("rules")).unwrap();
        for private in ["auth.json", "models_cache.json"] {
            fs::write(layout.shared.join(private), "shared").unwrap();
        }
        fs::create_dir_all(layout.shared.join("memories")).unwrap();
        fs::write(layout.shared.join("config.toml"), "model = 1").unwrap();
        materialize(&layout).unwrap();
        let shadow = layout.effective.clone().unwrap();
        for linked in ["sessions", "skills", "mcp-oauth-locks", "rules", "config.toml"] {
            assert_eq!(fs::read_link(shadow.join(linked)).unwrap(), layout.shared.join(linked), "{linked}");
        }
        for own in ["auth.json", "models_cache.json", "memories"] {
            assert!(fs::symlink_metadata(shadow.join(own)).is_err(), "{own} is the shadow's own and not linked");
        }
        // Idempotent; a link to somewhere else is corrected; a models cache that was linked is unlinked.
        materialize(&layout).unwrap();
        fs::remove_file(shadow.join("skills")).unwrap();
        symlink(Path::new("/elsewhere"), &shadow.join("skills")).unwrap();
        symlink(&layout.shared.join("models_cache.json"), &shadow.join("models_cache.json")).unwrap();
        materialize(&layout).unwrap();
        assert_eq!(fs::read_link(shadow.join("skills")).unwrap(), layout.shared.join("skills"));
        assert!(fs::symlink_metadata(shadow.join("models_cache.json")).is_err());

        // A real file where a link belongs stops it, except for the runtime directory that is safe to replace.
        fs::remove_file(shadow.join("config.toml")).unwrap();
        fs::write(shadow.join("config.toml"), "mine").unwrap();
        assert_eq!(materialize(&layout).unwrap_err(), format!("Cannot create Codex shadow home entry 'config.toml' because '{}' already exists and is not a symlink.", shadow.join("config.toml").to_string_lossy()));
        fs::remove_file(shadow.join("config.toml")).unwrap();
        fs::remove_file(shadow.join("mcp-oauth-locks")).unwrap();
        fs::create_dir(shadow.join("mcp-oauth-locks")).unwrap();
        materialize(&layout).unwrap();
        assert!(fs::symlink_metadata(shadow.join("mcp-oauth-locks")).unwrap().file_type().is_symlink());
        // The instance's credentials are never a link into the shared home.
        symlink(&layout.shared.join("auth.json"), &shadow.join("auth.json")).unwrap();
        assert!(materialize(&layout).unwrap_err().contains("private entry 'auth.json'"));
        // A shadow that is the shared home would link the home to itself.
        let same = HomeLayout { effective: Some(layout.shared.clone()), ..layout };
        assert!(materialize(&same).unwrap_err().contains("must be different from the shared home path"));
    }
}
