//! Where `vcs.createWorktree` puts a checkout and how it fills submodules: the
//! `worktreesDirectory` setting (`worktreesDirectory.ts`) and the `worktreeSubmodules` chain of
//! project override, environment setting, the new checkout's `t3.json`, then recursive
//! (`resolveProjectFileBackedSetting`). `t3_vcs` takes both already resolved.
use std::path::{Component, Path, PathBuf};
use t3_contracts::{ServerSettings, WorktreeSubmodules as SettingMode};
use t3_vcs::core::{ExecuteGitInput, GitCore, WorktreeSubmodules};

/// `path.resolve`: absolute, with `.` and `..` folded lexically (symlinks are not consulted).
pub(crate) fn resolve_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                if resolved.parent().is_some() {
                    resolved.pop();
                }
            }
            Component::CurDir => {}
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// `expandHomePathWith`: a leading `~`, `~/` or `~\`; `~user` is left alone.
fn expand_home(value: &str, home: Option<&Path>) -> PathBuf {
    match home {
        Some(home) if value == "~" => home.to_path_buf(),
        Some(home) if value.starts_with("~/") || value.starts_with("~\\") => home.join(&value[2..]),
        _ => PathBuf::from(value),
    }
}

/// Directory new worktrees are created under: the setting, or `default_dir` when it is empty.
/// `None` when the setting is not an absolute path on this machine (a Windows path synced to
/// a Linux server) or is a filesystem root.
pub(crate) fn resolve_worktrees_directory(setting: &str, default_dir: &Path) -> Option<PathBuf> {
    resolve_worktrees_directory_in(setting, default_dir, home_dir().as_deref())
}

pub(crate) fn resolve_worktrees_directory_in(
    setting: &str,
    default_dir: &Path,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if setting.is_empty() {
        return Some(default_dir.to_path_buf());
    }
    let expanded = expand_home(setting, home);
    if !expanded.is_absolute() {
        return None;
    }
    let resolved = resolve_path(&expanded);
    (resolved.parent().is_some()).then_some(resolved)
}

fn setting_mode(mode: SettingMode) -> WorktreeSubmodules {
    match mode {
        SettingMode::Recursive => WorktreeSubmodules::Recursive,
        SettingMode::TopLevel => WorktreeSubmodules::TopLevel,
        SettingMode::None => WorktreeSubmodules::None,
    }
}

/// The project-over-environment `worktreeSubmodules`: `None` defers to the checkout's `t3.json`.
/// A project override of null defers like an unset one (`resolveProjectSettings`).
pub(crate) fn submodule_setting(
    settings: &ServerSettings,
    project_id: Option<&t3_contracts::ProjectId>,
) -> Option<WorktreeSubmodules> {
    let overridden = project_id
        .and_then(|id| settings.project_settings_overrides.get(id))
        .and_then(|overrides| overrides.worktree_submodules);
    match overridden {
        Some(value) => value,
        None => settings.worktree_submodules,
    }
    .map(setting_mode)
}

/// What the checkout's `t3.json` declares that the VCS layer reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectFile {
    pub worktree_submodules: Option<WorktreeSubmodules>,
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// A string that is non-empty once trimmed; `max` bounds the raw length where the schema does.
fn trimmed_non_empty(value: &serde_json::Value, max: Option<usize>) -> bool {
    value.as_str().is_some_and(|text| {
        !text.is_empty()
            && max.is_none_or(|max| utf16_len(text) <= max)
            && !text.trim().is_empty()
            && max.is_none_or(|max| utf16_len(text.trim()) <= max)
    })
}

fn one_of(value: &serde_json::Value, choices: &[&str]) -> bool {
    value.as_str().is_some_and(|text| choices.contains(&text))
}

/// `parseT3ProjectFile`: lenient JSON that must decode as a whole against the `T3ProjectFile`
/// schema, otherwise `None` (an invalid file counts as absent). Only the fields the VCS layer
/// needs are kept; every field is still checked, because one bad field rejects the file.
pub(crate) fn parse_project_file(contents: &str) -> Option<ProjectFile> {
    let value: serde_json::Value =
        serde_json::from_str(&t3_vcs::project_config::strip_lenient_json(contents)).ok()?;
    let file = value.as_object()?;
    let valid =
        |key: &str, check: &dyn Fn(&serde_json::Value) -> bool| file.get(key).is_none_or(check);
    let boolean = |value: &serde_json::Value| value.is_boolean();
    let script_ok = |script: &serde_json::Value| {
        let Some(script) = script.as_object() else {
            return false;
        };
        let field = |key: &str, check: &dyn Fn(&serde_json::Value) -> bool| {
            script.get(key).is_none_or(check)
        };
        script
            .get("name")
            .is_some_and(|v| trimmed_non_empty(v, None))
            && script
                .get("command")
                .is_some_and(|v| trimmed_non_empty(v, None))
            && field("icon", &|v| {
                one_of(v, &["play", "test", "lint", "configure", "build", "debug"])
            })
            && field("runOnWorktreeCreate", &boolean)
            && field("runOnSettle", &boolean)
            && field("async", &boolean)
            && field("previewUrl", &|v| trimmed_non_empty(v, None))
            && field("autoOpenPreview", &boolean)
    };
    let modes = ["recursive", "top-level", "none"];
    let ok = valid("$schema", &|v| v.is_string())
        && valid("iconPath", &|v| trimmed_non_empty(v, Some(512)))
        && valid("defaultThreadEnvMode", &|v| {
            one_of(v, &["local", "worktree"])
        })
        && valid("worktreeSubmodules", &|v| one_of(v, &modes))
        && valid("scripts", &|v| {
            v.as_array()
                .is_some_and(|scripts| scripts.len() <= 50 && scripts.iter().all(script_ok))
        });
    ok.then(|| ProjectFile {
        worktree_submodules: file
            .get("worktreeSubmodules")
            .and_then(|v| match v.as_str()? {
                "recursive" => Some(WorktreeSubmodules::Recursive),
                "top-level" => Some(WorktreeSubmodules::TopLevel),
                _ => Some(WorktreeSubmodules::None),
            }),
    })
}

/// Initialises submodules in a worktree `create_worktree` left empty, for the case where no
/// setting decided and the new checkout's own `t3.json` has to. Best effort, like the
/// original: a failure leaves the submodule paths empty and does not fail the caller.
pub(crate) async fn populate_submodules_from_project_file(core: &GitCore, worktree: &Path) {
    if !worktree.join(".gitmodules").try_exists().unwrap_or(false) {
        return;
    }
    let file = match std::fs::read_to_string(worktree.join("t3.json")) {
        Ok(contents) => {
            let parsed = parse_project_file(&contents);
            if parsed.is_none() {
                tracing::warn!(worktree=%worktree.display(),"t3.json is invalid; initializing submodules recursively");
            }
            parsed
        }
        Err(_) => None,
    };
    let mode = file
        .and_then(|file| file.worktree_submodules)
        .unwrap_or(WorktreeSubmodules::Recursive);
    if mode == WorktreeSubmodules::None {
        return;
    }
    let mut args = vec!["submodule".to_string(), "update".into(), "--init".into()];
    if mode == WorktreeSubmodules::Recursive {
        args.push("--recursive".into());
    }
    let result = core
        .execute(ExecuteGitInput {
            operation: "GitVcsDriver.createWorktree.updateSubmodules".into(),
            cwd: worktree.to_path_buf(),
            args,
            ..ExecuteGitInput::default()
        })
        .await;
    if let Err(error) = result {
        tracing::warn!(worktree=%worktree.display(),error=%error,"worktree submodule checkout failed; submodule paths are empty");
    }
}

/// `<T3 home>/worktrees` for a store whose state directory is `<T3 home>/userdata` (a state
/// directory with any other name is itself the home), as `VcsContext::default_worktrees_dir`.
pub(crate) fn default_worktrees_directory(state_dir: Option<&Path>) -> PathBuf {
    let home = match state_dir {
        Some(dir) if dir.file_name().is_some_and(|name| name == "userdata") => dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.to_path_buf()),
        Some(dir) => dir.to_path_buf(),
        None => std::env::temp_dir().join("t3-home"),
    };
    home.join("worktrees")
}

/// What a thread launch does for `vcs.createWorktree`: the checkout goes under the
/// `worktreesDirectory` setting, submodules follow the project and environment setting, and the
/// new checkout's `t3.json` decides when neither says. `progress` reports the claimed path and
/// git's checkout percentage to the worktree setup card.
pub(crate) async fn create_for_launch(
    core: &GitCore,
    settings: &ServerSettings,
    project: Option<&t3_contracts::ProjectId>,
    state_dir: Option<&Path>,
    input: &t3_vcs::core::VcsCreateWorktreeInput,
    progress: Option<t3_vcs::core::CreateWorktreeProgress>,
) -> Result<t3_vcs::core::VcsWorktree, t3_vcs::GitCommandError> {
    let setting = submodule_setting(settings, project);
    let worktrees_setting = settings.worktrees_directory.as_str();
    let default_dir = default_worktrees_directory(state_dir);
    let worktrees_directory = if input.path.is_some() {
        default_dir
    } else {
        resolve_worktrees_directory(worktrees_setting, &default_dir).ok_or_else(|| {
            t3_vcs::GitCommandError {
                operation: "GitVcsDriver.createWorktree".into(),
                command: "git worktree add".into(),
                cwd: input.cwd.to_string_lossy().into_owned(),
                argument_count: None,
                exit_code: None,
                stdout_length: None,
                stderr_length: None,
                output_length: None,
                reason: None,
                detail: format!("The worktree location \"{worktrees_setting}\" must be an absolute folder on this machine, not a drive root. Change it in Settings → Storage."),
                cause: None,
            }
        })?
    };
    let worktree = core
        .create_worktree(
            input,
            &t3_vcs::core::CreateWorktreeOptions {
                worktrees_directory,
                submodules: setting.unwrap_or(WorktreeSubmodules::None),
                progress,
            },
        )
        .await?;
    if setting.is_none() {
        populate_submodules_from_project_file(core, &worktree.path).await;
    }
    Ok(worktree)
}
