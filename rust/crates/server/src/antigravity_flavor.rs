//! Antigravity as a flavour of the ACP adapter: what `AntigravityAdapterV2` adds to `AcpAdapterV2`. The adapter asks
//! the flavour for the hooks that differ (the process, the `initialize` request and sign-in method, the file
//! system the client serves, questions among the permission requests, tool normalisation, the session mode a
//! runtime mode selects) and gets the rest from the generic path.
use crate::{
    acp_runtime::StartHooks,
    antigravity_acp as acp,
    antigravity_files as files,
    antigravity_instance::{AntigravityInstance, Holds, initialize_request},
};
use crate::provider_registry::ProviderRegistry;
use futures_util::{StreamExt, future::BoxFuture};
use serde_json::Value;
use std::{collections::VecDeque, path::{Path, PathBuf}, sync::Arc};
use t3_acp::{AcpError, transport::{ProtocolOptions, StdoutStream}};

const MAX_STDOUT_LINE_BYTES: usize = 16 * 1024 * 1024;
/// Called with the Google sign-in URL the agent printed.
pub type OnAuthorizationUrl = Arc<dyn Fn(String) -> Result<(), AcpError> + Send + Sync>;

/// Everything the adapter asks of Antigravity: one per actor, from the thread's runtime mode.
#[derive(Clone)]
pub struct Antigravity {
    pub instance: AntigravityInstance,
    /// The release in use and the runtime directory of the process the actor started; they go with the actor.
    pub holds: Holds,
    /// Where what a session learns about the account (its models and commands) is published.
    pub registry: ProviderRegistry,
}
impl Antigravity {
    pub fn new(instance: AntigravityInstance, registry: ProviderRegistry) -> Self {
        Self { instance, holds: Holds::default(), registry }
    }
    /// A normal launch refuses a browser sign-in: the user signs in from Settings. A URL on either stream ends the
    /// launch with the message that says so.
    pub fn protocol_options(&self) -> ProtocolOptions {
        refuse_sign_in_options()
    }
    /// A sign-in launch hands the Google URL to `on_url`, from whichever stream the agent prints it on.
    pub fn sign_in_options(on_url: OnAuthorizationUrl) -> ProtocolOptions {
        let (stdout_url, stderr_url) = (on_url.clone(), on_url);
        let lines = std::sync::Mutex::new(StderrLines::default());
        ProtocolOptions {
            transform_stdout: Some(Arc::new(move |stdout| authorization_lines(stdout, Some(stdout_url.clone()), None))),
            on_stderr: Some(Arc::new(move |bytes| {
                for url in lines.lock().unwrap().feed(bytes) {
                    let _ = stderr_url(url);
                }
            })),
            ..Default::default()
        }
    }
    /// `onSessionStarted`: a session proves the account works and tells which models it offers.
    pub fn session_started(&self, instance_id: &str, session: &crate::acp_runtime::AcpSession, cwd: &Path) {
        let initialize = serde_json::to_value(&session.initialize).unwrap_or_default();
        let started = serde_json::json!({"version":initialize["agentInfo"]["version"],"setup":serde_json::to_value(&session.setup_response).unwrap_or_default()});
        let cwd = cwd.to_string_lossy().into_owned();
        self.registry.antigravity_update(instance_id, |instance, snapshot, now| {
            let auth = instance.auth();
            crate::antigravity_instance::session_started(snapshot, &started, Some(&cwd), auth.label(), &auth.auth_method, now);
            snapshot["supportsTextGeneration"] = serde_json::json!(instance.text_generation_available());
        });
    }
    /// A session that could not start because the account is signed out clears what was known about it.
    pub fn sign_in_failed(&self, instance_id: &str, cause: &AcpError) {
        if sign_in_required(cause) {
            self.registry.antigravity_update(instance_id, |_, snapshot, now| crate::antigravity_instance::account_cleared(snapshot, now));
        }
    }
    /// Every session update before the adapter sees it: tool payloads are bounded, and the commands and models
    /// the agent advertises reach the provider snapshot.
    pub fn update_seen(&self, instance_id: &str, notification: &mut Value, cwd: &Path) {
        acp::normalize_session_update(notification);
        let update = notification["update"].clone();
        match update["sessionUpdate"].as_str() {
            Some("available_commands_update") => {
                let cwd = cwd.to_string_lossy().into_owned();
                self.registry.antigravity_update(instance_id, |_, snapshot, now| crate::antigravity_instance::commands_updated(snapshot, &update["availableCommands"], Some(&cwd), now));
            }
            Some("config_option_update") => {
                self.registry.antigravity_update(instance_id, |_, snapshot, _| crate::antigravity_instance::models_updated(snapshot, &update["configOptions"]));
            }
            _ => {}
        }
    }
    /// The agent's native permission mode for the thread's runtime mode.
    pub fn session_mode(&self, runtime_mode: &str) -> &'static str {
        acp::permission_mode(runtime_mode)
    }
    /// The workspace, and the leaf directory of pasted uploads the turn text refers to. A session without a
    /// workspace gets no workspace root rather than the server's cwd.
    pub fn file_roots(&self, cwd: &Path) -> Vec<PathBuf> {
        let mut roots = vec![cwd.to_path_buf()];
        roots.extend(self.instance.attachments_dir());
        roots
    }
    /// The `initialize` request (the file system is the client's), the method to authenticate with, and the
    /// handlers for the files the agent reads and writes through T3.
    pub fn hooks(&self, cwd: &Path) -> StartHooks {
        let roots = Arc::new(self.file_roots(cwd));
        StartHooks {
            initialize: Some(initialize_request("t3-code", true)),
            register: Some(Arc::new(move |context| -> BoxFuture<'static, ()> {
                let (read_roots, write_roots) = (roots.clone(), roots.clone());
                Box::pin(async move {
                    context.client.handle_read_text_file(Arc::new(move |request, _| {
                        let roots = read_roots.clone();
                        Box::pin(async move { reply(files::read_text_file(&roots, request.as_value()).await) })
                    }));
                    context.client.handle_write_text_file(Arc::new(move |request, _| {
                        let roots = write_roots.clone();
                        Box::pin(async move { reply(files::write_text_file(&roots, request.as_value()).await) })
                    }));
                })
            })),
            authenticate: Some(self.instance.auth().auth_method),
            prefer_resume: true,
            additional_directories: self.instance.attachments_dir().map(|dir| dir.to_string_lossy().into_owned()).into_iter().collect(),
        }
    }
}
/// The protocol options of every launch that has nobody to sign in: a Google URL ends it.
pub fn refuse_sign_in_options() -> ProtocolOptions {
    let refused = Arc::new(tokio::sync::Notify::new());
    let observed = refused.clone();
    let lines = std::sync::Mutex::new(StderrLines::default());
    ProtocolOptions {
        transform_stdout: Some(Arc::new(move |stdout| authorization_lines(stdout, None, Some(refused.clone())))),
        on_stderr: Some(Arc::new(move |bytes| {
            if !lines.lock().unwrap().feed(bytes).is_empty() {
                observed.notify_one();
            }
        })),
        ..Default::default()
    }
}
fn reply<T: serde::de::DeserializeOwned>(result: Result<Value, AcpError>) -> Result<T, AcpError> {
    result.and_then(|value| serde_json::from_value(value).map_err(|error| AcpError::Transport(error.to_string())))
}
/// Whether the agent refused a request because the account is signed out.
pub fn sign_in_required(cause: &AcpError) -> bool {
    use t3_acp::errors::Failure;
    match cause {
        AcpError::Request(error) => acp::is_sign_in_required(error.code),
        AcpError::Failure(failure) => matches!(failure.as_ref(), Failure::Request(error) if acp::is_sign_in_required(error.code)),
        AcpError::ResponseError { error, .. } => acp::is_sign_in_required(error.code),
        other => other.to_string().contains(acp::SIGN_IN_REQUIRED_MESSAGE),
    }
}
/// `makeAntigravityStdoutTransform`: the line the agent prints before a Google sign-in URL is not protocol. With
/// nobody to show it to the launch fails; otherwise the URL goes to `on_url` and the line is dropped.
pub fn authorization_lines(stdout: StdoutStream, on_url: Option<OnAuthorizationUrl>, refused: Option<Arc<tokio::sync::Notify>>) -> StdoutStream {
    struct State {
        input: StdoutStream,
        pending: Vec<u8>,
        ready: VecDeque<Result<Vec<u8>, AcpError>>,
        done: bool,
    }
    let prefix = acp::AUTH_STDOUT_PREFIX.as_bytes().to_vec();
    let handle = Arc::new(move |line: Vec<u8>| -> Option<Result<Vec<u8>, AcpError>> {
        if !line.starts_with(&prefix) {
            return Some(Ok(line));
        }
        let text = String::from_utf8_lossy(&line);
        let message = text.trim_end_matches(['\r', '\n']);
        let Some(request) = acp::parse_authorization_url(&message[acp::AUTH_STDOUT_PREFIX.len()..]) else {
            return Some(Err(AcpError::Transport("Antigravity returned an invalid Google sign-in URL.".into())));
        };
        match &on_url {
            None => Some(Err(AcpError::Transport(acp::SIGN_IN_REQUIRED_MESSAGE.into()))),
            Some(on_url) => on_url(request.authorization_url).err().map(Err),
        }
    });
    futures_util::stream::unfold(State { input: stdout, pending: vec![], ready: VecDeque::new(), done: false }, move |mut state| {
        let handle = handle.clone();
        let refused = refused.clone();
        async move {
            loop {
                if let Some(next) = state.ready.pop_front() {
                    return Some((next, state));
                }
                if state.done {
                    return None;
                }
                let next = match &refused {
                    Some(refused) => tokio::select! {
                        next = state.input.next() => next,
                        _ = refused.notified() => Some(Err(AcpError::Transport(acp::SIGN_IN_REQUIRED_MESSAGE.into()))),
                    },
                    None => state.input.next().await,
                };
                match next {
                    Some(Ok(chunk)) => {
                        let mut offset = 0;
                        while offset < chunk.len() {
                            let end = chunk[offset..].iter().position(|byte| *byte == b'\n').map_or(chunk.len(), |at| offset + at + 1);
                            if state.pending.len() + (end - offset) > MAX_STDOUT_LINE_BYTES {
                                state.ready.push_back(Err(AcpError::Transport("Antigravity sent a protocol line that is too large.".into())));
                                state.done = true;
                                break;
                            }
                            state.pending.extend_from_slice(&chunk[offset..end]);
                            if chunk[end - 1] == b'\n' {
                                let line = std::mem::take(&mut state.pending);
                                if let Some(item) = handle(line) {
                                    state.ready.push_back(item);
                                }
                            }
                            offset = end;
                        }
                    }
                    Some(Err(error)) => {
                        state.ready.push_back(Err(error));
                        state.done = true;
                    }
                    None => {
                        state.done = true;
                        if !state.pending.is_empty() {
                            let line = std::mem::take(&mut state.pending);
                            if let Some(item) = handle(line) {
                                state.ready.push_back(item);
                            }
                        }
                    }
                }
            }
        }
    })
    .boxed()
}
/// The sign-in URLs among what the agent writes to stderr: complete lines only, each bounded.
#[derive(Default)]
pub struct StderrLines {
    pending: String,
}
impl StderrLines {
    const MAX_LINE: usize = acp::AUTH_BROWSER_MARKER.len() + 16_384 + 2;
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.push_str(&String::from_utf8_lossy(bytes));
        let mut lines: Vec<String> = self.pending.split('\n').map(str::to_owned).collect();
        self.pending = lines.pop().unwrap_or_default();
        if self.pending.len() > Self::MAX_LINE {
            self.pending.clear();
        }
        lines.iter().filter(|line| line.len() <= Self::MAX_LINE).filter_map(|line| acp::authorization_url_in_line(line)).filter_map(|url| acp::parse_authorization_url(&url)).map(|url| url.authorization_url).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    const URL: &str = "https://accounts.google.com/o/oauth2/v2/auth?response_type=code&client_id=c&redirect_uri=http%3A%2F%2F127.0.0.1%3A46353%2F&state=s";
    fn stream(chunks: &[&str]) -> StdoutStream {
        let chunks: Vec<Result<Vec<u8>, AcpError>> = chunks.iter().map(|chunk| Ok(chunk.as_bytes().to_vec())).collect();
        futures_util::stream::iter(chunks).boxed()
    }
    async fn collect(stream: StdoutStream) -> (String, Vec<String>) {
        let items: Vec<Result<Vec<u8>, AcpError>> = stream.collect().await;
        let text = items.iter().filter_map(|item| item.as_ref().ok()).map(|bytes| String::from_utf8_lossy(bytes).into_owned()).collect::<String>();
        (text, items.iter().filter_map(|item| item.as_ref().err()).map(ToString::to_string).collect())
    }
    #[tokio::test]
    async fn the_line_before_a_google_url_is_not_protocol_and_the_url_goes_to_whoever_asked() {
        let seen = Arc::new(std::sync::Mutex::new(vec![]));
        let record = seen.clone();
        let on_url: OnAuthorizationUrl = Arc::new(move |url| {
            record.lock().unwrap().push(url);
            Ok(())
        });
        let line = format!("{}{URL}\n", acp::AUTH_STDOUT_PREFIX);
        let (half, rest) = line.split_at(30);
        // The line arrives in pieces, between two protocol lines; only the protocol lines pass through.
        let (text, errors) = collect(authorization_lines(stream(&["{\"a\":1}\n", half, &format!("{rest}{{\"b\":2}}\n{{\"c\"")]), Some(on_url), None)).await;
        assert_eq!((text.as_str(), errors.len()), ("{\"a\":1}\n{\"b\":2}\n{\"c\"", 0));
        assert_eq!(*seen.lock().unwrap(), [URL.to_owned()]);
    }
    #[tokio::test]
    async fn a_launch_nobody_signs_in_for_fails_on_the_url_and_a_url_that_is_not_googles_is_refused() {
        let line = format!("{}{URL}\n", acp::AUTH_STDOUT_PREFIX);
        let (_, errors) = collect(authorization_lines(stream(&[&line]), None, None)).await;
        assert!(errors.len() == 1 && errors[0].contains(acp::SIGN_IN_REQUIRED_MESSAGE), "{errors:?}");
        let (text, errors) = collect(authorization_lines(stream(&[&format!("{}https://evil.example/\n", acp::AUTH_STDOUT_PREFIX)]), Some(Arc::new(|_| Ok(()))), None)).await;
        assert!(text.is_empty() && errors.len() == 1 && errors[0].contains("Antigravity returned an invalid Google sign-in URL."), "{errors:?}");
        // A URL on stderr ends a launch that has nobody to show it to, even while the agent is silent on stdout.
        let refused = Arc::new(tokio::sync::Notify::new());
        refused.notify_one();
        let silent: StdoutStream = futures_util::stream::pending().boxed();
        let (_, errors) = collect(authorization_lines(silent, None, Some(refused))).await;
        assert!(errors.len() == 1 && errors[0].contains(acp::SIGN_IN_REQUIRED_MESSAGE), "{errors:?}");
    }
    #[test]
    fn stderr_yields_only_complete_lines_that_carry_a_valid_url() {
        let mut lines = StderrLines::default();
        let marker = format!("{}{}\n", acp::AUTH_BROWSER_MARKER, serde_json::to_string(URL).unwrap());
        let (first, second) = marker.split_at(40);
        assert!(lines.feed(first.as_bytes()).is_empty());
        assert_eq!(lines.feed(format!("{second}noise\n").as_bytes()), [URL.to_owned()]);
        assert_eq!(lines.feed(format!("{}{URL}\n", acp::AUTH_STDOUT_PREFIX).as_bytes()), [URL.to_owned()]);
        assert!(lines.feed(format!("{}\"https://evil.example/\"\n", acp::AUTH_BROWSER_MARKER).as_bytes()).is_empty());
    }
}
