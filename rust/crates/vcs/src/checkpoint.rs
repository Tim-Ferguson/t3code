//! Hidden-ref workspace checkpoints (port of `checkpointing/*` and the `checkpoints` half of
//! `vcs/GitVcsDriver.ts`). Ref names and git command sequences match the TypeScript server,
//! because repositories already hold refs it wrote.
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use regex::Regex;

use crate::{
    error::{VcsDriverKind, VcsError},
    git::{
        GitCommand, GitVcs, PATCH_RENDER_PREFIX_ARGS, WORKSPACE_FILES_MAX_OUTPUT_BYTES,
        split_null_separated_paths,
    },
    process::{CHECKPOINT_CAPTURE_OPERATION, EnvOverrides, OutputMode, VcsProcessInput},
};

const CHECKPOINT_REFS_PREFIX: &str = "refs/t3/checkpoints";
const CHECKPOINT_RECOVERY_MAX_CANDIDATES: usize = 64;
const CHECKPOINT_DIFF_MAX_OUTPUT_BYTES: usize = 10_000_000;
const INDEX_INSPECTION_MAX_OUTPUT_BYTES: usize = 4_096;

const COMMIT_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "T3 Code"),
    ("GIT_AUTHOR_EMAIL", "t3code@users.noreply.github.com"),
    ("GIT_COMMITTER_NAME", "T3 Code"),
    ("GIT_COMMITTER_EMAIL", "t3code@users.noreply.github.com"),
];
// Variables that would make a nested repository probe inherit this checkout's bindings.
const NESTED_REPOSITORY_UNSET: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

// Git renames loose objects and refs into place without fsync by default, so an unclean
// restart can leave 0-byte files under refs/t3/** that break every later fetch and push.
// Checkpoint writes flush before they are published; macOS defaults to writeout-only, which
// does not reach the disk either.
const DURABLE_WRITE: [&str; 4] = [
    "-c",
    "core.fsync=objects,reference",
    "-c",
    "core.fsyncMethod=fsync",
];
const INDEX_CONFIG: [&str; 4] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "sparse.expectFilesOutsideOfPatterns=false",
];

/// `refs/t3/checkpoints/<base64url(threadId)>/turn/<turnCount>`.
pub fn checkpoint_ref_for_thread_turn(thread_id: &str, turn_count: u64) -> String {
    format!(
        "{CHECKPOINT_REFS_PREFIX}/{}/turn/{turn_count}",
        URL_SAFE_NO_PAD.encode(thread_id)
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnDiffFileSummary {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

/// Reads git's NUL-delimited `--numstat -z` output without decoding display paths. Files are
/// sorted with the process locale, as the original's `localeCompare` does.
pub fn parse_turn_diff_files_from_numstat(numstat: &str) -> Vec<TurnDiffFileSummary> {
    let collator = process_locale_collator();
    parse_numstat(numstat, |left, right| collator.compare(left, right))
}

/// `String.prototype.localeCompare` with the process locale.
pub(crate) fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    process_locale_collator().compare(left, right)
}

fn process_locale_collator() -> &'static icu_collator::CollatorBorrowed<'static> {
    static COLLATOR: OnceLock<icu_collator::CollatorBorrowed<'static>> = OnceLock::new();
    COLLATOR.get_or_init(|| {
        icu_collator::Collator::try_new(locale_from_environment().into(), Default::default())
            .expect("compiled ICU collation data")
    })
}

// POSIX ICU locale precedence, with Node's en-US fallback for C/POSIX.
fn locale_from_environment() -> icu_locale::Locale {
    let name = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| "en-US".into());
    let name = name.split(['.', '@']).next().unwrap().replace('_', "-");
    if matches!(name.as_str(), "C" | "POSIX") {
        return "en-US".parse().unwrap();
    }
    name.parse().unwrap_or_else(|_| "en-US".parse().unwrap())
}

/// Exposed to the fixture test, which fixes the locale instead of reading the environment.
#[doc(hidden)]
pub fn parse_numstat_with_locale(numstat: &str, locale: &str) -> Vec<TurnDiffFileSummary> {
    let collator = icu_collator::Collator::try_new(
        locale.parse::<icu_locale::Locale>().unwrap().into(),
        Default::default(),
    )
    .expect("compiled ICU collation data");
    parse_numstat(numstat, |left, right| collator.compare(left, right))
}

fn parse_numstat(
    numstat: &str,
    compare: impl Fn(&str, &str) -> std::cmp::Ordering,
) -> Vec<TurnDiffFileSummary> {
    static COUNTS: OnceLock<Regex> = OnceLock::new();
    let counts_pattern = COUNTS.get_or_init(|| Regex::new(r"^(\d+|-)\t(\d+|-)\t").unwrap());
    let records: Vec<&str> = numstat.split('\0').collect();
    let mut files = Vec::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        let Some(counts) = counts_pattern.captures(record) else {
            index += 1;
            continue;
        };
        let mut path = &record[counts.get(0).unwrap().end()..];
        if path.is_empty() {
            // Renames and copies use two more records: the source and destination.
            path = records.get(index + 2).copied().unwrap_or("");
            index += 2;
        }
        index += 1;
        if path.is_empty() {
            continue;
        }
        let count = |text: &str| {
            if text == "-" {
                0
            } else {
                text.parse().unwrap_or(0)
            }
        };
        files.push(TurnDiffFileSummary {
            path: path.to_string(),
            additions: count(&counts[1]),
            deletions: count(&counts[2]),
        });
    }
    files.sort_by(|left, right| compare(&left.path, &right.path));
    files
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffFormat {
    #[default]
    Patch,
    /// `--numstat -z`, parsed by `parse_turn_diff_files_from_numstat`.
    Numstat,
}

#[derive(Debug, Clone)]
pub struct DiffCheckpointsInput {
    pub cwd: PathBuf,
    pub from_checkpoint_ref: String,
    pub to_checkpoint_ref: String,
    /// Diff from `HEAD` when the "from" ref does not exist.
    pub fallback_from_to_head: bool,
    pub ignore_whitespace: bool,
    pub format: DiffFormat,
}

fn operation(name: &str) -> String {
    format!("GitVcsDriver.checkpoints.{name}")
}

fn env_set(pairs: impl IntoIterator<Item = (&'static str, String)>) -> EnvOverrides {
    pairs
        .into_iter()
        .map(|(name, value)| (name.to_string(), Some(value)))
        .collect()
}

/// Removes Git's private index (and a lock a forced termination left behind) however the
/// capture ends, including when its future is dropped.
struct TempIndex(PathBuf);

impl TempIndex {
    fn remove(&self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Any failure while seeding the private index only means "build it from HEAD instead".
struct ReuseFailed;

impl From<VcsError> for ReuseFailed {
    fn from(_: VcsError) -> Self {
        Self
    }
}

impl From<std::io::Error> for ReuseFailed {
    fn from(_: std::io::Error) -> Self {
        Self
    }
}

/// How the recovery attempt ended when it did not produce a staged index.
enum Recovery {
    /// Fail with the staging error that started recovery.
    Original,
    Failed(VcsError),
}

impl From<VcsError> for Recovery {
    fn from(error: VcsError) -> Self {
        Self::Failed(error)
    }
}

impl GitVcs {
    async fn resolve_commit(
        &self,
        operation_name: &str,
        cwd: &Path,
        revision: &str,
    ) -> Result<Option<String>, VcsError> {
        let result = self
            .execute(
                GitCommand::new(
                    operation(operation_name),
                    cwd,
                    [
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("{revision}^{{commit}}"),
                    ],
                )
                .allow_non_zero_exit(),
            )
            .await?;
        let commit = result.stdout.trim();
        Ok((result.exit_code == 0 && !commit.is_empty()).then(|| commit.to_string()))
    }

    async fn resolve_head_commit(&self, cwd: &Path) -> Result<Option<String>, VcsError> {
        self.resolve_commit("resolveHeadCommit", cwd, "HEAD").await
    }

    async fn resolve_checkpoint_commit(
        &self,
        cwd: &Path,
        checkpoint_ref: &str,
    ) -> Result<Option<String>, VcsError> {
        self.resolve_commit("resolveCheckpointCommit", cwd, checkpoint_ref)
            .await
    }

    async fn has_head_commit(&self, cwd: &Path, env: EnvOverrides) -> Result<bool, VcsError> {
        let result = self
            .execute(
                GitCommand::new(
                    operation("hasHeadCommit"),
                    cwd,
                    ["rev-parse", "--verify", "HEAD"],
                )
                .allow_non_zero_exit()
                .env(env),
            )
            .await?;
        Ok(result.exit_code == 0)
    }

    async fn resolve_git_common_dir(&self, cwd: &Path) -> Result<PathBuf, VcsError> {
        let result = self
            .execute(GitCommand::new(
                operation("resolveGitCommonDir"),
                cwd,
                ["rev-parse", "--git-common-dir"],
            ))
            .await?;
        // `join` keeps an absolute answer as it is and anchors a relative one at `cwd`.
        Ok(cwd.join(result.stdout.trim()))
    }

    pub async fn has_checkpoint_ref(
        &self,
        cwd: &Path,
        checkpoint_ref: &str,
    ) -> Result<bool, VcsError> {
        Ok(self
            .resolve_checkpoint_commit(cwd, checkpoint_ref)
            .await?
            .is_some())
    }

    /// Snapshots the workspace (tracked, staged and unignored untracked files) as a commit at
    /// `checkpoint_ref`, using a private index so the user's index is never touched.
    pub async fn capture_checkpoint(
        &self,
        cwd: &Path,
        checkpoint_ref: &str,
    ) -> Result<(), VcsError> {
        let git_common_dir = self.resolve_git_common_dir(cwd).await?;
        let index_id = uuid::Uuid::new_v4();
        let temp_index = TempIndex(git_common_dir.join(format!("t3-checkpoint-index-{index_id}")));
        let mut commit_env = env_set([(
            "GIT_INDEX_FILE",
            temp_index.0.to_string_lossy().into_owned(),
        )]);
        commit_env.extend(
            COMMIT_IDENTITY.map(|(name, value)| (name.to_string(), Some(value.to_string()))),
        );

        let capture = CheckpointCapture {
            git: self,
            cwd,
            commit_env,
            temp_index: &temp_index,
        };
        capture.run(checkpoint_ref).await
    }

    /// Restores workspace and staging state to a checkpoint. Returns `false` when the ref is
    /// missing (and `fallback_to_head` is unset or `HEAD` does not exist either).
    pub async fn restore_checkpoint(
        &self,
        cwd: &Path,
        checkpoint_ref: &str,
        fallback_to_head: bool,
    ) -> Result<bool, VcsError> {
        let operation_name = operation("restoreCheckpoint");
        let mut commit = self.resolve_checkpoint_commit(cwd, checkpoint_ref).await?;
        if commit.is_none() && fallback_to_head {
            commit = self.resolve_head_commit(cwd).await?;
        }
        let Some(commit) = commit else {
            return Ok(false);
        };

        let tracked = self
            .execute(GitCommand::new(
                &operation_name,
                cwd,
                [
                    "ls-files",
                    "--cached",
                    &format!("--with-tree={commit}"),
                    "-z",
                    "--",
                    ".",
                ],
            ))
            .await?;
        // An empty index and checkpoint have nothing for git restore's pathspec to match.
        if !tracked.stdout.is_empty() {
            self.execute(GitCommand::new(
                &operation_name,
                cwd,
                [
                    "restore",
                    "--source",
                    &commit,
                    "--worktree",
                    "--staged",
                    "--",
                    ".",
                ],
            ))
            .await?;
        }
        // Restoring away the last tracked file can remove a nested workspace directory.
        if let Err(cause) = tokio::fs::create_dir_all(cwd).await {
            return Err(VcsError::exit(
                &operation_name,
                "git restore",
                &cwd.to_string_lossy(),
                0,
                format!("Could not recreate the checkpoint workspace: {cause}"),
            ));
        }
        let cleaned = self
            .execute(
                GitCommand::new(&operation_name, cwd, ["clean", "-fd", "--", "."])
                    .allow_non_zero_exit(),
            )
            .await?;
        if cleaned.exit_code != 0 {
            // Git can remove every child, then fail trying to remove './' itself.
            static EMPTIED: OnceLock<Regex> = OnceLock::new();
            let emptied = EMPTIED
                .get_or_init(|| Regex::new(r"^warning: failed to remove \./: [^\n]+$").unwrap());
            let emptied_workspace = cleaned.exit_code == 1
                && emptied.is_match(cleaned.stderr.trim())
                && match tokio::fs::read_dir(cwd).await {
                    Ok(mut entries) => matches!(entries.next_entry().await, Ok(None)),
                    Err(_) => false,
                };
            if !emptied_workspace {
                let stderr = cleaned.stderr.trim();
                return Err(VcsError::exit(
                    &operation_name,
                    "git clean",
                    &cwd.to_string_lossy(),
                    cleaned.exit_code,
                    if stderr.is_empty() {
                        "Could not clean the checkpoint workspace."
                    } else {
                        stderr
                    },
                ));
            }
        }

        if self.has_head_commit(cwd, Vec::new()).await? {
            self.execute(GitCommand::new(
                &operation_name,
                cwd,
                ["reset", "--quiet", "--", "."],
            ))
            .await?;
        }
        Ok(true)
    }

    /// Diff text between two checkpoint refs: a patch, or NUL-delimited numstat.
    pub async fn diff_checkpoints(&self, input: &DiffCheckpointsInput) -> Result<String, VcsError> {
        let operation_name = operation("diffCheckpoints");
        let cwd = input.cwd.as_path();
        let mut from_revision = input.from_checkpoint_ref.clone();
        if input.fallback_from_to_head {
            match self
                .resolve_checkpoint_commit(cwd, &input.from_checkpoint_ref)
                .await?
            {
                Some(commit) => from_revision = commit,
                None => {
                    from_revision = self.resolve_head_commit(cwd).await?.ok_or_else(|| {
                        VcsError::exit(
                            &operation_name,
                            "git diff",
                            &cwd.to_string_lossy(),
                            1,
                            "Checkpoint ref is unavailable for diff operation.",
                        )
                    })?;
                }
            }
        }

        let numstat = input.format == DiffFormat::Numstat;
        let mut args: Vec<String> = vec!["diff".into()];
        args.extend(if numstat {
            vec!["--numstat".into(), "-z".into()]
        } else {
            vec!["--patch".into()]
        });
        args.extend(["--no-color", "--no-ext-diff", "--no-textconv"].map(String::from));
        args.extend(PATCH_RENDER_PREFIX_ARGS.map(String::from));
        if input.ignore_whitespace {
            args.push("--ignore-all-space".into());
        }
        args.push(format!("{from_revision}^{{commit}}"));
        args.push(format!("{}^{{commit}}", input.to_checkpoint_ref));

        let result = self
            .execute(
                GitCommand::new(&operation_name, cwd, args)
                    .allow_non_zero_exit()
                    .max_output_bytes(CHECKPOINT_DIFF_MAX_OUTPUT_BYTES)
                    .output_mode(if numstat {
                        OutputMode::Error
                    } else {
                        OutputMode::Truncate
                    }),
            )
            .await?;
        if result.exit_code != 0 {
            let stderr = result.stderr.trim();
            return Err(VcsError::exit(
                &operation_name,
                "git diff",
                &cwd.to_string_lossy(),
                result.exit_code,
                if stderr.is_empty() {
                    "Checkpoint ref is unavailable for diff operation."
                } else {
                    stderr
                },
            ));
        }
        Ok(result.stdout)
    }

    /// Best-effort delete: refs that do not exist are tolerated.
    pub async fn delete_checkpoint_refs(
        &self,
        cwd: &Path,
        checkpoint_refs: &[String],
    ) -> Result<(), VcsError> {
        for checkpoint_ref in checkpoint_refs {
            self.execute(
                GitCommand::new(
                    operation("deleteCheckpointRefs"),
                    cwd,
                    ["update-ref", "-d", checkpoint_ref],
                )
                .allow_non_zero_exit(),
            )
            .await?;
        }
        Ok(())
    }
}

/// State of the one-pass `git ls-files --sparse -v -z` inspection of the copied index.
#[derive(Default)]
struct IndexScan {
    special_flags: bool,
    record_start: bool,
    skipped: bool,
    skipped_record: Vec<u8>,
    skipped_paths: Vec<String>,
}

impl IndexScan {
    fn new() -> Self {
        Self {
            record_start: true,
            ..Self::default()
        }
    }

    /// Looks at every record's tag byte (`S` skip-worktree, lowercase for assume-unchanged and
    /// friends) and keeps only the paths of skipped files, across arbitrary chunk boundaries.
    fn feed(&mut self, chunk: &[u8], sparse_checkout: bool) {
        for &byte in chunk {
            if self.record_start {
                self.skipped = byte == b'S';
            }
            if self.skipped && sparse_checkout {
                if byte != 0 {
                    self.skipped_record.push(byte);
                } else {
                    if self.skipped_record.last() != Some(&b'/') {
                        let name = self.skipped_record.get(2..).unwrap_or(&[]);
                        match std::str::from_utf8(name) {
                            Ok(name) => self.skipped_paths.push(name.to_string()),
                            Err(_) => self.special_flags = true,
                        }
                    }
                    self.skipped_record.clear();
                }
            }
            if self.record_start
                && (byte.is_ascii_lowercase() || (!sparse_checkout && byte == b'S'))
            {
                self.special_flags = true;
            }
            self.record_start = byte == 0;
        }
    }
}

struct CheckpointCapture<'a> {
    git: &'a GitVcs,
    cwd: &'a Path,
    commit_env: EnvOverrides,
    temp_index: &'a TempIndex,
}

impl CheckpointCapture<'_> {
    /// A command of the capture operation, so `VcsProcess` retries transient lock failures.
    fn command(&self, args: impl IntoIterator<Item = impl Into<String>>) -> GitCommand {
        GitCommand::new(CHECKPOINT_CAPTURE_OPERATION, self.cwd, args)
    }

    fn with_index_env(&self, command: GitCommand) -> GitCommand {
        command.env(self.commit_env.clone())
    }

    async fn run(&self, checkpoint_ref: &str) -> Result<(), VcsError> {
        let git = self.git;
        let cwd = self.cwd;
        let cwd_text = cwd.to_string_lossy().into_owned();

        let head_exists = git.has_head_commit(cwd, Vec::new()).await?;
        let sparse_config = git
            .execute(
                self.command(["config", "--bool", "core.sparseCheckout"])
                    .allow_non_zero_exit(),
            )
            .await?;
        let mut sparse_checkout = sparse_config.stdout.trim() == "true";
        if sparse_checkout {
            let help = git
                .execute(self.command(["add", "-h"]).allow_non_zero_exit())
                .await?;
            static SPARSE_FLAG: OnceLock<Regex> = OnceLock::new();
            let flag = SPARSE_FLAG.get_or_init(|| Regex::new(r"--(?:\[no-\])?sparse\b").unwrap());
            sparse_checkout = flag.is_match(&format!("{}{}", help.stdout, help.stderr));
        }

        if head_exists {
            let reused = self
                .reuse_user_index(sparse_checkout)
                .await
                .unwrap_or(false);
            if !reused {
                if sparse_checkout {
                    let cone = git
                        .execute(
                            self.command(["config", "--bool", "core.sparseCheckoutCone"])
                                .allow_non_zero_exit(),
                        )
                        .await?;
                    // Rebuilding a non-cone index loses exclusions; do not publish false deletions.
                    if cone.stdout.trim() != "true" {
                        return Err(VcsError::exit(
                            CHECKPOINT_CAPTURE_OPERATION,
                            "git read-tree",
                            &cwd_text,
                            1,
                            "Cannot rebuild a checkpoint index for non-cone sparse checkout.",
                        ));
                    }
                }
                self.temp_index.remove();
                // A fresh sparse index represents excluded directories without marking them deleted.
                let args: Vec<String> = if sparse_checkout {
                    INDEX_CONFIG
                        .into_iter()
                        .chain(["-c", "index.sparse=true", "read-tree", "--reset", "HEAD"])
                        .map(String::from)
                        .collect()
                } else {
                    ["read-tree", "HEAD"].map(String::from).into()
                };
                git.execute(self.with_index_env(self.command(args))).await?;
            }
        }

        match self.stage_files(sparse_checkout, &[]).await {
            Ok(()) => {}
            Err(error @ VcsError::ProcessExit { .. }) => {
                // Git cannot stage an embedded repository until it has a commit. Discover these
                // only after staging fails so ordinary checkpoints do not need another file scan.
                // One budget covers discovery, queued Git admission, probes, and the retry.
                let recovery = tokio::time::timeout(
                    git.checkpoint_recovery_timeout,
                    self.recover_nested_repositories(sparse_checkout),
                )
                .await;
                match recovery {
                    Ok(Ok(())) => {}
                    Ok(Err(Recovery::Failed(other))) => return Err(other),
                    Ok(Err(Recovery::Original)) | Err(_) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }

        let write_tree = git
            .execute(
                self.with_index_env(
                    self.command(
                        INDEX_CONFIG
                            .into_iter()
                            .chain(DURABLE_WRITE)
                            .chain(["write-tree"]),
                    ),
                ),
            )
            .await?;
        let tree_oid = write_tree.stdout.trim();
        if tree_oid.is_empty() {
            return Err(VcsError::exit(
                CHECKPOINT_CAPTURE_OPERATION,
                "git write-tree",
                &cwd_text,
                0,
                "git write-tree returned an empty tree oid.",
            ));
        }

        let message = format!("t3 checkpoint ref={checkpoint_ref}");
        let commit_tree = git
            .execute(
                self.with_index_env(self.command(DURABLE_WRITE.into_iter().chain([
                    "commit-tree",
                    tree_oid,
                    "-m",
                    &message,
                ]))),
            )
            .await?;
        let commit_oid = commit_tree.stdout.trim();
        if commit_oid.is_empty() {
            return Err(VcsError::exit(
                CHECKPOINT_CAPTURE_OPERATION,
                "git commit-tree",
                &cwd_text,
                0,
                "git commit-tree returned an empty commit oid.",
            ));
        }

        git.execute(self.command(DURABLE_WRITE.into_iter().chain([
            "update-ref",
            checkpoint_ref,
            commit_oid,
        ])))
        .await?;
        Ok(())
    }

    /// Seeds the private index from the user's, so git can skip re-hashing unchanged files.
    /// Returns whether the copy is safe to use; any failure means "rebuild from HEAD".
    async fn reuse_user_index(&self, sparse_checkout: bool) -> Result<bool, ReuseFailed> {
        let git = self.git;
        let user_index = git
            .execute(self.command(["rev-parse", "--path-format=absolute", "--git-path", "index"]))
            .await?;
        let user_index = PathBuf::from(user_index.stdout.trim());
        let Some(index_time) = index_time_seconds(&user_index) else {
            return Ok(false);
        };
        tokio::fs::copy(&user_index, &self.temp_index.0).await?;

        // Retain stat data only where the copied index already matches HEAD.
        git.execute(
            self.with_index_env(self.command(INDEX_CONFIG.into_iter().chain([
                "read-tree",
                "--reset",
                "HEAD",
            ]))),
        )
        .await?;
        // read-tree can rewrite the index, so restore its racy timestamp afterward.
        set_file_times(&self.temp_index.0, index_time)?;

        let scan = Arc::new(Mutex::new(IndexScan::new()));
        let mut input = VcsProcessInput::new(
            CHECKPOINT_CAPTURE_OPERATION,
            "git",
            INDEX_CONFIG
                .into_iter()
                .chain(["ls-files", "--full-name", "--sparse", "-v", "-z"]),
            self.cwd,
        );
        input.env = self.commit_env.clone();
        input.max_output_bytes = Some(INDEX_INSPECTION_MAX_OUTPUT_BYTES);
        input.output_mode = Some(OutputMode::Truncate);
        // Inspect every tag; retain only skipped file paths for checking sparse rules.
        let sink = scan.clone();
        input.on_stdout_chunk = Some(Arc::new(move |chunk| {
            sink.lock().unwrap().feed(chunk, sparse_checkout)
        }));
        git.process.run(input).await?;

        let (mut special_flags, skipped_paths) = {
            let mut scan = scan.lock().unwrap();
            (scan.special_flags, std::mem::take(&mut scan.skipped_paths))
        };
        if !skipped_paths.is_empty() && !special_flags {
            let mut stdin = skipped_paths.join("\0");
            stdin.push('\0');
            let selected = git
                .execute(
                    self.with_index_env(self.command(INDEX_CONFIG.into_iter().chain([
                        "sparse-checkout",
                        "check-rules",
                        "-z",
                    ])))
                    .stdin(stdin)
                    .max_output_bytes(1)
                    .output_mode(OutputMode::Truncate),
                )
                .await?;
            // Any selected skipped file has a manual flag, not a sparse exclusion.
            special_flags = !selected.stdout.is_empty() || selected.stdout_truncated;
        }
        // Sparse Git clears skip-worktree for present files. Manual flags still need a reset.
        Ok(!special_flags)
    }

    async fn stage_files(
        &self,
        sparse_checkout: bool,
        exclusions: &[String],
    ) -> Result<(), VcsError> {
        // Preserve absent skipped entries, but capture present nonignored files outside the cone.
        let mut args: Vec<&str> = INDEX_CONFIG.into_iter().chain(DURABLE_WRITE).collect();
        args.push("add");
        if sparse_checkout {
            args.push("--sparse");
        }
        args.extend(["-A", "--", "."]);
        args.extend(exclusions.iter().map(String::as_str));
        self.git
            .execute(self.with_index_env(self.command(args)))
            .await
            .map(|_| ())
    }

    async fn recover_nested_repositories(&self, sparse_checkout: bool) -> Result<(), Recovery> {
        let untracked = self
            .git
            .execute(
                self.with_index_env(self.command([
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                    "-z",
                    "--",
                    ".",
                ]))
                .max_output_bytes(WORKSPACE_FILES_MAX_OUTPUT_BYTES),
            )
            .await?;
        if untracked.stdout_truncated {
            return Err(Recovery::Original);
        }
        let candidates: Vec<String> =
            split_null_separated_paths(&untracked.stdout, untracked.stdout_truncated)
                .into_iter()
                .filter(|entry| entry.ends_with('/'))
                .collect();
        // Refuse excessive recovery work before probing any nested repositories.
        if candidates.len() > CHECKPOINT_RECOVERY_MAX_CANDIDATES {
            return Err(Recovery::Original);
        }
        // Discover each child's repository instead of inheriting the server's Git bindings.
        let nested_env: EnvOverrides = NESTED_REPOSITORY_UNSET
            .map(|name| (name.to_string(), None))
            .into();
        let mut exclusions = Vec::new();
        for entry in candidates {
            let nested_cwd = self.cwd.join(&entry);
            let has_git_entry = tokio::fs::try_exists(nested_cwd.join(".git"))
                .await
                .map_err(|_| Recovery::Original)?;
            if has_git_entry
                && !self
                    .git
                    .has_head_commit(&nested_cwd, nested_env.clone())
                    .await?
            {
                exclusions.push(format!(":(exclude,literal){entry}"));
            }
        }
        if exclusions.is_empty() {
            return Err(Recovery::Original);
        }
        Ok(self.stage_files(sparse_checkout, &exclusions).await?)
    }
}

/// Whole seconds strictly below the index's modification time, so Git's racy-timestamp check
/// still treats entries written in that second as suspect. `None` when there is no such time.
fn index_time_seconds(index: &Path) -> Option<u64> {
    let modified = std::fs::metadata(index).ok()?.modified().ok()?;
    let millis = modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
    // Stay below the source timestamp even if the clock rounded up.
    let seconds = millis.saturating_sub(1) / 1000;
    (seconds > 0).then_some(seconds)
}

fn set_file_times(path: &Path, seconds: u64) -> std::io::Result<()> {
    let time = UNIX_EPOCH + Duration::from_secs(seconds);
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(time)
            .set_modified(time),
    )
}

/// What `CheckpointStore.resolveCheckpoints` does before every operation: refuse a directory
/// that is not inside a git work tree with the registry's unsupported-operation error.
pub async fn ensure_git_work_tree(
    git: &GitVcs,
    operation: &str,
    cwd: &Path,
) -> Result<(), VcsError> {
    if git.is_inside_work_tree(cwd).await? {
        return Ok(());
    }
    Err(VcsError::UnsupportedOperation {
        operation: operation.to_string(),
        kind: VcsDriverKind::Unknown,
        detail: format!(
            "No supported VCS repository was detected at {}.",
            cwd.display()
        ),
    })
}

/// Checkpoint operations behind the repository check `CheckpointStore.ts` performs.
#[derive(Clone)]
pub struct CheckpointStore {
    git: GitVcs,
}

impl CheckpointStore {
    pub fn new(git: GitVcs) -> Self {
        Self { git }
    }

    pub async fn is_git_repository(&self, cwd: &Path) -> Result<bool, VcsError> {
        self.git.is_git_repository(cwd).await
    }

    pub async fn capture_checkpoint(
        &self,
        cwd: &Path,
        checkpoint_ref: &str,
    ) -> Result<(), VcsError> {
        ensure_git_work_tree(&self.git, "CheckpointStore.captureCheckpoint", cwd).await?;
        self.git.capture_checkpoint(cwd, checkpoint_ref).await
    }

    pub async fn has_checkpoint_ref(
        &self,
        cwd: &Path,
        checkpoint_ref: &str,
    ) -> Result<bool, VcsError> {
        ensure_git_work_tree(&self.git, "CheckpointStore.hasCheckpointRef", cwd).await?;
        self.git.has_checkpoint_ref(cwd, checkpoint_ref).await
    }

    pub async fn restore_checkpoint(
        &self,
        cwd: &Path,
        checkpoint_ref: &str,
        fallback_to_head: bool,
    ) -> Result<bool, VcsError> {
        ensure_git_work_tree(&self.git, "CheckpointStore.restoreCheckpoint", cwd).await?;
        self.git
            .restore_checkpoint(cwd, checkpoint_ref, fallback_to_head)
            .await
    }

    pub async fn diff_checkpoints(&self, input: &DiffCheckpointsInput) -> Result<String, VcsError> {
        ensure_git_work_tree(&self.git, "CheckpointStore.diffCheckpoints", &input.cwd).await?;
        self.git.diff_checkpoints(input).await
    }

    pub async fn delete_checkpoint_refs(
        &self,
        cwd: &Path,
        checkpoint_refs: &[String],
    ) -> Result<(), VcsError> {
        ensure_git_work_tree(&self.git, "CheckpointStore.deleteCheckpointRefs", cwd).await?;
        self.git.delete_checkpoint_refs(cwd, checkpoint_refs).await
    }
}
