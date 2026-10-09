//! The project folders the server makes for the user under the T3 home instead of ones the user
//! picks (port of `project/ManagedProjectFolders.ts`):
//!
//! - `<home>/scratch`: the Scratch project ("No project");
//! - `<home>/projects/<slug>`: projects started from just a name, each a new Git repository with a
//!   README, an icon and a first commit.
//!
//! - `<home>/scratch/<date>-<words>-<id>`: the folder of one thread in the Scratch project
//!   (`folderForThread`), claimed by thread launch.
use crate::{
    persistence::{Receipt, Store},
    project::{ProjectCommand, ProjectService},
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};
use t3_vcs::{GitCommand, GitVcs, VcsProcess, process::EnvOverrides};

const MAX_NAMED_FOLDER_ATTEMPTS: usize = 100;

/// Why a managed-folder operation failed. `message` is the original error's user-facing text and
/// is all that crosses the wire; `cause` is for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderError {
    ScratchUnavailable,
    ScratchFolder(String),
    ScratchProject(String),
    NamedFolder(String),
    NamedCreate(String),
}

impl FolderError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::ScratchUnavailable => {
                "Threads without a project are not available on this environment."
            }
            Self::ScratchFolder(_) => "Failed to create the folder for threads without a project.",
            Self::ScratchProject(_) => {
                "Failed to create the project for threads without a project."
            }
            Self::NamedFolder(_) => "Failed to create the project folder.",
            Self::NamedCreate(_) => "Failed to create the project.",
        }
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::ScratchUnavailable => "ScratchUnavailableError",
            Self::ScratchFolder(_) => "ScratchFolderError",
            Self::ScratchProject(_) => "ScratchProjectError",
            Self::NamedFolder(_) => "NamedProjectFolderError",
            Self::NamedCreate(_) => "NamedProjectCreateError",
        }
    }
    /// The `OrchestrationDispatchCommandError` the socket handler wraps every one of them in.
    pub fn wire(&self) -> Value {
        json!({"_tag":"OrchestrationDispatchCommandError","message":self.message()})
    }
}

pub struct ManagedProjectFolders {
    store: Store,
    home: PathBuf,
    git: GitVcs,
    /// Applied to every git command; tests pin the identity and signing settings here.
    git_env: EnvOverrides,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedProject {
    pub project_id: String,
    pub workspace_root: String,
    pub commit_error: Option<String>,
}

impl ManagedProjectFolders {
    /// None for the in-memory store, which has no state directory to derive a T3 home from.
    pub fn for_store(store: &Store) -> Option<Self> {
        let state = store.state_dir()?;
        // The state directory is `<T3 home>/userdata`; one with another name is itself the home.
        let home = match state.file_name() {
            Some(name) if name == "userdata" => state.parent().unwrap_or(state),
            _ => state,
        };
        Some(Self::new(store.clone(), home.to_path_buf()))
    }
    pub fn new(store: Store, home: PathBuf) -> Self {
        Self {
            store,
            home,
            git: GitVcs::new(VcsProcess::new()),
            git_env: Vec::new(),
        }
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_git_env(mut self, env: EnvOverrides) -> Self {
        self.git_env = env;
        self
    }
    pub fn named_projects_root(&self) -> PathBuf {
        self.home.join("projects")
    }

    /// The two paths `server.getConfig` carries (`loadServerConfig`): where a new named project
    /// goes, and the Scratch folder when it is offered. Both are read afresh, since whether the
    /// home sits inside a Git work tree can change. A store without a state directory has no
    /// home, so the snapshot keeps neither.
    pub async fn overlay_config(store: &Store, config: &mut Value) {
        let Some(folders) = Self::for_store(store) else { return };
        let Some(object) = config.as_object_mut() else { return };
        object.insert("newProjectsRoot".into(), json!(folders.named_projects_root()));
        match folders.scratch_root().await {
            Some(root) => object.insert("scratchWorkspaceRoot".into(), json!(root)),
            None => object.remove("scratchWorkspaceRoot"),
        };
    }

    /// The Scratch folder, or None when the home sits inside a Git work tree: there the folder
    /// would inherit the repository's status and checkpoints. A failed probe hides Scratch.
    pub async fn scratch_root(&self) -> Option<PathBuf> {
        match self.git.is_git_repository(&self.home).await {
            Ok(false) => Some(self.home.join("scratch")),
            _ => None,
        }
    }

    /// `folderForThread`: claims a fresh folder for a new thread of the Scratch project, named from
    /// the date, its first message and its id. `None` for every other project, and when Scratch
    /// is not offered. Blocking, because thread launch plans inside a store transaction.
    ///
    /// Thread ids often share a prefix (`thread:...`), so the first name uses the id's tail. A
    /// taken short name falls back to the full id, which only this thread holds. Ids that
    /// normalize alike, or a launch retried without its receipt, can find the full name taken too,
    /// so later attempts add a random suffix: the names never run out. Creating each folder
    /// without `recursive` is the claim, so no two threads or projects ever share one.
    pub fn folder_for_thread(
        &self,
        project_id: &str,
        thread_id: &str,
        text: &str,
        now: chrono::DateTime<Utc>,
    ) -> Result<Option<PathBuf>, FolderError> {
        let Some(root) = self.scratch_root_blocking() else {
            return Ok(None);
        };
        let is_scratch = self
            .store
            .projection("project", project_id)
            .ok()
            .flatten()
            .filter(|project| project["deletedAt"].is_null())
            .and_then(|project| project["workspaceRoot"].as_str().map(|root| crate::vcs_worktree::resolve_path(Path::new(root))))
            .is_some_and(|project_root| project_root == crate::vcs_worktree::resolve_path(&root));
        if !is_scratch {
            return Ok(None);
        }
        let failed = |folder: &Path, error: std::io::Error| {
            FolderError::ScratchFolder(format!("{}: {error}", folder.display()))
        };
        std::fs::create_dir_all(&root).map_err(|error| failed(&root, error))?;
        let date = now.format("%Y-%m-%d").to_string();
        let words = folder_words(text);
        let id: String = thread_id
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            .collect();
        let folder_for = |id_part: &str| {
            let name = [date.as_str(), words.as_str(), id_part]
                .iter()
                .filter(|part| !part.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("-");
            root.join(name)
        };
        let full = folder_for(&id);
        for attempt in 1usize.. {
            let folder = match attempt {
                1 => folder_for(&id[id.len().saturating_sub(8)..]),
                2 => full.clone(),
                _ => {
                    let mut name = full.clone().into_os_string();
                    name.push(format!("-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
                    PathBuf::from(name)
                }
            };
            match std::fs::create_dir(&folder) {
                Ok(()) => return Ok(Some(folder)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(failed(&folder, error)),
            }
        }
        unreachable!("attempts are unbounded")
    }

    /// `scratch_root` for a caller that cannot await. The probe is `git rev-parse
    /// --is-inside-work-tree` in the home; a failed probe hides Scratch.
    fn scratch_root_blocking(&self) -> Option<PathBuf> {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&self.home)
            .stdin(std::process::Stdio::null())
            .output();
        match output {
            // Not a repository, or no home folder yet: Scratch creates it.
            Ok(output) if !output.status.success() => Some(self.home.join("scratch")),
            Ok(output) if String::from_utf8_lossy(&output.stdout).trim() == "false" => Some(self.home.join("scratch")),
            _ => None,
        }
    }

    /// Finds or creates the Scratch project and (re)creates its folder.
    pub async fn ensure_scratch_project(&self) -> Result<String, FolderError> {
        let root = self
            .scratch_root()
            .await
            .ok_or(FolderError::ScratchUnavailable)?;
        // Re-made on every call, so a deleted Scratch folder still runs threads.
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(|error| FolderError::ScratchFolder(error.to_string()))?;
        let workspace_root = root.to_string_lossy().into_owned();
        let failed = |cause: String| FolderError::ScratchProject(cause);
        if let Some(existing) = self.active_project_at(&workspace_root).await {
            return Ok(existing);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let receipt = self
            .dispatch(
                "project.create",
                format!("scratch-project:{id}"),
                &id,
                json!({"title":"No project","workspaceRoot":workspace_root}),
            )
            .await
            .map_err(&failed)?;
        if receipt.status == "rejected" {
            // The create looks the root up and claims it in one transaction, so a racing create
            // loses with a conflict that names the winner.
            let error = receipt.error.unwrap_or(Value::Null);
            return match (error["_tag"].as_str(), error["conflictingProjectId"].as_str()) {
                (Some("ProjectWorkspaceConflictError"), Some(winner)) => Ok(winner.to_owned()),
                _ => Err(failed(error.to_string())),
            };
        }
        // A dashed chat bubble in neutral gray marks Scratch. Set once at create, so a user's own
        // icon choice is never overwritten.
        let icon = self
            .dispatch(
                "project.meta.update",
                format!("scratch-project-icon:{id}"),
                &id,
                json!({"projectIcon":{"kind":"lucide","name":"message-square-dashed","color":"gray"}}),
            )
            .await
            .map_err(&failed)?;
        if icon.status == "rejected" {
            return Err(failed(icon.error.unwrap_or(Value::Null).to_string()));
        }
        Ok(id)
    }

    /// Starts a project from just a name: claims `<projects root>/<slug>` (adding `-2`, `-3`, ...
    /// when taken), makes it a Git repository with a README, an icon and a first commit, then
    /// creates the project. A failed commit (no Git identity, a signing prompt) keeps the project
    /// and returns why in `commit_error`. The folder is removed when the create fails before the
    /// project exists, never once another project owns it.
    pub async fn create_named_project(&self, name: &str) -> Result<NamedProject, FolderError> {
        let folder_error = |cause: String| FolderError::NamedFolder(cause);
        let workspace_root = self.claim_named_folder(name).await?;
        let remove = || async {
            let _ = tokio::fs::remove_dir_all(&workspace_root).await;
        };
        let commit_error = match self.scaffold_repository(&workspace_root, name).await {
            Ok(commit_error) => commit_error,
            Err(cause) => {
                remove().await;
                return Err(folder_error(cause));
            }
        };
        let id = uuid::Uuid::new_v4().to_string();
        let root = workspace_root.to_string_lossy().into_owned();
        let created = self
            .dispatch(
                "project.create",
                format!("named-project:{id}"),
                &id,
                json!({"title":name,"workspaceRoot":root}),
            )
            .await;
        let rejection = match created {
            Ok(receipt) if receipt.status != "rejected" => {
                return Ok(NamedProject {
                    project_id: id,
                    workspace_root: root,
                    commit_error,
                });
            }
            Ok(receipt) => receipt.error.unwrap_or(Value::Null),
            Err(cause) => json!({"detail":cause}),
        };
        // A conflict means another project owns the folder, so it stays. Otherwise the folder is
        // unused, but the owner is checked again first: deleting another project's files is never
        // acceptable.
        if rejection["_tag"] != "ProjectWorkspaceConflictError"
            && self.active_project_at(&root).await.is_none()
        {
            remove().await;
        }
        Err(FolderError::NamedCreate(rejection.to_string()))
    }

    async fn dispatch(
        &self,
        kind: &str,
        command_id: String,
        project_id: &str,
        fields: Value,
    ) -> Result<Receipt, String> {
        let command = ProjectCommand {
            command_type: kind.into(),
            command_id,
            project_id: project_id.into(),
            fields: serde_json::from_value(fields).map_err(|error| error.to_string())?,
        };
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            ProjectService::new(store)
                .dispatch(&command, Utc::now())
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())?
    }

    async fn active_project_at(&self, workspace_root: &str) -> Option<String> {
        let (store, root) = (self.store.clone(), workspace_root.to_owned());
        tokio::task::spawn_blocking(move || {
            store
                .projections("project")
                .ok()?
                .into_iter()
                .find(|row| row["deletedAt"].is_null() && row["workspaceRoot"] == root.as_str())
                .and_then(|row| row["projectId"].as_str().map(str::to_owned))
        })
        .await
        .ok()
        .flatten()
    }

    /// Claims the first free folder among `slug`, `slug-2`, ... Creating each folder without
    /// `recursive` is the claim: of two racers for one name, one gets AlreadyExists and moves on.
    async fn claim_named_folder(&self, name: &str) -> Result<PathBuf, FolderError> {
        let root = self.named_projects_root();
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(|error| FolderError::NamedFolder(error.to_string()))?;
        let slug = new_project_folder_name(name);
        for attempt in 1..=MAX_NAMED_FOLDER_ATTEMPTS {
            let folder = root.join(if attempt == 1 {
                slug.clone()
            } else {
                format!("{slug}-{attempt}")
            });
            match tokio::fs::create_dir(&folder).await {
                Ok(()) => return Ok(folder),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(FolderError::NamedFolder(error.to_string())),
            }
        }
        Err(FolderError::NamedFolder(format!(
            "Every folder name for \"{slug}\" is taken."
        )))
    }

    async fn git(
        &self,
        operation: &str,
        cwd: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> GitCommand {
        let mut command = GitCommand::new(operation, cwd, args.iter().copied()).timeout(timeout);
        command.env = self.git_env.clone();
        command
    }

    /// git init, the starter files, and the first commit. Only the commit may fail softly (its
    /// reason is the Ok value); everything before it fails the create.
    async fn scaffold_repository(
        &self,
        cwd: &Path,
        name: &str,
    ) -> Result<Option<String>, String> {
        let branch = self
            .git
            .core()
            .read_config_value(cwd, "init.defaultBranch")
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "main".into());
        let init = format!("--initial-branch={branch}");
        let secs = Duration::from_secs;
        let run = |command: GitCommand| async {
            self.git
                .execute(command)
                .await
                .map_err(|error| error.to_string())
        };
        run(self
            .git(
                "ManagedProjectFolders.init",
                cwd,
                &["init", &init],
                secs(10),
            )
            .await)
        .await?;
        let io = |error: std::io::Error| error.to_string();
        tokio::fs::write(cwd.join("README.md"), named_project_readme(name))
            .await
            .map_err(io)?;
        tokio::fs::create_dir(cwd.join("assets")).await.map_err(io)?;
        tokio::fs::write(
            cwd.join("assets").join("icon.svg"),
            named_project_icon_svg(name),
        )
        .await
        .map_err(io)?;
        // Named and forced so a global ignore rule (say `*.svg`) cannot drop one.
        run(self
            .git(
                "ManagedProjectFolders.add",
                cwd,
                &["add", "--force", "--", "README.md", "assets/icon.svg"],
                secs(10),
            )
            .await)
        .await?;
        let commit = self
            .git(
                "ManagedProjectFolders.commit",
                cwd,
                &["commit", "--message", "Initial commit"],
                secs(30),
            )
            .await
            .allow_non_zero_exit();
        Ok(match self.git.execute(commit).await {
            Ok(result) if result.exit_code == 0 => None,
            Ok(result) => Some(describe_commit_failure(&result.stderr)),
            Err(error) => Some(error.to_string()),
        })
    }
}

/// The words of a thread's first message that name its folder: only `[a-z0-9]` reaches a folder
/// name, so it stays one path segment, and the words are capped so a pasted blob cannot outgrow
/// a file name.
pub fn folder_words(text: &str) -> String {
    let lowered = text.to_lowercase();
    let joined = lowered
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|word| !word.is_empty())
        .take(5)
        .collect::<Vec<_>>()
        .join("-");
    let cut: String = joined.chars().take(48).collect();
    cut.trim_end_matches('-').to_owned()
}

/// Folder name for a project started from just a name ("Pinball Stats" becomes "pinball-stats").
pub fn new_project_folder_name(name: &str) -> String {
    use icu_normalizer::DecomposingNormalizer;
    let decomposed = DecomposingNormalizer::new_nfkd().normalize(name);
    let lowered: String = decomposed
        .chars()
        .filter(|c| !('\u{300}'..='\u{36f}').contains(c))
        .collect::<String>()
        .to_lowercase();
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in lowered.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c);
        } else {
            pending_dash = true;
        }
    }
    slug.truncate(64);
    let slug = slug.trim_end_matches('-').to_owned();
    if slug.is_empty() {
        return "project".into();
    }
    // Windows refuses these as file names, with or without an extension.
    static RESERVED: OnceLock<regex::Regex> = OnceLock::new();
    let reserved = RESERVED.get_or_init(|| {
        regex::Regex::new(r"^(con|prn|aux|nul|com[1-9]|lpt[1-9])$").expect("reserved names")
    });
    if reserved.is_match(&slug) {
        format!("{slug}-project")
    } else {
        slug
    }
}

// Tailwind 600 shades: dark enough for white initials on every hue.
const ICON_BACKGROUNDS: [&str; 15] = [
    "#dc2626", "#ea580c", "#d97706", "#16a34a", "#059669", "#0d9488", "#0891b2", "#0284c7",
    "#2563eb", "#4f46e5", "#7c3aed", "#9333ea", "#c026d3", "#db2777", "#e11d48",
];

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A rounded square with the name's initials, colored by a hash of the name. It lives at
/// `assets/icon.svg`, a path the favicon resolver already checks, so every machine that clones the
/// project shows the same icon.
pub fn named_project_icon_svg(name: &str) -> String {
    static WORDS: OnceLock<regex::Regex> = OnceLock::new();
    let separator = WORDS.get_or_init(|| regex::Regex::new(r"[^\p{L}\p{N}]+").expect("word split"));
    let first = |word: &str| word.chars().next().map(String::from).unwrap_or_default();
    let initials = separator
        .split(name)
        .filter(|word| !word.is_empty())
        .take(2)
        .map(first)
        .collect::<String>()
        .to_uppercase();
    let initials = if initials.is_empty() {
        let fallback = first(trim_js(name));
        if fallback.is_empty() {
            "?".into()
        } else {
            fallback
        }
    } else {
        initials
    };
    let mut hash = 0u32;
    for c in name.chars() {
        hash = hash.wrapping_mul(31).wrapping_add(c as u32);
    }
    let background = ICON_BACKGROUNDS[hash as usize % ICON_BACKGROUNDS.len()];
    let size = if initials.encode_utf16().count() > 1 { 26 } else { 32 };
    [
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 64 64\">".to_owned(),
        format!("  <rect width=\"64\" height=\"64\" rx=\"14\" fill=\"{background}\"/>"),
        format!("  <text x=\"32\" y=\"32\" dy=\"0.35em\" text-anchor=\"middle\" font-family=\"ui-sans-serif, system-ui, -apple-system, sans-serif\" font-size=\"{size}\" font-weight=\"600\" fill=\"#ffffff\">{}</text>", escape_xml(&initials)),
        "</svg>".to_owned(),
        String::new(),
    ]
    .join("\n")
}

fn trim_js(value: &str) -> &str {
    t3_contracts::trim_wire_string(value)
}

pub fn named_project_readme(name: &str) -> String {
    [
        "<img src=\"assets/icon.svg\" width=\"64\" height=\"64\" alt=\"\">",
        "",
        &format!("# {name}"),
        "",
        "Created in [T3 Code](https://t3.codes).",
        "",
    ]
    .join("\n")
}

/// Git's own identity message runs several lines; say what to do instead. Otherwise its last line
/// ("error: gpg failed to sign the data") says enough.
pub fn describe_commit_failure(stderr: &str) -> String {
    static IDENTITY: OnceLock<regex::Regex> = OnceLock::new();
    let identity = IDENTITY.get_or_init(|| {
        regex::Regex::new(r"(?i)identity unknown|tell me who you are|no (name|email) was given")
            .expect("identity pattern")
    });
    if identity.is_match(stderr) {
        return "Git has no name or email on this machine. Set user.name and user.email, then commit.".into();
    }
    stderr
        .split('\n')
        .map(trim_js)
        .filter(|line| !line.is_empty())
        .next_back()
        .unwrap_or("Git could not make the first commit.")
        .to_owned()
}

#[cfg(test)]
#[path = "project_folders_tests.rs"]
mod tests;
