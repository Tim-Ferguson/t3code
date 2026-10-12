//! The server browser's processes and the contexts its tabs live in (apps/server/src/preview/ServerBrowserContexts.ts).
//!
//! Persistent human profiles keep their storage on disk, each in a browser process of its own; isolated agents share one browser process,
//! never a context. A context is created once and reused until it closes; a closed one is forgotten and the next request makes another
//! without relaunching a browser that is still alive. Clearing a profile closes its context and deletes its storage while holding the
//! profile's place, so a tab asked for meanwhile opens after the storage is gone.
//!
//! TODO(original: the browser starts with the server's whole environment and no network policy). The browser is started with every variable
//! of this process and reaches whatever this machine reaches. Unlike the HTML renderer's browser (`headless_chrome::ProcessLauncher`, which
//! goes through the public-address proxy), nothing here limits where a page may connect; that is the launcher passed in, and a stricter
//! policy is a different launcher.
use crate::{
    cdp::{CdpConnection, CdpEvent},
    headless_chrome::{BrowserHandle, BrowserLauncher, HtmlRenderBrowserError, LaunchInput},
    preview_browser_host::{self, PreviewBrowserHostError},
    preview_browser_install::PreviewBrowserError,
    server_browser_page::{OpError, OpResult, Page, PageEvent},
};
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use t3_contracts::INCOGNITO_BROWSER_PROFILE_ID;
use tokio::sync::{mpsc, watch};

/// Why the browser could not be started.
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchFailure {
    /// The pinned browser is not installed (yet).
    Install(PreviewBrowserError),
    /// The host lacks the sandbox or the libraries the browser needs.
    Host(PreviewBrowserHostError),
    Browser(HtmlRenderBrowserError),
    Closed,
}

impl std::fmt::Display for LaunchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Install(error) => f.write_str(&error.message()),
            Self::Host(error) => f.write_str(&error.message()),
            Self::Browser(error) => f.write_str(&error.message()),
            Self::Closed => f.write_str("The preview browser is closed."),
        }
    }
}
impl std::error::Error for LaunchFailure {}

impl From<LaunchFailure> for OpError {
    fn from(failure: LaunchFailure) -> Self {
        OpError::execution(failure.to_string())
    }
}

/// The pinned headless shell, installed on first use.
#[async_trait]
pub trait BrowserExecutable: Send + Sync {
    async fn executable(&self) -> Result<PathBuf, PreviewBrowserError>;
}

#[async_trait]
impl BrowserExecutable for crate::preview_browser_install::PreviewBrowser {
    async fn executable(&self) -> Result<PathBuf, PreviewBrowserError> {
        crate::preview_browser_install::PreviewBrowser::executable(self).await
    }
}

pub struct ContextsOptions {
    pub profiles_dir: PathBuf,
    /// Where a finished download is kept until its tab claims it, and its tab's directory is made.
    pub downloads_dir: PathBuf,
    pub executable: Arc<dyn BrowserExecutable>,
    pub launcher: Arc<dyn BrowserLauncher>,
    /// Only an explicit operator opt-out turns the sandbox off. A launch error never does.
    pub no_sandbox: bool,
    /// The command `browser setup` is run with, as a host error names it.
    pub setup_command: String,
    /// Where the shared browser's throwaway profile is made.
    pub scratch_root: PathBuf,
}

/// A browser event the service reads, with the browser it came from.
#[derive(Debug, Clone)]
pub struct BrowserEvent {
    pub browser: u64,
    pub event: Arc<CdpEvent>,
}

/// A throwaway directory, removed with the value.
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running browser process and its DevTools connection.
pub struct BrowserProcess {
    id: u64,
    connection: CdpConnection,
    handle: Arc<dyn BrowserHandle>,
    _scratch: Option<Scratch>,
    shut_down: AtomicBool,
}

impl BrowserProcess {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn connection(&self) -> &CdpConnection {
        &self.connection
    }
    pub fn is_alive(&self) -> bool {
        !self.connection.is_closed() && !self.shut_down.load(Ordering::SeqCst)
    }
    /// Ends the browser and everything it started.
    pub async fn shutdown(&self) {
        if !self.shut_down.swap(true, Ordering::SeqCst) {
            self.handle.shutdown().await;
            self.connection.close();
        }
    }
}

/// Where a tab's page lives: a default context of a persistent profile's browser, or an isolated context of the shared one.
pub struct BrowserContext {
    id: u64,
    browser: Arc<BrowserProcess>,
    /// The protocol's id of an isolated context; none for the default context of a persistent profile's own browser.
    context_id: Option<String>,
    closed: watch::Sender<bool>,
}

impl std::fmt::Debug for BrowserContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserContext").field("id", &self.id).field("context_id", &self.context_id).finish()
    }
}

impl BrowserContext {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn browser(&self) -> &Arc<BrowserProcess> {
        &self.browser
    }
    pub fn context_id(&self) -> Option<&str> {
        self.context_id.as_deref()
    }
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow() || !self.browser.is_alive()
    }

    /// Resolves once the context has closed or its browser has gone.
    pub async fn closed(&self) {
        let mut closed = self.closed.subscribe();
        tokio::select! {
            _ = closed.wait_for(|closed| *closed) => {}
            () = self.browser.connection.closed() => {}
        }
    }

    /// A new blank page in this context.
    pub async fn new_page(&self, clipboard: bool) -> OpResult<(Page, mpsc::UnboundedReceiver<PageEvent>)> {
        let mut params = json!({"url": "about:blank"});
        if let Some(context_id) = &self.context_id {
            params["browserContextId"] = json!(context_id);
        }
        let created = self.browser.connection.send("Target.createTarget", params, None).await.map_err(crate::server_browser_page::map_cdp)?;
        let target = created["targetId"].as_str().ok_or_else(|| OpError::execution("The browser did not create a page."))?.to_owned();
        Page::attach(&self.browser.connection, &target, self.context_id.as_deref(), clipboard).await
    }

    /// Closes the context and its pages. A persistent profile's context is its whole browser.
    pub async fn close(&self) {
        if self.closed.send_replace(true) {
            return;
        }
        match &self.context_id {
            Some(context_id) => {
                let _ = self.browser.connection.send("Target.disposeBrowserContext", json!({"browserContextId": context_id}), None).await;
            }
            None => self.browser.shutdown().await,
        }
    }
}

type Slot = Arc<tokio::sync::Mutex<Option<Arc<BrowserContext>>>>;

struct Inner {
    options: ContextsOptions,
    slots: Mutex<HashMap<String, Slot>>,
    /// The browser the isolated contexts share, launched on first use.
    shared: tokio::sync::Mutex<Option<Arc<BrowserProcess>>>,
    closing: AtomicBool,
    next_id: AtomicU64,
    on_context_close: Mutex<Option<Arc<dyn Fn(Arc<BrowserContext>) + Send + Sync>>>,
    events: mpsc::UnboundedSender<BrowserEvent>,
    events_receiver: Mutex<Option<mpsc::UnboundedReceiver<BrowserEvent>>>,
}

/// Persistent human profiles keep their storage; isolated agents share a browser, never a context.
#[derive(Clone)]
pub struct ServerBrowserContexts(Arc<Inner>);

fn context_key(profile_id: &str, isolation_key: Option<&str>) -> String {
    json!([profile_id, isolation_key]).to_string()
}

impl ServerBrowserContexts {
    pub fn new(options: ContextsOptions) -> Self {
        let (events, events_receiver) = mpsc::unbounded_channel();
        Self(Arc::new(Inner {
            options,
            slots: Mutex::default(),
            shared: tokio::sync::Mutex::new(None),
            closing: AtomicBool::new(false),
            next_id: AtomicU64::new(0),
            on_context_close: Mutex::new(None),
            events,
            events_receiver: Mutex::new(Some(events_receiver)),
        }))
    }

    /// What the service learns of each context that closes, such as the tabs that lived in it.
    pub fn on_context_close(&self, callback: impl Fn(Arc<BrowserContext>) + Send + Sync + 'static) {
        *self.0.on_context_close.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(callback));
    }

    /// The browsers' own events (new targets, downloads), once: the service takes the stream.
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<BrowserEvent>> {
        self.0.events_receiver.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()
    }

    pub fn is_closing(&self) -> bool {
        self.0.closing.load(Ordering::SeqCst)
    }

    pub fn profiles_dir(&self) -> &Path {
        &self.0.options.profiles_dir
    }

    fn slot(&self, key: &str) -> Slot {
        self.0.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).entry(key.to_owned()).or_default().clone()
    }

    async fn launch(&self, profile_directory: &Path, scratch: Option<Scratch>) -> Result<Arc<BrowserProcess>, LaunchFailure> {
        let options = &self.0.options;
        let executable = options.executable.executable().await.map_err(LaunchFailure::Install)?;
        let input = LaunchInput { executable: executable.clone(), no_sandbox: options.no_sandbox, profile_directory: profile_directory.to_owned() };
        let failed = |error: HtmlRenderBrowserError| {
            let executable = executable.clone();
            let setup_command = options.setup_command.clone();
            async move {
                // Chrome's own account of why it stopped is what names the host setup that is missing.
                if let Some(output) = error.output.as_deref()
                    && let Some(host) = preview_browser_host::diagnose_launch_failure(&executable, output, &setup_command).await
                {
                    return LaunchFailure::Host(host);
                }
                LaunchFailure::Browser(error)
            }
        };
        let launched = match options.launcher.launch(&input).await {
            Ok(launched) => launched,
            Err(error) => return Err(failed(error).await),
        };
        // A browser that dies on startup shows it on the first command.
        if launched.connection.send("Target.setDiscoverTargets", json!({"discover": true}), None).await.is_err() {
            let error = launched.handle.exited().await;
            launched.handle.shutdown().await;
            return Err(failed(error).await);
        }
        let id = self.0.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let events = self.0.events.clone();
        launched.connection.on_event(None, move |event| {
            // Only the browser's own events: a page's go to the page.
            if event.session_id.is_none() {
                let _ = events.send(BrowserEvent { browser: id, event: Arc::new(event.clone()) });
            }
        });
        Ok(Arc::new(BrowserProcess { id, connection: launched.connection, handle: launched.handle, _scratch: scratch, shut_down: AtomicBool::new(false) }))
    }

    fn scratch_profile(&self) -> Result<Scratch, LaunchFailure> {
        let path = self.0.options.scratch_root.join(format!("t3-server-browser-{}", uuid::Uuid::new_v4().simple()));
        let mut directory = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut directory, 0o700);
        directory.recursive(true).create(&path).map_err(|error| LaunchFailure::Browser(HtmlRenderBrowserError { reason: "a temporary directory could not be created".into(), cause: Some(error.to_string()), output: None }))?;
        Ok(Scratch(path))
    }

    /// The browser the isolated contexts share. A launch that fails is forgotten, so a later request tries again with the sandbox still on.
    async fn shared_browser(&self) -> Result<Arc<BrowserProcess>, LaunchFailure> {
        let mut shared = self.0.shared.lock().await;
        if let Some(browser) = shared.as_ref().filter(|browser| browser.is_alive()) {
            return Ok(browser.clone());
        }
        let scratch = self.scratch_profile()?;
        let profile = scratch.0.join("profile");
        let browser = self.launch(&profile, Some(scratch)).await?;
        if self.is_closing() {
            browser.shutdown().await;
            return Err(LaunchFailure::Closed);
        }
        *shared = Some(browser.clone());
        Ok(browser)
    }

    /// The context for a profile, made on first use. An isolation key gives one context of its own in the shared browser.
    pub async fn context_for(&self, profile_id: &str, isolation_key: Option<&str>) -> Result<Arc<BrowserContext>, LaunchFailure> {
        if self.is_closing() {
            return Err(LaunchFailure::Closed);
        }
        let key = context_key(profile_id, isolation_key);
        let slot = self.slot(&key);
        let mut held = slot.lock().await;
        if let Some(context) = held.as_ref().filter(|context| !context.is_closed()) {
            return Ok(context.clone());
        }
        *held = None;
        let context = self.create_context(profile_id, isolation_key).await?;
        if self.is_closing() {
            context.close().await;
            return Err(LaunchFailure::Closed);
        }
        *held = Some(context.clone());
        // The slot forgets the context when it closes, and the service learns of it.
        let (watched, contexts, held_slot) = (context.clone(), self.clone(), slot.clone());
        tokio::spawn(async move {
            watched.closed().await;
            {
                // `try_lock`: a holder (a clear, or a request making a new context) is replacing the slot's content itself.
                if let Ok(mut held) = held_slot.try_lock()
                    && held.as_ref().is_some_and(|held| held.id == watched.id)
                {
                    *held = None;
                }
            }
            let callback = contexts.0.on_context_close.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
            if let Some(callback) = callback {
                callback(watched);
            }
        });
        Ok(context)
    }

    async fn create_context(&self, profile_id: &str, isolation_key: Option<&str>) -> Result<Arc<BrowserContext>, LaunchFailure> {
        let id = self.0.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let options = &self.0.options;
        let (browser, context_id) = if isolation_key.is_some() || profile_id == INCOGNITO_BROWSER_PROFILE_ID {
            let browser = self.shared_browser().await?;
            let created = browser.connection.send("Target.createBrowserContext", json!({"disposeOnDetach": false}), None).await.map_err(|error| LaunchFailure::Browser(HtmlRenderBrowserError::new(error.to_string())))?;
            let context_id = created["browserContextId"].as_str().ok_or_else(|| LaunchFailure::Browser(HtmlRenderBrowserError::new("the browser did not create a context")))?.to_owned();
            (browser, Some(context_id))
        } else {
            let directory = crate::server_browser_profiles::profile_directory(&options.profiles_dir, profile_id);
            // TODO(original: the profile directory is made with the process's default mode). Its parents and the directory itself are as
            // readable as the umask allows; what the browser writes inside is its own.
            std::fs::create_dir_all(&directory).map_err(|error| LaunchFailure::Browser(HtmlRenderBrowserError { reason: "the profile directory could not be created".into(), cause: Some(error.to_string()), output: None }))?;
            (self.launch(&directory, None).await?, None)
        };
        let context = Arc::new(BrowserContext { id, browser, context_id, closed: watch::channel(false).0 });
        // Downloads are kept by name, and pages may copy text out.
        let mut download = json!({"behavior": "allowAndName", "downloadPath": options.downloads_dir.join("incoming"), "eventsEnabled": true});
        let mut permissions = json!({"permissions": ["clipboardSanitizedWrite"]});
        if let Some(context_id) = &context.context_id {
            download["browserContextId"] = json!(context_id);
            permissions["browserContextId"] = json!(context_id);
        }
        let _ = tokio::fs::create_dir_all(options.downloads_dir.join("incoming")).await;
        for (method, params) in [("Browser.setDownloadBehavior", download), ("Browser.grantPermissions", permissions)] {
            if context.browser.connection.send(method, params, None).await.is_err() && !context.browser.is_alive() {
                return Err(LaunchFailure::Browser(context.browser.handle.exited().await));
            }
        }
        Ok(context)
    }

    /// A throwaway page in the shared browser, for work that must not touch a tab's own storage. Close it with `ScratchPage::close`.
    pub async fn scratch_page(&self) -> Result<ScratchPage, LaunchFailure> {
        if self.is_closing() {
            return Err(LaunchFailure::Closed);
        }
        let browser = self.shared_browser().await?;
        let created = browser.connection.send("Target.createBrowserContext", json!({"disposeOnDetach": false}), None).await.map_err(|error| LaunchFailure::Browser(HtmlRenderBrowserError::new(error.to_string())))?;
        let context_id = created["browserContextId"].as_str().unwrap_or_default().to_owned();
        let context = Arc::new(BrowserContext { id: 0, browser, context_id: Some(context_id), closed: watch::channel(false).0 });
        match context.new_page(false).await {
            Ok((page, events)) => Ok(ScratchPage { page, events: Some(events), context }),
            Err(error) => {
                context.close().await;
                Err(LaunchFailure::Browser(HtmlRenderBrowserError::new(error.message)))
            }
        }
    }

    /// Closes a human profile's context, ending its tabs, then deletes its storage. A tab asked for that profile meanwhile waits for the
    /// storage to be gone.
    pub async fn clear_profile(&self, profile_id: &str) -> std::io::Result<()> {
        if profile_id == INCOGNITO_BROWSER_PROFILE_ID {
            return Ok(());
        }
        let slot = self.slot(&context_key(profile_id, None));
        let mut held = slot.lock().await;
        if let Some(context) = held.take() {
            context.close().await;
        }
        crate::server_browser_profiles::clear_profile(&self.0.options.profiles_dir, profile_id).await
    }

    /// Closes every context and browser; later requests fail.
    pub async fn close(&self) {
        self.0.closing.store(true, Ordering::SeqCst);
        let slots: Vec<Slot> = self.0.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).values().cloned().collect();
        for slot in slots {
            let context = slot.lock().await.take();
            if let Some(context) = context {
                context.close().await;
            }
        }
        if let Some(browser) = self.0.shared.lock().await.take() {
            browser.shutdown().await;
        }
    }
}

/// A page in a context of its own, which goes with it.
pub struct ScratchPage {
    pub page: Page,
    pub events: Option<mpsc::UnboundedReceiver<PageEvent>>,
    context: Arc<BrowserContext>,
}

impl ScratchPage {
    pub async fn close(&self) {
        self.page.close().await;
        self.context.close().await;
    }
}

#[cfg(test)]
#[path = "server_browser_contexts_tests.rs"]
mod tests;
