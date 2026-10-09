//! Review diffs: the diff panel's "Uncommitted" and "Changes" sources, whole-file contents
//! for expanding unchanged lines, and the Changes totals of the status row (ports of
//! `getReviewDiffPreview`, `getReviewDiffFileContents` and `readBranchChangeTotals` in
//! `vcs/GitVcsDriverCore.ts`). Untracked files are shown by adding them with intent-to-add
//! to a temporary copy of the index, never to the real one.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::{
    core::{ExecuteGitResult, GitCommandError, GitCore, GitOptions, is_unborn_head_stderr},
    git::{PATCH_RENDER_PREFIX_ARGS, split_null_separated_paths},
    refs::resolve_path,
    status::BranchChanges,
    temp::{TempPath, copy_index_with_whole_second_mtime},
    wire::{
        ReviewDiffChangeType, ReviewDiffFileContentsInput, ReviewDiffFileContentsResult,
        ReviewDiffFileStat, ReviewDiffPreviewInput, ReviewDiffPreviewResult,
        ReviewDiffPreviewSource, ReviewDiffSourceKind,
    },
};

const REVIEW_DIFF_PATCH_MAX_OUTPUT_BYTES: usize = 120_000;
const REVIEW_METADATA_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const REVIEW_DIFF_FILE_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
// Every status refresh adds the untracked files to a temporary index, and git add's cost grows
// much faster than the file count (50k files took about 30 seconds). Past this many, the
// totals are reported incomplete instead.
const REVIEW_UNTRACKED_MAX_FILES: usize = 5_000;
type Env = Vec<(String, Option<String>)>;

// Shared by review previews and status totals, so the Changes row matches the Changes view.
fn review_diff_args() -> Vec<String> {
    [
        "diff",
        "--find-renames",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--minimal",
    ]
    .into_iter()
    .chain(PATCH_RENDER_PREFIX_ARGS)
    .map(String::from)
    .collect()
}

/// `git diff --numstat -z`. `-z` preserves tabs and newlines in paths and gives renames two
/// separate path fields.
pub fn parse_review_numstat(stdout: &str) -> Vec<ReviewDiffFileStat> {
    static FIELD: OnceLock<Regex> = OnceLock::new();
    let field_pattern = FIELD.get_or_init(|| Regex::new(r"(?s)^(\d+|-)\t(\d+|-)\t(.*)$").unwrap());
    let fields: Vec<&str> = stdout.split('\0').collect();
    let mut files = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        index += 1;
        let Some(captures) = field_pattern.captures(field) else {
            continue;
        };
        let mut next = || {
            let value = fields.get(index).copied().unwrap_or("").to_string();
            index += 1;
            value
        };
        let (previous_path, path) = if captures[3].is_empty() {
            let previous = next();
            (Some(previous), next())
        } else {
            (None, captures[3].to_string())
        };
        let count = |text: &str| {
            if text == "-" {
                0
            } else {
                text.parse().unwrap_or(0)
            }
        };
        files.push(ReviewDiffFileStat {
            path,
            previous_path,
            additions: count(&captures[1]),
            deletions: count(&captures[2]),
        });
    }
    files
}

/// SHA-256 over `JSON.stringify([diff, files])`, as the original identifies a diff.
pub fn review_diff_hash(diff: &str, files: &[ReviewDiffFileStat]) -> String {
    let encoded = serde_json::to_string(&(diff, files)).expect("diff stats serialize");
    Sha256::digest(encoded.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn no_args_error(
    operation: &str,
    command: &str,
    cwd: &Path,
    detail: impl Into<String>,
) -> GitCommandError {
    GitCommandError {
        command: command.into(),
        argument_count: None,
        ..GitCommandError::new(operation, cwd, &[], detail)
    }
}

/// The result of one tracked-file diff: its patch and the complete per-file statistics.
/// `files: None` marks statistics that are incomplete.
struct TrackedDiff {
    stdout: String,
    stdout_truncated: bool,
    files: Option<Vec<ReviewDiffFileStat>>,
}

impl TrackedDiff {
    fn empty() -> Self {
        Self {
            stdout: String::new(),
            stdout_truncated: false,
            files: Some(Vec::new()),
        }
    }

    fn incomplete(self) -> Self {
        Self {
            files: None,
            stdout_truncated: true,
            ..self
        }
    }
}

/// Untracked files added to a temporary index (`env` points git at it); the index lives as
/// long as this does.
struct UntrackedReviewIndex {
    env: Option<Env>,
    _index: Option<TempPath>,
}

impl GitCore {
    async fn prepare_review_index(
        &self,
        cwd: &Path,
        untracked_paths: &[String],
    ) -> Result<Option<(Env, TempPath)>, GitCommandError> {
        let (staged_deletions, index_value) = tokio::try_join!(
            self.stdout_with_marker(
                "GitVcsDriver.readUnifiedWorkingTreeReviewDiff.stagedDeletions",
                cwd,
                [
                    "diff",
                    "--cached",
                    "--name-only",
                    "--diff-filter=D",
                    "-z",
                    "HEAD",
                    "--"
                ],
                GitOptions {
                    allow_non_zero_exit: true,
                    max_output_bytes: Some(REVIEW_METADATA_MAX_OUTPUT_BYTES),
                    ..GitOptions::default()
                },
            ),
            self.stdout(
                "GitVcsDriver.readUnifiedWorkingTreeReviewDiff.indexPath",
                cwd,
                &["rev-parse", "--git-path", "index"],
                false,
            ),
        )?;
        let staged_deletions: HashSet<&str> = staged_deletions
            .split('\0')
            .filter(|path| !path.is_empty())
            .collect();
        let paths_to_add: Vec<&str> = untracked_paths
            .iter()
            .map(String::as_str)
            .filter(|path| !staged_deletions.contains(path))
            .collect();
        if paths_to_add.is_empty() {
            return Ok(None);
        }
        let prepare_error = |cause: std::io::Error| GitCommandError {
            cause: Some(std::sync::Arc::new(cause)),
            ..no_args_error(
                "GitVcsDriver.prepareReviewIndex",
                "git diff",
                cwd,
                "Could not prepare the review index.",
            )
        };
        let index_path = resolve_path(cwd, Path::new(index_value.trim()));
        let temp_index = TempPath::file(&format!("t3code-review-index-{}-", std::process::id()));
        let index_exists = index_path.try_exists().map_err(prepare_error)?;
        if index_exists {
            copy_index_with_whole_second_mtime(&index_path, temp_index.path())
                .map_err(prepare_error)?;
        }
        let env: Env = vec![(
            "GIT_INDEX_FILE".into(),
            Some(temp_index.path().to_string_lossy().into_owned()),
        )];
        let options = || GitOptions {
            env: env.clone(),
            ..GitOptions::default()
        };
        let config = [
            "-c",
            "core.splitIndex=false",
            "-c",
            "splitIndex.sharedIndexExpire=never",
        ];
        if !index_exists {
            self.run(
                "GitVcsDriver.review.emptyIndex",
                cwd,
                ["read-tree", "--empty"],
                options(),
            )
            .await?;
        }
        self.run(
            "GitVcsDriver.readUnifiedWorkingTreeReviewDiff.expandSplitIndex",
            cwd,
            config
                .into_iter()
                .chain(["update-index", "--no-split-index"]),
            options(),
        )
        .await?;
        self.run(
            "GitVcsDriver.readUnifiedWorkingTreeReviewDiff.addUntracked",
            cwd,
            config.into_iter().chain([
                "--literal-pathspecs",
                "add",
                "--intent-to-add",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ]),
            GitOptions {
                stdin: Some(format!("{}\0", paths_to_add.join("\0"))),
                ..options()
            },
        )
        .await?;
        Ok(Some((env.clone(), temp_index)))
    }

    /// Before the first commit, review diffs compare with the empty tree instead of HEAD.
    async fn read_empty_tree_hash(&self, cwd: &Path) -> Result<String, GitCommandError> {
        let stdout = self
            .stdout(
                "GitVcsDriver.review.emptyTree",
                cwd,
                &[
                    "hash-object",
                    "-t",
                    "tree",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                ],
                false,
            )
            .await?;
        Ok(stdout.trim().to_string())
    }

    /// Lists untracked files and adds them to a temporary index, so a diff against any commit
    /// shows them as new. `None` when the list is too big to index or could not be read.
    async fn prepare_untracked_review_index(
        &self,
        cwd: &Path,
        path_args: &[String],
        only_path: Option<&str>,
    ) -> Result<Option<UntrackedReviewIndex>, GitCommandError> {
        let listing = self
            .run(
                "GitVcsDriver.review.listUntracked",
                cwd,
                ["ls-files", "--others", "--exclude-standard", "-z", "--"]
                    .into_iter()
                    .map(String::from)
                    .chain(path_args.iter().cloned()),
                GitOptions {
                    max_output_bytes: Some(REVIEW_METADATA_MAX_OUTPUT_BYTES),
                    ..GitOptions::default()
                },
            )
            .await;
        let listing = match listing {
            Ok(listing) => listing,
            // Tracked changes stay visible when untracked discovery fails; only an oversized
            // listing is an error.
            Err(error) if error.output_length.is_none() => return Ok(None),
            Err(error) => return Err(error),
        };
        let paths: Vec<String> =
            split_null_separated_paths(&listing.stdout, listing.stdout_truncated)
                .into_iter()
                .filter(|path| only_path.is_none_or(|only| path == only))
                .collect();
        if paths.is_empty() {
            return Ok(Some(UntrackedReviewIndex {
                env: None,
                _index: None,
            }));
        }
        if paths.len() > REVIEW_UNTRACKED_MAX_FILES {
            return Ok(None);
        }
        let prepared = self.prepare_review_index(cwd, &paths).await?;
        Ok(Some(match prepared {
            Some((env, index)) => UntrackedReviewIndex {
                env: Some(env),
                _index: Some(index),
            },
            None => UntrackedReviewIndex {
                env: None,
                _index: None,
            },
        }))
    }

    /// The diff panel's Changes view compares the working tree with merge-base(base, HEAD).
    /// With no usable base it compares with HEAD, so Changes equals Uncommitted. Returns the
    /// base ref (if any) and the revision to diff against.
    async fn resolve_review_merge_base(
        &self,
        cwd: &Path,
        branch: Option<&str>,
        explicit_base_ref: Option<&str>,
    ) -> Result<(Option<String>, String), GitCommandError> {
        let base_ref = match explicit_base_ref {
            Some(base) => Some(base.to_string()),
            None => match branch {
                Some(branch) => self
                    .resolve_base_branch_for_no_upstream(cwd, branch, true)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            },
        };
        let Some(base_ref) = base_ref else {
            return Ok((None, "HEAD".into()));
        };
        let args = ["merge-base", &base_ref, "HEAD"].map(String::from);
        let result = self
            .run(
                "GitVcsDriver.resolveReviewMergeBase",
                cwd,
                args.clone(),
                GitOptions {
                    allow_non_zero_exit: true,
                    ..GitOptions::default()
                },
            )
            .await?;
        let merge_base = result.stdout.trim();
        if result.exit_code != 0 || merge_base.is_empty() {
            // Before the first commit there is nothing to compare with, so Changes equals
            // Uncommitted.
            let head = self
                .run(
                    "GitVcsDriver.resolveReviewMergeBase.head",
                    cwd,
                    ["rev-parse", "--verify", "--quiet", "HEAD"],
                    GitOptions {
                        allow_non_zero_exit: true,
                        ..GitOptions::default()
                    },
                )
                .await?;
            if explicit_base_ref.is_none() && head.exit_code != 0 {
                return Ok((None, "HEAD".into()));
            }
            return Err(GitCommandError {
                exit_code: Some(result.exit_code),
                ..GitCommandError::new(
                    "GitVcsDriver.resolveReviewMergeBase",
                    cwd,
                    &args,
                    format!("Could not find a common commit between '{base_ref}' and HEAD."),
                )
            });
        }
        Ok((Some(base_ref), merge_base.to_string()))
    }

    /// Totals for the thread panel's Changes row: same base and untracked files as the
    /// Changes view.
    pub(crate) async fn read_branch_change_totals(
        &self,
        cwd: &Path,
        branch: Option<&str>,
    ) -> Result<BranchChanges, GitCommandError> {
        let (base_ref, merge_base) = self.resolve_review_merge_base(cwd, branch, None).await?;
        let Some(untracked) = self.prepare_untracked_review_index(cwd, &[], None).await? else {
            return Err(no_args_error(
                "GitVcsDriver.readBranchChangeTotals",
                "git ls-files",
                cwd,
                "Too many untracked files to count.",
            ));
        };
        let read_numstat = |revision: String| {
            let args: Vec<String> = review_diff_args()
                .into_iter()
                .chain(["--numstat".into(), "-z".into(), revision, "--".into()])
                .collect();
            self.run(
                "GitVcsDriver.readBranchChangeTotals",
                cwd,
                args,
                GitOptions {
                    allow_non_zero_exit: true,
                    max_output_bytes: Some(REVIEW_METADATA_MAX_OUTPUT_BYTES),
                    env: untracked.env.clone().unwrap_or_default(),
                    ..GitOptions::default()
                },
            )
        };
        let mut result = read_numstat(merge_base.clone()).await?;
        if result.exit_code != 0 && merge_base == "HEAD" && is_unborn_head_stderr(&result.stderr) {
            result = read_numstat(self.read_empty_tree_hash(cwd).await?).await?;
        }
        if result.exit_code != 0 {
            return Err(GitCommandError {
                exit_code: Some(result.exit_code),
                ..no_args_error(
                    "GitVcsDriver.readBranchChangeTotals",
                    "git diff --numstat",
                    cwd,
                    "Could not read Changes totals.",
                )
            });
        }
        let files = parse_review_numstat(&result.stdout);
        Ok(BranchChanges {
            base_ref,
            insertions: files.iter().map(|file| file.additions).sum(),
            deletions: files.iter().map(|file| file.deletions).sum(),
        })
    }

    async fn read_review_stats(
        &self,
        cwd: &Path,
        diff_args: &[String],
        revision: &str,
        env: &Env,
        path_args: &[String],
    ) -> Result<(String, Vec<ReviewDiffFileStat>), GitCommandError> {
        let args = |revision: &str| -> Vec<String> {
            diff_args
                .iter()
                .cloned()
                .chain([
                    "--numstat".into(),
                    "-z".into(),
                    revision.into(),
                    "--".into(),
                ])
                .chain(path_args.iter().cloned())
                .collect()
        };
        let options = || GitOptions {
            max_output_bytes: Some(REVIEW_METADATA_MAX_OUTPUT_BYTES),
            env: env.clone(),
            ..GitOptions::default()
        };
        let result = self
            .run(
                "GitVcsDriver.getReviewDiffPreview.stat",
                cwd,
                args(revision),
                GitOptions {
                    allow_non_zero_exit: true,
                    ..options()
                },
            )
            .await?;
        if result.exit_code == 0 {
            return Ok((revision.into(), parse_review_numstat(&result.stdout)));
        }
        if revision == "HEAD" && is_unborn_head_stderr(&result.stderr) {
            let empty_tree = self.read_empty_tree_hash(cwd).await?;
            let stdout = self
                .stdout_with_marker(
                    "GitVcsDriver.getReviewDiffPreview.unbornStat",
                    cwd,
                    args(&empty_tree),
                    options(),
                )
                .await?;
            return Ok((empty_tree, parse_review_numstat(&stdout)));
        }
        Err(GitCommandError {
            exit_code: Some(result.exit_code),
            ..no_args_error(
                "GitVcsDriver.getReviewDiffPreview.stat",
                "git diff --numstat",
                cwd,
                "Could not read complete diff statistics.",
            )
        })
    }

    /// One commit argument diffs that commit against the working tree.
    async fn read_tracked_review_diff(
        &self,
        cwd: &Path,
        diff_args: &[String],
        revision: Option<&str>,
        env: &Env,
        path_args: &[String],
        patch_limit: usize,
    ) -> Result<TrackedDiff, GitCommandError> {
        let Some(revision) = revision else {
            return Ok(TrackedDiff::empty());
        };
        let (stat_ref, files) = self
            .read_review_stats(cwd, diff_args, revision, env, path_args)
            .await?;
        if files.is_empty() {
            return Ok(TrackedDiff::empty());
        }
        let patch: ExecuteGitResult = self
            .run(
                "GitVcsDriver.getReviewDiffPreview.patch",
                cwd,
                diff_args
                    .iter()
                    .cloned()
                    .chain(["--patch".into(), stat_ref, "--".into()])
                    .chain(path_args.iter().cloned()),
                GitOptions {
                    max_output_bytes: Some(patch_limit),
                    append_truncation_marker: true,
                    env: env.clone(),
                    ..GitOptions::default()
                },
            )
            .await?;
        Ok(TrackedDiff {
            stdout: patch.stdout,
            stdout_truncated: patch.stdout_truncated,
            files: Some(files),
        })
    }

    /// The "Uncommitted" and "Changes" sources for `input.cwd`, or one source's patch for a
    /// single file. A directory outside any worktree has no sources.
    pub async fn get_review_diff_preview(
        &self,
        input: &ReviewDiffPreviewInput,
    ) -> Result<ReviewDiffPreviewResult, GitCommandError> {
        let path_args: Vec<String> = input
            .file
            .iter()
            .flat_map(|file| std::iter::once(&file.path).chain(file.previous_path.as_ref()))
            .map(|path| format!(":(top,literal){path}"))
            .collect();
        let patch_limit = if input.file.is_some() {
            REVIEW_DIFF_FILE_MAX_OUTPUT_BYTES
        } else {
            REVIEW_DIFF_PATCH_MAX_OUTPUT_BYTES
        };
        let input_cwd = Path::new(&input.cwd);
        let repository = match self.resolve_repository_paths_uncached(input_cwd).await {
            Ok(repository) => repository,
            Err(error) if self.is_missing_cwd(&error, input_cwd) => None,
            Err(error) => return Err(error),
        };
        let Some((repository, root)) = repository.and_then(|repository| {
            let root = repository.worktree_root.clone()?;
            Some((repository, root))
        }) else {
            return Ok(ReviewDiffPreviewResult {
                cwd: input.cwd.clone(),
                generated_at: self.state.clock.utc_now(),
                sources: Vec::new(),
            });
        };
        let cwd = PathBuf::from(root);
        let cwd = cwd.as_path();

        let file_kind = input.file.as_ref().map(|file| file.source_kind);
        // A per-file request only reads its own source.
        let dirty_ref = (file_kind != Some(ReviewDiffSourceKind::BranchRange)).then_some("HEAD");
        let (base_ref, merge_base) = if file_kind == Some(ReviewDiffSourceKind::WorkingTree) {
            (input.base_ref.clone(), None)
        } else {
            let (base_ref, merge_base) = self
                .resolve_review_merge_base(
                    cwd,
                    repository.current_branch.as_deref(),
                    input.base_ref.as_deref(),
                )
                .await?;
            (base_ref, Some(merge_base))
        };

        let diff_args: Vec<String> = review_diff_args()
            .into_iter()
            .chain(
                (input.ignore_whitespace == Some(true)).then(|| "--ignore-all-space".to_string()),
            )
            .collect();
        let untracked = self
            .prepare_untracked_review_index(
                cwd,
                &path_args,
                input.file.as_ref().map(|file| file.path.as_str()),
            )
            .await?;
        let env = untracked
            .as_ref()
            .and_then(|untracked| untracked.env.clone())
            .unwrap_or_default();
        let read = |revision: Option<String>| {
            let (diff_args, env, path_args) = (&diff_args, &env, &path_args);
            async move {
                self.read_tracked_review_diff(
                    cwd,
                    diff_args,
                    revision.as_deref(),
                    env,
                    path_args,
                    patch_limit,
                )
                .await
            }
        };
        // With no base both sources diff HEAD, so read it once.
        let (mut dirty, mut base) = if merge_base.as_deref() == dirty_ref {
            let result = read(dirty_ref.map(String::from)).await?;
            let copy = TrackedDiff {
                stdout: result.stdout.clone(),
                stdout_truncated: result.stdout_truncated,
                files: result.files.clone(),
            };
            (result, copy)
        } else {
            tokio::try_join!(read(dirty_ref.map(String::from)), read(merge_base.clone()))?
        };
        if untracked.is_none() {
            // Too many untracked files to list: show tracked changes and mark totals
            // incomplete.
            if dirty_ref.is_some() {
                dirty = dirty.incomplete();
            }
            if merge_base.is_some() {
                base = base.incomplete();
            }
        }
        drop(untracked);

        let source = |id: &str, kind, title: String, base_ref, head_ref, result: TrackedDiff| {
            ReviewDiffPreviewSource {
                id: id.into(),
                kind,
                title,
                base_ref,
                head_ref,
                diff_hash: review_diff_hash(&result.stdout, result.files.as_deref().unwrap_or(&[])),
                diff: result.stdout,
                truncated: result.stdout_truncated,
                files: result.files,
            }
        };
        let sources = vec![
            source(
                "working-tree",
                ReviewDiffSourceKind::WorkingTree,
                "Uncommitted".into(),
                Some("HEAD".into()),
                None,
                dirty,
            ),
            source(
                "branch-range",
                ReviewDiffSourceKind::BranchRange,
                base_ref
                    .as_ref()
                    .map_or_else(|| "Changes".into(), |base| format!("Changes vs {base}")),
                base_ref,
                // For display only. The new side is the working tree.
                Some(repository.current_branch.unwrap_or_else(|| "HEAD".into())),
                base,
            ),
        ];
        Ok(ReviewDiffPreviewResult {
            cwd: input.cwd.clone(),
            generated_at: self.state.clock.utc_now(),
            sources,
        })
    }

    async fn read_review_file_at_revision(
        &self,
        input: &ReviewDiffFileContentsInput,
        revision: &str,
        relative_path: &str,
    ) -> Result<String, GitCommandError> {
        let result = self
            .run(
                "GitVcsDriver.getReviewDiffFileContents.revision",
                Path::new(&input.cwd),
                ["show", &format!("{revision}:{relative_path}")],
                GitOptions {
                    max_output_bytes: Some(REVIEW_DIFF_FILE_MAX_OUTPUT_BYTES),
                    ..GitOptions::default()
                },
            )
            .await?;
        if result.stdout.contains('\0') {
            return Err(file_contents_error(
                input,
                format!("Cannot expand binary file '{relative_path}'."),
            ));
        }
        Ok(result.stdout)
    }

    /// The old and new sides of one file, for expanding unchanged lines. Both views compare a
    /// commit with the working tree: HEAD (or the given base) for Uncommitted,
    /// merge-base(base, HEAD) for Changes. The new side always comes from disk.
    pub async fn get_review_diff_file_contents(
        &self,
        input: &ReviewDiffFileContentsInput,
    ) -> Result<ReviewDiffFileContentsResult, GitCommandError> {
        let cwd = Path::new(&input.cwd);
        let root = self
            .stdout(
                "GitVcsDriver.getReviewDiffFileContents.repositoryRoot",
                cwd,
                &["rev-parse", "--show-toplevel"],
                false,
            )
            .await?;
        let root = root.trim();
        if root.is_empty() {
            return Err(file_contents_error(
                input,
                "Could not resolve the Git repository root.",
            ));
        }
        let old_revision = match input.source_kind {
            ReviewDiffSourceKind::WorkingTree => {
                input.base_ref.clone().unwrap_or_else(|| "HEAD".into())
            }
            ReviewDiffSourceKind::BranchRange => {
                self.resolve_review_merge_base(cwd, None, input.base_ref.as_deref())
                    .await?
                    .1
            }
        };
        let (old_contents, new_contents) = tokio::try_join!(
            async {
                if input.change_type == ReviewDiffChangeType::New {
                    Ok(String::new())
                } else {
                    self.read_review_file_at_revision(input, &old_revision, &input.old_path)
                        .await
                }
            },
            async {
                if input.change_type == ReviewDiffChangeType::Deleted {
                    Ok(String::new())
                } else {
                    read_working_tree_review_file(input, Path::new(root)).await
                }
            },
        )?;
        Ok(ReviewDiffFileContentsResult {
            old_contents,
            new_contents,
        })
    }
}

fn file_contents_error(
    input: &ReviewDiffFileContentsInput,
    detail: impl Into<String>,
) -> GitCommandError {
    no_args_error(
        "GitVcsDriver.getReviewDiffFileContents",
        "git",
        Path::new(&input.cwd),
        detail,
    )
}

async fn read_working_tree_review_file(
    input: &ReviewDiffFileContentsInput,
    repository_root: &Path,
) -> Result<String, GitCommandError> {
    let cwd = Path::new(&input.cwd);
    let error = |stage: &str, detail: String, cause: Option<std::io::Error>| GitCommandError {
        cause: cause.map(std::sync::Arc::new),
        ..no_args_error(
            &format!("GitVcsDriver.getReviewDiffFileContents.workingTree.{stage}"),
            stage,
            cwd,
            detail,
        )
    };
    let outside = |stage: &str| {
        error(
            stage,
            format!(
                "Diff file '{}' resolves outside the review workspace.",
                input.new_path
            ),
            None,
        )
    };
    let requested = resolve_path(repository_root, Path::new(&input.new_path));
    if !requested.starts_with(repository_root) {
        return Err(outside("path.resolve"));
    }
    let unresolved = |cause| {
        error(
            "fs.realPath",
            format!("Could not resolve diff file '{}'.", input.new_path),
            Some(cause),
        )
    };
    let (real_root, real_target) = tokio::try_join!(
        tokio::fs::canonicalize(repository_root),
        tokio::fs::canonicalize(&requested),
    )
    .map_err(unresolved)?;
    if !real_target.starts_with(&real_root) {
        return Err(outside("fs.realPath"));
    }
    let info = tokio::fs::metadata(&real_target).await.map_err(|cause| {
        error(
            "fs.stat",
            format!("Could not inspect diff file '{}'.", input.new_path),
            Some(cause),
        )
    })?;
    if !info.is_file() {
        return Err(error(
            "fs.stat",
            format!("Diff path '{}' is not a file.", input.new_path),
            None,
        ));
    }
    if info.len() > REVIEW_DIFF_FILE_MAX_OUTPUT_BYTES as u64 {
        return Err(error(
            "fs.stat",
            format!(
                "Diff file '{}' exceeds the 1 MB expansion limit.",
                input.new_path
            ),
            None,
        ));
    }
    let bytes = tokio::fs::read(&real_target).await.map_err(|cause| {
        error(
            "fs.readFile",
            format!("Could not read diff file '{}'.", input.new_path),
            Some(cause),
        )
    })?;
    if bytes.contains(&0) {
        return Err(error(
            "fs.readFile",
            format!("Cannot expand binary file '{}'.", input.new_path),
            None,
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
