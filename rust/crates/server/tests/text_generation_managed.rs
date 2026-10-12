//! Text generation on a managed Codex (signed in with ChatGPT), where the sign-in decides whether and for how long a request may run
//! (`CodexManagedProvider.ts`: `withAccess(runtime.resolve)`). What the CLI is started with is held to the original in
//! `text_generation_parity.rs`; this file holds what happens around the run: a sign-in that is changing refuses it, a sign-in that is replaced
//! or ended stops it and waits until it is gone, and an ordinary run releases the sign-in.
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};
use t3_contracts::{ProviderSetupError, ProviderSetupErrorTag};
use t3_server::{
    codex::CodexInstance,
    codex_managed::Effective,
    provider_auth_flow::StopSession,
    provider_registry::TextInstance,
    test_support::HANG_BOUND,
    text_generation::{Operation, Request, Runner, Shape},
    text_generation_run::{CliRunner, ManagedRuntime, Options},
};

fn fake() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text-generation-provider.py")
}

/// A sign-in that admits a request (or refuses it with `refusal`) and hands the stop it was given to the test.
struct SignIn {
    effective: Effective,
    refusal: Option<&'static str>,
    stop: Mutex<Option<tokio::sync::oneshot::Sender<StopSession>>>,
    released: Arc<std::sync::atomic::AtomicBool>,
}
struct Admitted(Arc<std::sync::atomic::AtomicBool>);
impl Drop for Admitted {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
impl ManagedRuntime for SignIn {
    fn resolve(&self, stop: StopSession) -> BoxFuture<'_, Result<(Effective, Box<dyn std::any::Any + Send>), ProviderSetupError>> {
        if let Some(sender) = self.stop.lock().unwrap().take() {
            let _ = sender.send(stop);
        }
        Box::pin(async move {
            match self.refusal {
                Some(detail) => Err(ProviderSetupError { tag: ProviderSetupErrorTag::ProviderSetupError, instance_id: "work".parse().unwrap(), operation: "session".into(), detail: detail.into(), cause: None }),
                None => Ok((self.effective.clone(), Box::new(Admitted(self.released.clone())) as Box<dyn std::any::Any + Send>)),
            }
        })
    }
}

struct Fixture {
    runner: CliRunner,
    plan: tempfile::TempDir,
    stop: tokio::sync::oneshot::Receiver<StopSession>,
    released: Arc<std::sync::atomic::AtomicBool>,
}
fn fixture(refusal: Option<&'static str>, plan: Value) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("plan.json"), plan.to_string()).unwrap();
    let environment: HashMap<String, String> = [("T3_TEXTGEN_DIR".to_string(), directory.path().to_string_lossy().into_owned())].into();
    let settings = json!({"enabled":true,"binaryPath":"","homePath":"","shadowHomePath":"","launchArgs":"","customModels":[],"setupMode":"managed"});
    let instance = CodexInstance { instance_id: "work".into(), display_name: "Work".into(), accent_color: None, enabled: true, config: serde_json::from_value(settings).unwrap(), environment: environment.clone() };
    let (sender, stop) = tokio::sync::oneshot::channel();
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let effective = Effective { binary: fake(), environment, launch_args: String::new(), home: directory.path().into(), access_token: "token".into(), lease: None };
    let sign_in: Arc<dyn ManagedRuntime> = Arc::new(SignIn { effective, refusal, stop: Mutex::new(Some(sender)), released: released.clone() });
    let found = Arc::new(Mutex::new(Some(TextInstance::ManagedCodex(instance, vec![], sign_in))));
    Fixture { runner: CliRunner::new(Arc::new(move |_| found.lock().unwrap().take()), Options::default(), None), plan: directory, stop, released }
}
fn request(cwd: &Path) -> Request {
    Request { operation: Operation::ThreadTitle, cwd: cwd.to_string_lossy().into_owned(), prompt: "Fix the login bug".into(), shape: Shape::ThreadTitle, model_selection: json!({"instanceId":"work","model":"m"}), attachments: vec![] }
}

#[tokio::test]
async fn a_run_answers_and_releases_the_sign_in() {
    let fixture = fixture(None, json!({"answer":{"title":"Login bug","needsRefinement":false}}));
    let answer = fixture.runner.run(request(fixture.plan.path())).await.unwrap();
    assert_eq!(answer["title"], "Login bug");
    assert!(fixture.released.load(std::sync::atomic::Ordering::SeqCst), "the sign-in is not held after the run");
}

#[tokio::test]
async fn a_sign_in_that_is_changing_refuses_the_run_with_its_own_words() {
    let fixture = fixture(Some("Provider sign-in is changing. Try again after it finishes."), json!({"answer":{"title":"never"}}));
    let error = fixture.runner.run(request(fixture.plan.path())).await.unwrap_err();
    assert_eq!(error.detail, "Provider sign-in is changing. Try again after it finishes.");
    // Nothing was started: the CLI would have left a capture.
    assert!(!fixture.plan.path().join("001.capture.json").exists());
}

#[tokio::test]
async fn a_sign_in_that_is_replaced_stops_the_run_and_the_stop_returns_once_it_is_gone() {
    let Fixture { runner, plan, stop, .. } = fixture(None, json!({"hang":true}));
    let run = runner.run(request(plan.path()));
    let replace = async {
        let stop = tokio::time::timeout(HANG_BOUND, stop).await.expect("the run asked the sign-in for access").unwrap();
        // The run is stopped wherever it is (resolving, starting or waiting for the CLI), and this returns when it has ended.
        tokio::time::timeout(HANG_BOUND, stop()).await.expect("the stop is answered");
    };
    let (outcome, ()) = tokio::time::timeout(HANG_BOUND, async { tokio::join!(run, replace) }).await.expect("the run ends when the sign-in stops it");
    assert_eq!(outcome.unwrap_err().detail, "Codex text generation failed.");
}
