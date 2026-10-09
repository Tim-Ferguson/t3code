//! Text generation held to the original, from fixtures recorded by
//! `rust/tools/generate_text_generation_fixtures.mjs` (which runs the unchanged TypeScript):
//! the prompts byte for byte, what is done to answers and to the transcript a title is made from,
//! which provider and model write, the change request template found in a repository, what a
//! commit's writing style and repository instructions add to the prompt, and the arguments,
//! directory, environment and stdin the Codex and Claude CLIs are started with, against a recording
//! stand-in executable (`fixtures/text-generation-provider.py`). No provider CLI is run.
#![recursion_limit = "256"]
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};
use t3_server::{
    claude_instance::ClaudeInstance,
    codex::CodexInstance,
    git_workflow_stacked::{CommitContext, PrContext},
    persistence::Store,
    provider_registry::TextInstance,
    text_generation::*,
    text_generation_context::{ContextMessage, format_thread_title_context},
    text_generation_run::{CliRunner, Options},
    text_generation_select as select,
    text_generation_service::{TextGenerationService, find_pr_template},
};

/// Long inputs are recorded as the pattern that makes them (`long()` in the generator).
fn expand(value: &Value) -> Value {
    match value {
        Value::Object(map) if map.len() == 1 && map.contains_key("repeat") => {
            let (unit, length) = (map["repeat"][0].as_str().unwrap(), map["repeat"][1].as_u64().unwrap() as usize);
            Value::String(unit.repeat(length.div_ceil(unit.len())).chars().take(length).collect())
        }
        Value::Object(map) => Value::Object(map.iter().map(|(key, member)| (key.clone(), expand(member))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(expand).collect()),
        other => other.clone(),
    }
}

fn rows(text: &str) -> Vec<Value> {
    text.lines().filter(|line| !line.is_empty()).map(|line| expand(&serde_json::from_str(line).unwrap())).collect()
}

fn optional(value: &Value) -> Option<&str> {
    value.as_str()
}

fn assert_prompt(label: &str, row: &Value, prompt: &str) {
    if let Some(expected) = row["prompt"].as_str() {
        assert_eq!(prompt, expected, "{label}");
        return;
    }
    let units = prompt.encode_utf16().count();
    assert_eq!(units as u64, row["promptLength"].as_u64().unwrap(), "{label}: length");
    let head: String = prompt.chars().take(300).collect();
    assert!(row["promptHead"].as_str().unwrap().len() >= 300 || prompt.starts_with(row["promptHead"].as_str().unwrap()), "{label}");
    assert!(prompt.starts_with(row["promptHead"].as_str().unwrap()), "{label}: head {head:?}");
    assert!(prompt.ends_with(row["promptTail"].as_str().unwrap()), "{label}: tail");
    let digest: String = Sha256::digest(prompt.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(digest, row["promptSha256"].as_str().unwrap(), "{label}: digest");
}

#[test]
fn prompts_are_the_originals_byte_for_byte() {
    let rows = rows(include_str!("fixtures/text-generation-prompts.jsonl"));
    assert_eq!(rows.len(), 204);
    for (index, row) in rows.iter().enumerate() {
        let input = &row["input"];
        let label = format!("{} #{index}", row["kind"]);
        let (prompt, shape) = match row["kind"].as_str().unwrap() {
            "commit" => (
                commit_message_prompt(&CommitPrompt {
                    branch: optional(&input["branch"]),
                    staged_summary: input["stagedSummary"].as_str().unwrap(),
                    staged_patch: input["stagedPatch"].as_str().unwrap(),
                    include_branch: input["includeBranch"] == true,
                    instructions: optional(&input["instructions"]),
                }),
                if input["includeBranch"] == true { Shape::CommitWithBranch } else { Shape::Commit },
            ),
            "changeRequest" => (
                change_request_prompt(&ChangeRequestPrompt {
                    base_branch: input["baseBranch"].as_str().unwrap(),
                    head_branch: input["headBranch"].as_str().unwrap(),
                    commit_summary: input["commitSummary"].as_str().unwrap(),
                    diff_summary: input["diffSummary"].as_str().unwrap(),
                    diff_patch: input["diffPatch"].as_str().unwrap(),
                    template: optional(&input["changeRequestTemplate"]),
                    instructions: optional(&input["instructions"]),
                }),
                Shape::ChangeRequest,
            ),
            "branch" => (
                branch_name_prompt(
                    input["message"].as_str().unwrap(),
                    input["attachments"].as_array().unwrap(),
                    &Naming { mode: input["naming"]["mode"].as_str().unwrap().into(), prefix: input["naming"]["prefix"].as_str().unwrap().into(), instructions: input["naming"]["instructions"].as_str().unwrap().into() },
                ),
                Shape::Branch,
            ),
            "threadTitle" => (
                thread_title_prompt(&TitlePrompt {
                    message: input["message"].as_str().unwrap(),
                    previous_title: optional(&input["previousTitle"]),
                    linked_context: optional(&input["linkedContext"]),
                    attachments: input["attachments"].as_array().unwrap(),
                }),
                Shape::ThreadTitle,
            ),
            other => panic!("unknown kind {other}"),
        };
        assert_prompt(&label, row, &prompt);
        assert_eq!(shape.schema(), &row["schema"], "{label}: schema");
        // The schema is held to what the CLI is given: key order included.
        assert_eq!(serde_json::to_string(shape.schema()).unwrap(), serde_json::to_string(&row["schema"]).unwrap(), "{label}: schema text");
    }
}

fn naming(row: &Value) -> Naming {
    Naming { mode: row["naming"]["mode"].as_str().unwrap().into(), prefix: row["naming"]["prefix"].as_str().unwrap().into(), instructions: row["naming"]["instructions"].as_str().unwrap().into() }
}

#[test]
fn answers_are_finished_as_the_original_finishes_them() {
    let rows = rows(include_str!("fixtures/text-generation-sanitize.jsonl"));
    assert_eq!(rows.len(), 148);
    for (index, row) in rows.iter().enumerate() {
        let label = format!("{} #{index} {}", row["fn"], row["raw"]);
        match row["fn"].as_str().unwrap() {
            "commitSubject" => assert_eq!(sanitize_commit_subject(row["raw"].as_str().unwrap()), row["out"], "{label}"),
            "prTitle" => assert_eq!(sanitize_pr_title(row["raw"].as_str().unwrap()), row["out"], "{label}"),
            "threadTitle" => assert_eq!(sanitize_thread_title(row["raw"].as_str().unwrap()), row["out"], "{label}"),
            "branchName" => assert_eq!(format_generated_branch_name(row["raw"].as_str().unwrap(), &naming(row)), row["out"], "{label}"),
            "featureBranch" => assert_eq!(t3_server::git_workflow_stacked::sanitize_feature_branch_name(row["raw"].as_str().unwrap()), row["out"], "{label}"),
            "limitSection" => assert_eq!(limit_section(row["value"].as_str().unwrap(), row["max"].as_u64().unwrap() as usize), row["out"], "{label}"),
            "limitTitleMessage" => assert_eq!(limit_title_message(row["text"].as_str().unwrap(), row["budget"].as_u64().unwrap() as usize), row["out"], "{label}"),
            "commitOp" => {
                let include = row["includeBranch"] == true;
                let answer = if include { Shape::CommitWithBranch } else { Shape::Commit }.decode(&row["generated"]).unwrap();
                let finished = finish_commit_message(&answer, include);
                assert_eq!(finished.subject, row["out"]["subject"], "{label}");
                assert_eq!(finished.body, row["out"]["body"], "{label}");
                assert_eq!(finished.branch.as_deref(), row["out"]["branch"].as_str(), "{label}");
            }
            "prOp" => {
                let finished = finish_change_request(&Shape::ChangeRequest.decode(&row["generated"]).unwrap());
                assert_eq!((finished.title.as_str(), finished.body.as_str()), (row["out"]["title"].as_str().unwrap(), row["out"]["body"].as_str().unwrap()), "{label}");
            }
            "titleOp" => {
                let finished = finish_thread_title(&Shape::ThreadTitle.decode(&row["generated"]).unwrap());
                assert_eq!(finished.title, row["out"]["title"], "{label}");
                assert_eq!(finished.needs_refinement, row["out"]["needsRefinement"] == true, "{label}");
            }
            "citations" => assert_eq!(t3_server::text_generation_context::assistant_citations_to_plain_text(row["prompt"].as_str().unwrap()), row["out"], "{label}"),
            "titleContext" => {
                let messages = row["messages"].as_array().unwrap();
                let empty: Vec<Value> = vec![];
                let context: Vec<ContextMessage> = messages
                    .iter()
                    .map(|message| ContextMessage { role: message["role"].as_str().unwrap(), text: message["text"].as_str().unwrap(), attachments: message["attachments"].as_array().unwrap_or(&empty) })
                    .collect();
                let formatted = format_thread_title_context(&context);
                assert_eq!(formatted.message, row["out"]["message"].as_str().unwrap(), "{label}");
                assert_eq!(json!(formatted.attachments), row["out"]["attachments"], "{label}");
            }
            other => panic!("unknown function {other}"),
        }
    }
}

#[test]
fn the_writing_provider_is_chosen_as_the_original_chooses() {
    let rows = rows(include_str!("fixtures/text-generation-settings.jsonl"));
    assert_eq!(rows.len(), 22);
    for row in &rows {
        let label = row["name"].as_str().unwrap();
        let settings = &row["settings"];
        let general = select::resolve_text_generation_provider(settings);
        assert_eq!(general["textGenerationModelSelection"], row["general"], "{label}: general");
        let project = select::resolve_project_settings(&general, row["projectId"].as_str());
        for key in ["textGenerationModelSelection", "sourceControlWriterModelSelection", "sourceControlWritingStyle", "branchNamingMode", "branchNamePrefix", "branchNameInstructions"] {
            assert_eq!(project[key], row["project"][key], "{label}: {key}");
        }
        let providers = row["providers"].as_array().unwrap();
        assert_eq!(select::source_control_writer_selection(&project, providers), row["writer"], "{label}: writer");
        assert_eq!(select::stacked_action_selection(&project, providers), row["stacked"], "{label}: stacked action");
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.com", "-c", "commit.gpgsign=false"]).args(args).current_dir(cwd).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write_files(root: &Path, files: &Value) {
    for (path, content) in files.as_object().unwrap() {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, content.as_str().unwrap()).unwrap();
    }
}

#[tokio::test]
async fn the_change_request_template_is_the_one_the_original_finds() {
    let rows = rows(include_str!("fixtures/text-generation-templates.jsonl"));
    assert_eq!(rows.len(), 12);
    for row in &rows {
        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        git(&cwd, &["init", "--initial-branch=main"]);
        write_files(&cwd, &row["files"]);
        git(&cwd, &["add", "-A"]);
        git(&cwd, &["commit", "-m", "Initial"]);
        let found = find_pr_template(cwd.to_str().unwrap(), "main").await;
        assert_eq!(found.as_deref(), row["template"].as_str(), "{}", row["name"]);
    }
}

/// A runner that records what it is asked and answers a commit message.
#[derive(Default)]
struct Recorder(Mutex<Vec<Request>>);

impl Runner for Recorder {
    fn run<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Value, TextGenerationError>> {
        let answer = match request.shape {
            Shape::CommitWithBranch => json!({"subject":"Subject","body":"","branch":"feature/x"}),
            _ => json!({"subject":"Subject","body":""}),
        };
        self.0.lock().unwrap().push(request);
        Box::pin(async move { Ok(answer) })
    }
}

#[tokio::test]
async fn a_commit_is_written_with_the_style_instructions_and_model_the_original_sends() {
    let rows = rows(include_str!("fixtures/text-generation-git-inputs.jsonl"));
    assert_eq!(rows.len(), 15);
    for row in &rows {
        let label = row["name"].as_str().unwrap();
        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        git(&cwd, &["init", "--initial-branch=main"]);
        write_files(&cwd, &json!({"README.md":"hello\n"}));
        git(&cwd, &["add", "-A"]);
        git(&cwd, &["commit", "-m", "Initial commit"]);
        for (index, subject) in row["commits"].as_array().unwrap().iter().enumerate() {
            write_files(&cwd, &json!({format!("history-{index}.txt"): format!("{index}\n")}));
            git(&cwd, &["add", "-A"]);
            git(&cwd, &["commit", "-m", subject.as_str().unwrap()]);
        }
        write_files(&cwd, &row["files"]);
        if row["detach"] == true {
            git(&cwd, &["checkout", "--detach"]);
        }
        assert!(row["called"].as_bool().unwrap(), "{label}");
        let input = &row["input"];
        let (settings, providers) = (row["settings"].clone(), row["providers"].as_array().unwrap().clone());
        let recorder = Arc::new(Recorder::default());
        let store = Store::open(root.path().join("state.sqlite")).unwrap();
        let service = TextGenerationService::with_sources(Arc::new(move || settings.clone()), Arc::new(move || providers.clone()), store, recorder.clone());
        let context = CommitContext {
            cwd: cwd.to_str().unwrap(),
            thread_id: None,
            branch: input["branch"].as_str(),
            staged_summary: input["stagedSummary"].as_str().unwrap(),
            staged_patch: input["stagedPatch"].as_str().unwrap(),
            include_branch: input["includeBranch"] == true,
        };
        service.write_commit_message(&context).await.unwrap();
        let requests = recorder.0.lock().unwrap();
        assert_eq!(requests.len(), 1, "{label}");
        let request = &requests[0];
        assert_prompt(label, row, &request.prompt);
        assert_eq!(request.model_selection, input["modelSelection"], "{label}: model");
        assert_eq!(request.operation, Operation::CommitMessage, "{label}");
        assert_eq!(request.shape, if context.include_branch { Shape::CommitWithBranch } else { Shape::Commit }, "{label}");
    }
}

#[tokio::test]
async fn a_pull_request_template_reaches_the_prompt_for_github_only() {
    let root = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(root.path()).unwrap();
    git(&cwd, &["init", "--initial-branch=main"]);
    write_files(&cwd, &json!({".github/pull_request_template.md":"## Template heading\n"}));
    git(&cwd, &["add", "-A"]);
    git(&cwd, &["commit", "-m", "Initial"]);
    let settings = rows(include_str!("fixtures/text-generation-settings.jsonl"))[0]["settings"].clone();
    for (kind, follow, expect) in [
        (t3_contracts::SourceControlProviderKind::Github, true, true),
        (t3_contracts::SourceControlProviderKind::Github, false, false),
        (t3_contracts::SourceControlProviderKind::Gitlab, true, false),
    ] {
        let mut settings = settings.clone();
        settings["sourceControlWritingStyle"]["followChangeRequestTemplates"] = json!(follow);
        let recorder = Arc::new(Recorder::default());
        let store = Store::open(root.path().join(format!("{expect}-{follow}-{kind:?}.sqlite"))).unwrap();
        let service = TextGenerationService::with_sources(Arc::new(move || settings.clone()), Arc::new(Vec::new), store, recorder.clone());
        let context = PrContext { cwd: cwd.to_str().unwrap(), thread_id: None, base_range_ref: "main", provider_kind: kind, base_branch: "main", head_branch: "feature/x", commit_summary: "abc Add", diff_summary: "a | 1 +", diff_patch: "+x" };
        let _ = service.write_change_request(&context).await;
        let prompt = recorder.0.lock().unwrap()[0].prompt.clone();
        assert_eq!(prompt.contains("Repository change request template:\n## Template heading"), expect, "{kind:?} follow={follow}");
        assert_eq!(prompt.contains("body must be markdown and include headings '## Summary' and '## Testing'"), !expect);
    }
}

// ---- the CLIs ---------------------------------------------------------------------------------

fn fake() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text-generation-provider.py")
}

/// The capture with the temporary file paths `codex exec` is given replaced by markers.
fn normalized(capture: &Value, cwd: &Path) -> Value {
    let argv: Vec<Value> = capture["argv"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(at, arg)| match capture["argv"].get(at.wrapping_sub(1)).and_then(Value::as_str) {
            Some("--output-schema") => json!("<schema>"),
            Some("--output-last-message") => json!("<output>"),
            _ => arg.clone(),
        })
        .collect();
    let recorded = capture["cwd"].as_str().unwrap();
    let marker = if Path::new(recorded) == cwd { "<cwd>".to_string() } else if recorded.contains("t3code-claude-title-") { "<title-dir>".into() } else { recorded.to_string() };
    json!({"argv":argv,"cwd":marker,"env":capture["env"],"stdin":capture["stdin"],"schema":capture["schema"]})
}

fn instance(provider: &str, config: &Value, plan_dir: &Path) -> TextInstance {
    let binary = config["binaryPath"].as_str().map(str::to_string).unwrap_or_else(|| fake().to_string_lossy().into_owned());
    let environment: HashMap<String, String> = [("T3_TEXTGEN_DIR".to_string(), plan_dir.to_string_lossy().into_owned())].into();
    if provider == "claude" {
        let settings = json!({"enabled":true,"binaryPath":binary,"homePath":config["homePath"].as_str().unwrap_or(""),"customModels":[],"launchArgs":"","autoCompactWindow":""});
        TextInstance::Claude(ClaudeInstance { instance_id: "claudeAgent".into(), display_name: "Claude".into(), accent_color: None, enabled: true, config: serde_json::from_value(settings).unwrap(), environment })
    } else {
        let settings = json!({"enabled":true,"binaryPath":binary,"homePath":config["homePath"].as_str().unwrap_or(""),"shadowHomePath":"","launchArgs":config["launchArgs"].as_str().unwrap_or(""),"customModels":[]});
        TextInstance::Codex(CodexInstance { instance_id: "codex".into(), display_name: "Codex".into(), accent_color: None, enabled: true, config: serde_json::from_value(settings).unwrap(), environment }, vec![])
    }
}

#[tokio::test]
async fn the_clis_are_started_as_the_original_starts_them() {
    let rows = rows(include_str!("fixtures/text-generation-invocations.jsonl"));
    assert_eq!(rows.len(), 83);
    let cwd_root = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(cwd_root.path()).unwrap();
    for (index, row) in rows.iter().enumerate() {
        let label = format!("#{index} {} {} {}: {}", row["provider"], row["op"], row["label"], row["selection"]);
        let plan = tempfile::tempdir().unwrap();
        let mut script = json!({});
        for (field, key) in [("stdout", "stdout"), ("outputFile", "outputFile"), ("stderr", "stderr")] {
            if let Some(value) = row[field].as_str() {
                script[key] = json!(value);
            }
        }
        script["exit"] = row["exit"].clone();
        std::fs::write(plan.path().join("plan.json"), script.to_string()).unwrap();
        let provider = row["provider"].as_str().unwrap().to_string();
        let found = instance(&provider, &row["config"], plan.path());
        let found = Arc::new(Mutex::new(Some(found)));
        let runner = CliRunner::new(Arc::new(move |_| found.lock().unwrap().take()), Options::default(), None);
        let input = &row["input"];
        let op = row["op"].as_str().unwrap();
        let selection = row["selection"].clone();
        let (operation, prompt, shape) = match op {
            "generateCommitMessage" => (
                Operation::CommitMessage,
                commit_message_prompt(&CommitPrompt { branch: input["branch"].as_str(), staged_summary: input["stagedSummary"].as_str().unwrap(), staged_patch: input["stagedPatch"].as_str().unwrap(), include_branch: false, instructions: None }),
                Shape::Commit,
            ),
            "generatePrContent" => (
                Operation::PrContent,
                change_request_prompt(&ChangeRequestPrompt { base_branch: "main", head_branch: "feature/x", commit_summary: "abc Add", diff_summary: "a.txt | 1 +", diff_patch: "+x", template: None, instructions: None }),
                Shape::ChangeRequest,
            ),
            "generateBranchName" => (Operation::BranchName, branch_name_prompt("Add search", &[], &Naming { mode: "static".into(), prefix: "t3".into(), instructions: String::new() }), Shape::Branch),
            "generateThreadTitle" => (Operation::ThreadTitle, thread_title_prompt(&TitlePrompt { message: "Fix the login bug", previous_title: None, linked_context: Some("none"), attachments: &[] }), Shape::ThreadTitle),
            other => panic!("unknown operation {other}"),
        };
        let outcome = runner.run(Request { operation, cwd: cwd.to_string_lossy().into_owned(), prompt, shape, model_selection: selection, attachments: vec![] }).await;
        match (outcome, row["ok"].as_bool().unwrap()) {
            (Ok(answer), true) => {
                let finished = match op {
                    "generateCommitMessage" => json!({"subject":finish_commit_message(&answer, false).subject,"body":finish_commit_message(&answer, false).body}),
                    "generatePrContent" => json!({"title":finish_change_request(&answer).title,"body":finish_change_request(&answer).body}),
                    "generateBranchName" => json!({"branch":format_generated_branch_name(answer["branch"].as_str().unwrap(), &Naming { mode: "static".into(), prefix: "t3".into(), instructions: String::new() })}),
                    _ => {
                        let title = finish_thread_title(&answer);
                        if title.needs_refinement { json!({"title":title.title,"needsRefinement":true}) } else { json!({"title":title.title}) }
                    }
                };
                assert_eq!(finished, row["result"], "{label}");
            }
            (Err(error), false) => {
                assert_eq!(error.detail, row["error"]["detail"].as_str().unwrap(), "{label}");
                assert_eq!(error.operation, row["error"]["operation"].as_str().unwrap(), "{label}");
            }
            (other, expected) => panic!("{label}: expected ok={expected}, got {other:?}"),
        }
        let capture_path = plan.path().join("001.capture.json");
        match (row["capture"].is_null(), capture_path.exists()) {
            (true, false) => continue,
            (false, true) => {}
            (expected_none, exists) => panic!("{label}: capture expected none={expected_none}, exists={exists}"),
        }
        let capture: Value = serde_json::from_str(&std::fs::read_to_string(capture_path).unwrap()).unwrap();
        let mut got = normalized(&capture, &cwd);
        let expected = row["capture"].clone();
        // The two variables are only comparable when the machine running this does not set them.
        for key in ["CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
            if row["capture"]["env"][key].is_null() && std::env::var_os(key).is_some() {
                got["env"][key] = Value::Null;
            }
        }
        assert_eq!(got["argv"], expected["argv"], "{label}: arguments");
        assert_eq!(got["cwd"], expected["cwd"], "{label}: directory");
        assert_eq!(got["env"], expected["env"], "{label}: environment");
        assert_eq!(got["stdin"], expected["stdin"], "{label}: stdin");
        assert_eq!(got["schema"], expected["schema"], "{label}: schema file");
    }
}

/// Answers every request with a branch name and keeps what it was asked.
struct Brancher(Mutex<Vec<Request>>);

impl Runner for Brancher {
    fn run<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Value, TextGenerationError>> {
        self.0.lock().unwrap().push(request);
        Box::pin(async { Ok(json!({"branch":"Add Search!"})) })
    }
}

#[tokio::test]
async fn a_branch_is_named_in_the_projects_style_by_the_projects_writer() {
    let rows = rows(include_str!("fixtures/text-generation-settings.jsonl"));
    let row = rows.iter().find(|row| row["name"] == "project overrides model").unwrap();
    let (settings, providers) = (row["settings"].clone(), row["providers"].as_array().unwrap().clone());
    let directory = tempfile::tempdir().unwrap();
    let runner = Arc::new(Brancher(Mutex::default()));
    let service = TextGenerationService::with_sources(Arc::new(move || settings.clone()), Arc::new(move || providers.clone()), Store::open(directory.path().join("state.sqlite")).unwrap(), runner.clone());
    let attachments = vec![json!({"type":"image","id":"a-1","name":"shot.png","mimeType":"image/png","sizeBytes":12})];
    // The project is overridden to semantic naming under its own prefix and its own model.
    let named = service.branch_name("project-1", directory.path().to_str().unwrap(), "Add search", &attachments).await.unwrap();
    assert_eq!((named.branch.as_str(), named.exact_name), ("add-search", false));
    let request = runner.0.lock().unwrap().remove(0);
    assert_eq!(request.model_selection, row["project"]["textGenerationModelSelection"]);
    assert_eq!(request.attachments, attachments);
    assert_eq!(request.shape, Shape::Branch);
    assert!(request.prompt.contains("Include a semantic prefix and a slash") && request.prompt.contains("Attachment metadata:\n- shot.png (image/png, 12 bytes)"), "{}", request.prompt);
    // Another project keeps the environment's static naming under the configured prefix.
    let named = service.branch_name("project-2", directory.path().to_str().unwrap(), "Add search", &[]).await.unwrap();
    assert_eq!(named.branch, "t3/add-search");
}
