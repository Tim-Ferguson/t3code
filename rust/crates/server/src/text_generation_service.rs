//! The text generation service: what the rest of the server asks for (a commit message, change
//! request content, a branch name, a thread title) turned into a prompt for the provider the
//! settings choose, and the answer turned back into text. Port of `TextGeneration.ts` and the parts
//! of `GitManager.ts` that decide what is sent: the writing-style policy, the repository's change
//! request template and which model writes.
//!
//! Generation never decides what happens when it fails. Each caller does: a stacked git action
//! stops with the error, a thread title keeps the title it had, a worktree keeps its temporary
//! branch name.
use crate::{
    git_workflow::WorkflowError,
    git_workflow_stacked::{CommitContext, GeneratedCommit, GeneratedPr, PrContext, TextGeneration},
    persistence::Store,
    provider_registry::ProviderRegistry,
    text_generation::{
        self as text, ChangeRequestPrompt, CommitPrompt, Naming, Operation, Request, Runner, Shape, TextGenerationError, ThreadTitle, TitlePrompt, js_trim,
    },
    text_generation_run::CliRunner,
    text_generation_select as select,
    vcs_service::VcsService,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use t3_contracts::SourceControlProviderKind;
use t3_vcs::core::{ExecuteGitInput, GitCore, GitTimeout};

/// The instructions a writing-style choice adds to the commit and change request prompts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Policy {
    pub commit_instructions: Option<String>,
    pub change_request_instructions: Option<String>,
}

/// `resolveStylePolicy` once the repository has been read: `examples` is what `repo_conventions`
/// adds after its preset.
pub fn style_policy(mode: &str, custom_instructions: &str, examples: &str) -> Policy {
    let presets = &text::data()["presets"];
    let preset = |mode: &str, key: &str| presets[mode][key].as_str().map(str::to_string);
    match mode {
        "conventional_commits" => Policy { commit_instructions: preset(mode, "commitInstructions"), change_request_instructions: preset(mode, "changeRequestInstructions") },
        "custom" => {
            let custom = (!custom_instructions.is_empty()).then(|| custom_instructions.to_string());
            Policy { commit_instructions: custom.clone(), change_request_instructions: custom }
        }
        _ => {
            let with_examples = |instructions: Option<String>| if examples.is_empty() { instructions } else { instructions.map(|instructions| format!("{instructions}\n\n{examples}")) };
            Policy { commit_instructions: with_examples(preset("repo_conventions", "commitInstructions")), change_request_instructions: with_examples(preset("repo_conventions", "changeRequestInstructions")) }
        }
    }
}

/// `readRepositoryInstructions`: a regular file of the checkout, within 20,000 bytes, that does not
/// leave the checkout by a link. Anything else is no instructions.
pub fn read_repository_instructions(cwd: &str, file_name: &str) -> String {
    let read = || -> std::io::Result<Option<String>> {
        let root = std::fs::canonicalize(cwd)?;
        let path = std::fs::canonicalize(root.join(file_name))?;
        if !path.starts_with(&root) || path == root {
            return Ok(None);
        }
        let info = std::fs::metadata(&path)?;
        if !info.is_file() || info.len() > 20_000 {
            return Ok(None);
        }
        Ok(Some(js_trim(&std::fs::read_to_string(&path)?).to_string()))
    };
    read().ok().flatten().unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// The repository's change request template (port of `PrTemplateDetection.ts`)
// ---------------------------------------------------------------------------------------------

const TEMPLATE_MAX_BYTES: usize = 8_000;
const TREE_LIST_MAX_BYTES: usize = 100_000;
const TRUNCATION_MARKER: &str = "[truncated]";
const TEMPLATE_PATHS: [&str; 6] = [
    ".github/pull_request_template.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
    "pull_request_template.md",
    "PULL_REQUEST_TEMPLATE.md",
    "docs/pull_request_template.md",
    "docs/PULL_REQUEST_TEMPLATE.md",
];
const TEMPLATE_DIRECTORIES: [&str; 3] = [".github/PULL_REQUEST_TEMPLATE", "PULL_REQUEST_TEMPLATE", "docs/PULL_REQUEST_TEMPLATE"];

/// `(object id, path)` of the regular blobs `git ls-tree -z` lists.
fn parse_template_entries(output: &str) -> Vec<(String, String)> {
    output
        .split('\0')
        .filter_map(|record| {
            let (meta, path) = record.split_once('\t')?;
            let mut parts = meta.split(' ');
            let (mode, kind, id) = (parts.next()?, parts.next()?, parts.next()?);
            let id_ok = (40..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
            (kind == "blob" && matches!(mode, "100644" | "100755") && id_ok).then(|| (id.to_string(), path.to_string()))
        })
        .collect()
}

async fn git_text(core: &GitCore, cwd: &str, operation: &str, args: Vec<String>, max_output_bytes: usize) -> Result<(String, bool), ()> {
    let result = core
        .execute(ExecuteGitInput {
            operation: operation.into(),
            cwd: cwd.into(),
            args,
            timeout: GitTimeout::After(Duration::from_secs(10)),
            max_output_bytes: Some(max_output_bytes),
            append_truncation_marker: true,
            ..Default::default()
        })
        .await
        .map_err(|_| ())?;
    Ok((result.stdout, result.stdout_truncated))
}

async fn read_template_blob(core: &GitCore, cwd: &str, id: &str) -> Result<Option<String>, ()> {
    let (stdout, truncated) = git_text(core, cwd, "PrTemplateDetection.readTemplateBlob", vec!["cat-file".into(), "blob".into(), id.into()], TEMPLATE_MAX_BYTES).await?;
    let template = js_trim(&stdout);
    if template.is_empty() {
        return Ok(None);
    }
    Ok(Some(if truncated && !template.ends_with(TRUNCATION_MARKER) { format!("{template}\n\n{TRUNCATION_MARKER}") } else { template.to_string() }))
}

/// `detectPrTemplate`: the change request template committed at `treeish`, read from git's object
/// store so a link in the working tree can never reach another file. A directory of templates counts
/// only when exactly one is non-empty. Any failure is no template.
pub async fn detect_pr_template(core: &GitCore, cwd: &str, treeish: &str) -> Option<String> {
    let run = async {
        let mut args: Vec<String> = ["ls-tree", "-r", "-z", "--full-tree", treeish, "--"].map(String::from).to_vec();
        args.extend(TEMPLATE_PATHS.iter().chain(TEMPLATE_DIRECTORIES.iter()).map(|path| path.to_string()));
        let (stdout, truncated) = git_text(core, cwd, "PrTemplateDetection.listTemplates", args, TREE_LIST_MAX_BYTES).await?;
        if truncated {
            return Ok(None);
        }
        let entries = parse_template_entries(&stdout);
        for template_path in TEMPLATE_PATHS {
            let Some((id, _)) = entries.iter().find(|(_, path)| path == template_path) else { continue };
            if let Some(template) = read_template_blob(core, cwd, id).await? {
                return Ok(Some(template));
            }
        }
        for directory in TEMPLATE_DIRECTORIES {
            let prefix = format!("{directory}/");
            let mut templates = vec![];
            for (id, path) in entries.iter().filter(|(_, path)| path.strip_prefix(&prefix).is_some_and(|relative| !relative.contains('/') && relative.to_lowercase().ends_with(".md"))) {
                let _ = path;
                if let Some(template) = read_template_blob(core, cwd, id).await? {
                    templates.push(template);
                    if templates.len() > 1 {
                        return Ok(None);
                    }
                }
            }
            if let Some(template) = templates.pop() {
                return Ok(Some(template));
            }
        }
        Ok::<_, ()>(None)
    };
    Box::pin(run).await.ok().flatten()
}

/// `detectPrTemplate` on the process's git.
pub async fn find_pr_template(cwd: &str, treeish: &str) -> Option<String> {
    detect_pr_template(VcsService::shared().core(), cwd, treeish).await
}

// ---------------------------------------------------------------------------------------------
// Links in a thread's first message
// ---------------------------------------------------------------------------------------------

/// `resolveThreadTitleLinks`: what the first two distinct supported links in a message point at,
/// each looked up for up to three seconds.
pub async fn resolve_thread_title_links(cwd: &str, message: &str) -> Option<String> {
    static LINK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = LINK.get_or_init(|| regex::Regex::new(r#"https://[^\s<>"')\]`]+"#).unwrap());
    let registry = crate::source_control_registry::SourceControlRegistry::new(crate::source_control_host::Host::process_default());
    let mut lookups = vec![];
    let mut seen = std::collections::HashSet::new();
    for found in pattern.find_iter(message) {
        let candidate = found.as_str().trim_end_matches(['.', ',', ';', '!', '?']);
        let Ok(mut url) = url::Url::parse(candidate) else { continue };
        url.set_fragment(None);
        url.set_query(None);
        if seen.contains(url.as_str()) {
            continue;
        }
        let Some(lookup) = registry.resolve_link(cwd, &url) else { continue };
        seen.insert(url.to_string());
        lookups.push((url.to_string(), lookup));
        if lookups.len() == 2 {
            break;
        }
    }
    if lookups.is_empty() {
        return None;
    }
    let subjects = futures_util::future::join_all(lookups.into_iter().map(|(url, lookup)| async move {
        match tokio::time::timeout(Duration::from_secs(3), lookup).await {
            Ok(Ok(subject)) => {
                let title: String = text::utf16_prefix(&subject.title, 300).into();
                let body: String = text::utf16_prefix(subject.body.as_deref().unwrap_or(""), 1_200).into();
                format!("{url}\n{}", json!({"title":title,"body":body}))
            }
            _ => format!("{url}: unavailable"),
        }
    }))
    .await;
    Some(subjects.join("\n\n"))
}

// ---------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------

/// Where the service reads the settings and the provider snapshots from.
type Source<T> = Arc<dyn Fn() -> T + Send + Sync>;

#[derive(Clone)]
pub struct TextGenerationService {
    settings: Source<Value>,
    snapshots: Source<Vec<Value>>,
    store: Store,
    runner: Arc<dyn Runner>,
}

pub struct TitleInput<'a> {
    pub cwd: &'a str,
    pub message: &'a str,
    /// Present when replacing an existing title from the thread's history.
    pub previous_title: Option<&'a str>,
    pub attachments: &'a [Value],
    pub model_selection: Value,
}

impl TextGenerationService {
    /// Writes with the CLIs of the registry's provider instances.
    pub fn new(registry: ProviderRegistry, store: Store) -> Self {
        let runner = Arc::new(CliRunner::for_registry(registry.clone(), store.state_dir().map(|directory| directory.join("attachments"))));
        Self::with_runner(registry, store, runner)
    }

    /// With another way of running prompts; tests script it.
    pub fn with_runner(registry: ProviderRegistry, store: Store, runner: Arc<dyn Runner>) -> Self {
        let (settings, snapshots) = (registry.clone(), registry);
        Self::with_sources(Arc::new(move || serde_json::to_value(settings.settings()).expect("settings encode")), Arc::new(move || snapshots.snapshots()), store, runner)
    }

    /// With the settings (as the JSON they are stored as) and provider snapshots given directly.
    pub fn with_sources(settings: Source<Value>, snapshots: Source<Vec<Value>>, store: Store, runner: Arc<dyn Runner>) -> Self {
        Self { settings, snapshots, store, runner }
    }

    /// The settings text generation reads: the global provider fallback applied, then the project's
    /// overrides when the thread or the directory belongs to a project that has any.
    pub fn effective_settings(&self, cwd: &str, thread_id: Option<&str>) -> Value {
        let global = select::resolve_text_generation_provider(&(self.settings)());
        let overridden = global["projectSettingsOverrides"].as_object().is_some_and(|entries| entries.values().any(|entry| entry.as_object().is_some_and(|entry| !entry.is_empty())));
        if !overridden {
            return global;
        }
        let project_id = match thread_id {
            Some(thread) => self.store.projection("thread", thread).ok().flatten().and_then(|view| view["thread"]["projectId"].as_str().map(str::to_string)),
            None => self.store.read(|connection| crate::persistence::read_projections(connection, "project")).ok().and_then(|projects| {
                projects.iter().find(|project| project["deletedAt"].is_null() && project["workspaceRoot"] == cwd).and_then(|project| project["id"].as_str().map(str::to_string))
            }),
        };
        select::resolve_project_settings(&global, project_id.as_deref())
    }

    /// The settings of a known project (a thread's, once it exists).
    pub fn project_settings(&self, project_id: &str) -> Value {
        select::resolve_project_settings(&select::resolve_text_generation_provider(&(self.settings)()), Some(project_id))
    }

    fn snapshots(&self) -> Vec<Value> {
        (self.snapshots)()
    }

    async fn policy(&self, cwd: &str, settings: &Value, selection: &Value) -> Policy {
        let style = &settings["sourceControlWritingStyle"];
        let mode = style["mode"].as_str().unwrap_or("repo_conventions");
        if mode != "repo_conventions" {
            return style_policy(mode, style["customInstructions"].as_str().unwrap_or(""), "");
        }
        let core = VcsService::shared().core();
        let subjects: Vec<String> = git_text_lines(core, cwd).await;
        let agent = read_repository_instructions(cwd, "AGENTS.md");
        let selected = selection["instanceId"].as_str().unwrap_or("");
        let claude_writer = selected == "claudeAgent" || self.snapshots().iter().any(|provider| provider["instanceId"] == selected && provider["driver"] == "claudeAgent");
        let claude = if claude_writer { read_repository_instructions(cwd, "CLAUDE.md") } else { String::new() };
        let examples = [
            (!subjects.is_empty()).then(|| format!("Recent commit subjects from this repository:\n{}", subjects.join("\n"))),
            (!agent.is_empty()).then(|| format!("Local AGENTS.md:\n{agent}")),
            (!claude.is_empty()).then(|| format!("Local CLAUDE.md:\n{claude}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n");
        style_policy(mode, "", &examples)
    }

    async fn run(&self, operation: Operation, cwd: &str, prompt: String, shape: Shape, model_selection: Value, attachments: Vec<Value>) -> Result<Value, TextGenerationError> {
        self.runner.run(Request { operation, cwd: cwd.into(), prompt, shape, model_selection, attachments }).await
    }

    pub async fn write_commit_message(&self, context: &CommitContext<'_>) -> Result<text::CommitMessage, TextGenerationError> {
        let settings = self.effective_settings(context.cwd, context.thread_id);
        let selection = select::stacked_action_selection(&settings, &self.snapshots());
        let policy = self.policy(context.cwd, &settings, &selection).await;
        let prompt = text::commit_message_prompt(&CommitPrompt {
            branch: context.branch,
            staged_summary: context.staged_summary,
            staged_patch: context.staged_patch,
            include_branch: context.include_branch,
            instructions: policy.commit_instructions.as_deref(),
        });
        let shape = if context.include_branch { Shape::CommitWithBranch } else { Shape::Commit };
        let answer = Box::pin(self.run(Operation::CommitMessage, context.cwd, prompt, shape, selection, vec![])).await?;
        Ok(text::finish_commit_message(&answer, context.include_branch))
    }

    pub async fn write_change_request(&self, context: &PrContext<'_>) -> Result<text::ChangeRequestContent, TextGenerationError> {
        let settings = self.effective_settings(context.cwd, context.thread_id);
        let selection = select::stacked_action_selection(&settings, &self.snapshots());
        let policy = self.policy(context.cwd, &settings, &selection).await;
        let follow = settings["sourceControlWritingStyle"]["followChangeRequestTemplates"] == true;
        let template = if follow && context.provider_kind == SourceControlProviderKind::Github { find_pr_template(context.cwd, context.base_range_ref).await } else { None };
        let prompt = text::change_request_prompt(&ChangeRequestPrompt {
            base_branch: context.base_branch,
            head_branch: context.head_branch,
            commit_summary: context.commit_summary,
            diff_summary: context.diff_summary,
            diff_patch: context.diff_patch,
            template: template.as_deref(),
            instructions: policy.change_request_instructions.as_deref(),
        });
        let answer = Box::pin(self.run(Operation::PrContent, context.cwd, prompt, Shape::ChangeRequest, selection, vec![])).await?;
        Ok(text::finish_change_request(&answer))
    }

    /// A branch name for the work a message asks for, in the project's naming style.
    pub async fn branch_name(&self, project_id: &str, cwd: &str, message: &str, attachments: &[Value]) -> Result<BranchName, TextGenerationError> {
        let settings = self.project_settings(project_id);
        let selection = select::stacked_action_selection(&settings, &self.snapshots());
        let naming = Naming {
            mode: settings["branchNamingMode"].as_str().unwrap_or("static").into(),
            prefix: settings["branchNamePrefix"].as_str().unwrap_or("").into(),
            instructions: settings["branchNameInstructions"].as_str().unwrap_or("").into(),
        };
        let prompt = text::branch_name_prompt(message, attachments, &naming);
        let answer = Box::pin(self.run(Operation::BranchName, cwd, prompt, Shape::Branch, selection, attachments.to_vec())).await?;
        Ok(BranchName { branch: text::format_generated_branch_name(answer["branch"].as_str().unwrap_or(""), &naming), exact_name: naming.mode == "custom" })
    }

    pub async fn thread_title(&self, input: &TitleInput<'_>) -> Result<ThreadTitle, TextGenerationError> {
        let linked = resolve_thread_title_links(input.cwd, input.message).await;
        let prompt = text::thread_title_prompt(&TitlePrompt { message: input.message, previous_title: input.previous_title, linked_context: linked.as_deref(), attachments: input.attachments });
        let answer = Box::pin(self.run(Operation::ThreadTitle, input.cwd, prompt, Shape::ThreadTitle, input.model_selection.clone(), input.attachments.to_vec())).await?;
        Ok(text::finish_thread_title(&answer))
    }
}

/// A generated branch name; `exact_name` when the project's naming is custom, so the rename keeps it
/// as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchName {
    pub branch: String,
    pub exact_name: bool,
}

/// `readRecentCommitSubjects`: the last twenty non-merge subjects, or none if git cannot say.
async fn git_text_lines(core: &GitCore, cwd: &str) -> Vec<String> {
    let args = ["log", "-n", "20", "--no-merges", "--pretty=format:%s"].map(String::from).to_vec();
    let result = core.execute(ExecuteGitInput { operation: "GitManager.readRecentCommitSubjects".into(), cwd: Path::new(cwd).into(), args, ..Default::default() }).await;
    result.map(|result| result.stdout.lines().map(|line| js_trim(line).to_string()).filter(|line| !line.is_empty()).collect()).unwrap_or_default()
}

fn failed(error: TextGenerationError) -> WorkflowError {
    WorkflowError::TextGeneration(error)
}

impl TextGeneration for TextGenerationService {
    fn commit_message<'a>(&'a self, context: CommitContext<'a>) -> BoxFuture<'a, crate::git_workflow::WorkflowResult<GeneratedCommit>> {
        Box::pin(async move {
            let generated = Box::pin(self.write_commit_message(&context)).await.map_err(failed)?;
            Ok(GeneratedCommit { subject: generated.subject, body: generated.body, branch: generated.branch })
        })
    }

    fn pr_content<'a>(&'a self, context: PrContext<'a>) -> BoxFuture<'a, crate::git_workflow::WorkflowResult<GeneratedPr>> {
        Box::pin(async move {
            let generated = Box::pin(self.write_change_request(&context)).await.map_err(failed)?;
            Ok(GeneratedPr { title: generated.title, body: generated.body })
        })
    }
}

/// Text generation for a server with no provider registry to write with.
pub struct NoProviders;

impl TextGeneration for NoProviders {
    fn commit_message<'a>(&'a self, _: CommitContext<'a>) -> BoxFuture<'a, crate::git_workflow::WorkflowResult<GeneratedCommit>> {
        Box::pin(async { Err(failed(TextGenerationError::new(Operation::CommitMessage, "No provider is configured to write text."))) })
    }

    fn pr_content<'a>(&'a self, _: PrContext<'a>) -> BoxFuture<'a, crate::git_workflow::WorkflowResult<GeneratedPr>> {
        Box::pin(async { Err(failed(TextGenerationError::new(Operation::PrContent, "No provider is configured to write text."))) })
    }
}
