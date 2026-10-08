//! Workspace browsing and the original pinned FFF native index, without Node.
use crate::workspace_files::{normalize, relative_target};
use fff_search::{
    FilePicker, FilePickerOptions, FuzzySearchOptions, GrepMode, GrepSearchOptions, MixedItemRef,
    PaginationArgs, QueryParser, SharedFilePicker, SharedFrecency,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};
use t3_contracts::*;

const MAX_ENTRIES: usize = 25_000;
const SCAN_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct EntriesError {
    message: String,
    context: Value,
    cause: Value,
}
impl EntriesError {
    pub fn rpc_error(&self, tag: &str, input: &Value) -> Value {
        let mut error = self.context.clone();
        error["_tag"] = json!(tag);
        let cwd = input["cwd"].as_str().unwrap_or_default();
        error["message"] = json!(match tag {
            "ProjectSearchEntriesError" =>
                format!("Failed to search workspace entries in '{cwd}'."),
            "ProjectSearchContentsError" =>
                format!("Failed to search workspace contents in '{cwd}'."),
            "ProjectListEntriesError" => format!("Failed to list workspace entries in '{cwd}'."),
            "FilesystemBrowseError" => format!(
                "Failed to browse filesystem path '{}'{}.",
                input["partialPath"].as_str().unwrap_or_default(),
                input["cwd"]
                    .as_str()
                    .map(|cwd| format!(" from '{cwd}'"))
                    .unwrap_or_default()
            ),
            _ => self.message.clone(),
        });
        if tag.starts_with("Project") {
            if let Some(root) = self.cause.get("normalizedWorkspaceRoot") {
                error["normalizedCwd"] = root.clone();
            } else if let Some(cwd) = self.cause.get("cwd").filter(|v| v.is_string()) {
                error["normalizedCwd"] = cwd.clone();
            }
            if self.context["failure"] == "read_directory_failed" {
                error["detail"] = json!(self.message);
            }
            if let Some(phase) = self.cause.get("phase") {
                error["detail"] = phase.clone();
            }
        }
        error["cause"] = self.cause.clone();
        if tag.starts_with("Project") && error["failure"] == "read_directory_failed" {
            error["failure"] = json!("directory_list_failed");
        }
        for field in ["cwd", "partialPath", "limit"] {
            if let Some(value) = input.get(field) {
                error[field] = value.clone();
            }
        }
        if let Some(query) = input["query"].as_str() {
            error["queryLength"] = json!(query.encode_utf16().count());
        }
        error
    }
}
fn error(
    failure: &str,
    tag: &str,
    message: String,
    fields: Value,
    cause: Option<String>,
) -> EntriesError {
    let mut context = fields.clone();
    context["failure"] = json!(failure);
    let mut nested = fields;
    nested["_tag"] = json!(tag);
    nested["message"] = json!(message);
    if tag.starts_with("WorkspaceSearchIndex") {
        if let Some(detail) = nested.as_object_mut().unwrap().remove("detail") {
            nested["reason"] = detail;
        }
    }
    if let Some(cause) = cause {
        nested["cause"] = json!({"name":"Error","message":cause});
    }
    EntriesError {
        message,
        context,
        cause: nested,
    }
}
fn directory_error(
    cwd: Option<&str>,
    partial: &str,
    parent: &Path,
    cause: impl std::fmt::Display,
) -> EntriesError {
    error(
        "read_directory_failed",
        "WorkspaceEntriesReadDirectoryError",
        format!(
            "Failed to read workspace directory '{}' while browsing '{partial}'{}.",
            parent.display(),
            cwd.map(|cwd| format!(" from '{cwd}'")).unwrap_or_default()
        ),
        json!({"parentPath":parent.to_string_lossy(),"partialPath":partial,"cwd":cwd}),
        Some(cause.to_string()),
    )
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Variant {
    Paths,
    Content,
}
type Slot = OnceLock<Result<Arc<Index>, EntriesError>>;
struct Cached {
    slot: Arc<Slot>,
    last_used: tokio::time::Instant,
    active: usize,
}
struct Inner {
    home: PathBuf,
    cwd: PathBuf,
    cache: Mutex<HashMap<(PathBuf, Variant), Cached>>,
    scan_timeout: Duration,
    idle_ttl: Duration,
    activity: Arc<tokio::sync::Notify>,
    cleanup: Mutex<Option<tokio::task::JoinHandle<()>>>,
    evictions: tokio::sync::watch::Sender<u64>,
    #[cfg(test)]
    before_index: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
}
impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(task) = self.cleanup.get_mut().unwrap().take() {
            task.abort();
        }
    }
}
struct CacheLease {
    owner: Weak<Inner>,
    key: (PathBuf, Variant),
    slot: Arc<Slot>,
}
impl Drop for CacheLease {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            {
                let mut cache = owner.cache.lock().unwrap();
                if let Some(entry) = cache
                    .get_mut(&self.key)
                    .filter(|entry| Arc::ptr_eq(&entry.slot, &self.slot))
                {
                    entry.active -= 1;
                    entry.last_used = tokio::time::Instant::now();
                }
            }
            owner.activity.notify_one();
        }
    }
}
struct IndexLease {
    index: Arc<Index>,
    _lease: CacheLease,
}
impl std::ops::Deref for IndexLease {
    type Target = Index;
    fn deref(&self) -> &Index {
        &self.index
    }
}
async fn expire_idle(owner: Weak<Inner>, activity: Arc<tokio::sync::Notify>) {
    loop {
        let changed = activity.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let Some(inner) = owner.upgrade() else { return };
        let (expired, deadline) = {
            let now = tokio::time::Instant::now();
            let mut cache = inner.cache.lock().unwrap();
            let stale: Vec<_> = cache
                .iter()
                .filter(|(_, entry)| entry.active == 0 && now >= entry.last_used + inner.idle_ttl)
                .map(|(key, _)| key.clone())
                .collect();
            let expired: Vec<_> = stale
                .into_iter()
                .filter_map(|key| cache.remove(&key))
                .collect();
            let deadline = cache
                .values()
                .filter(|entry| entry.active == 0)
                .map(|entry| entry.last_used + inner.idle_ttl)
                .min();
            (expired, deadline)
        };
        let count = expired.len();
        drop(expired);
        if count > 0 {
            inner.evictions.send_modify(|n| *n += count as u64);
        }
        drop(inner);
        if let Some(deadline) = deadline {
            tokio::select! { _ = tokio::time::sleep_until(deadline) => {}, _ = &mut changed => {} }
        } else {
            changed.await;
        }
    }
}
#[derive(Clone)]
pub struct WorkspaceEntries(Arc<Inner>);
struct Index {
    picker: SharedFilePicker,
    frecency: SharedFrecency,
    initial_timeout: bool,
    refresh: Mutex<()>,
}
impl Drop for Index {
    fn drop(&mut self) {
        if let Ok(mut picker) = self.picker.write() {
            picker.take();
        }
    }
}
impl Index {
    fn create(cwd: &Path, variant: Variant, timeout: Duration) -> Result<Arc<Self>, EntriesError> {
        let picker = SharedFilePicker::default();
        let frecency = SharedFrecency::default();
        FilePicker::new_with_shared_state(
            picker.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: cwd.to_string_lossy().into(),
                enable_mmap_cache: false,
                enable_content_indexing: variant == Variant::Content,
                enable_fs_root_scanning: true,
                enable_home_dir_scanning: true,
                ..Default::default()
            },
        )
        .map_err(|cause| {
            error(
                "search_index_create_failed",
                "WorkspaceSearchIndexCreateFailed",
                format!(
                    "Failed to create the workspace search index for '{}'.",
                    cwd.display()
                ),
                json!({"cwd":cwd.to_string_lossy(),"detail":cause.to_string()}),
                None,
            )
        })?;
        let initial_timeout = !picker.wait_for_indexing_complete(timeout);
        if initial_timeout && variant == Variant::Content {
            picker.write().unwrap().take();
            return Err(error(
                "search_index_scan_timed_out",
                "WorkspaceSearchIndexScanTimedOut",
                format!(
                    "Workspace search index for '{}' did not finish scanning within 15 seconds",
                    cwd.display()
                ),
                json!({"cwd":cwd.to_string_lossy(),"timeout":"15 seconds"}),
                None,
            ));
        }
        Ok(Arc::new(Self {
            picker,
            frecency,
            initial_timeout,
            refresh: Mutex::new(()),
        }))
    }
    fn incomplete(&self) -> bool {
        self.initial_timeout
            && self
                .picker
                .read()
                .ok()
                .and_then(|picker| {
                    picker
                        .as_ref()
                        .map(|picker| picker.get_scan_progress().is_scanning)
                })
                .unwrap_or(false)
    }
}
impl WorkspaceEntries {
    pub fn new(home: PathBuf, cwd: PathBuf) -> Self {
        let (evictions, _) = tokio::sync::watch::channel(0);
        let inner = Arc::new(Inner {
            home,
            cwd,
            cache: Mutex::new(HashMap::new()),
            scan_timeout: SCAN_TIMEOUT,
            idle_ttl: IDLE_TTL,
            activity: Arc::new(tokio::sync::Notify::new()),
            cleanup: Mutex::new(None),
            evictions,
            #[cfg(test)]
            before_index: Mutex::new(None),
        });
        let task = tokio::spawn(expire_idle(Arc::downgrade(&inner), inner.activity.clone()));
        *inner.cleanup.lock().unwrap() = Some(task);
        Self(inner)
    }

    #[allow(deprecated)]
    pub fn from_host() -> io::Result<Self> {
        Ok(Self::new(
            std::env::home_dir().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Host home directory is unavailable",
                )
            })?,
            std::env::current_dir()?,
        ))
    }
    fn expand(&self, value: &str) -> PathBuf {
        if value == "~" {
            self.0.home.clone()
        } else if value.starts_with("~/") || value.starts_with("~\\") {
            self.0.home.join(&value[2..])
        } else {
            PathBuf::from(value)
        }
    }
    fn resolve(&self, value: &str) -> PathBuf {
        let path = self.expand(value);
        normalize(&if path.is_absolute() {
            path
        } else {
            self.0.cwd.join(path)
        })
    }
    fn root(&self, cwd: &str) -> Result<PathBuf, EntriesError> {
        let root = self.resolve(trim_wire_string(cwd));
        match fs::metadata(&root) {
            Ok(stat) if stat.is_dir() => Ok(root),
            Ok(_) => Err(error(
                "workspace_root_not_directory",
                "WorkspaceRootNotDirectoryError",
                format!("Workspace root is not a directory: {}", root.display()),
                json!({"workspaceRoot":cwd,"normalizedWorkspaceRoot":root.to_string_lossy()}),
                None,
            )),
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => Err(error(
                "workspace_root_not_found",
                "WorkspaceRootNotExistsError",
                format!("Workspace root does not exist: {}", root.display()),
                json!({"workspaceRoot":cwd,"normalizedWorkspaceRoot":root.to_string_lossy()}),
                None,
            )),
            Err(cause) => Err(error(
                "workspace_root_stat_failed",
                "WorkspaceRootStatFailedError",
                format!(
                    "Failed to stat workspace root '{}' during 'validate-existing'.",
                    root.display()
                ),
                json!({"workspaceRoot":cwd,"normalizedWorkspaceRoot":root.to_string_lossy(),"phase":"validate-existing"}),
                Some(cause.to_string()),
            )),
        }
    }
    #[cfg(test)]
    pub(crate) fn block_next_index(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered, received) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        *self.0.before_index.lock().unwrap() = Some((entered, released));
        (received, release)
    }
    fn index(&self, cwd: &Path, variant: Variant) -> Result<IndexLease, EntriesError> {
        #[cfg(test)]
        if let Some((entered, release)) = self.0.before_index.lock().unwrap().take() {
            let _ = entered.send(());
            let _ = release.blocking_recv();
        }
        let key = (cwd.to_path_buf(), variant);
        let slot = {
            let mut cache = self.0.cache.lock().unwrap();
            let entry = cache.entry(key.clone()).or_insert_with(|| Cached {
                slot: Arc::new(OnceLock::new()),
                last_used: tokio::time::Instant::now(),
                active: 0,
            });
            entry.active += 1;
            entry.slot.clone()
        };
        let lease = CacheLease {
            owner: Arc::downgrade(&self.0),
            key: key.clone(),
            slot: slot.clone(),
        };
        let result = slot
            .get_or_init(|| Index::create(cwd, variant, self.0.scan_timeout))
            .clone();
        if result.is_err() {
            let mut cache = self.0.cache.lock().unwrap();
            if cache
                .get(&key)
                .is_some_and(|entry| Arc::ptr_eq(&entry.slot, &slot))
            {
                cache.remove(&key);
            }
        }
        result.map(|index| IndexLease {
            index,
            _lease: lease,
        })
    }
    pub fn browse(&self, input: &FilesystemBrowseInput) -> Result<Value, EntriesError> {
        self.browse_with(input, |path| {
            fs::read_dir(path)?
                .map(|entry| {
                    let entry = entry?;
                    Ok((
                        entry.file_name().to_string_lossy().into_owned(),
                        entry.file_type()?.is_dir(),
                    ))
                })
                .collect()
        })
    }
    fn browse_with(
        &self,
        input: &FilesystemBrowseInput,
        read: impl FnOnce(&Path) -> io::Result<Vec<(String, bool)>>,
    ) -> Result<Value, EntriesError> {
        let partial = input.partial_path.0.as_str();
        let cwd = input
            .cwd
            .as_ref()
            .and_then(Option::as_ref)
            .map(|cwd| cwd.0.as_str());
        let bytes = partial.as_bytes();
        let windows = partial.starts_with("\\\\")
            || (bytes.len() >= 2
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes.len() == 2 || matches!(bytes[2], b'/' | b'\\')));
        if !cfg!(windows) && windows {
            let platform = host_platform();
            let suffix = cwd
                .filter(|cwd| !cwd.is_empty())
                .map(|cwd| format!(" from '{cwd}'"))
                .unwrap_or_default();
            return Err(error(
                "windows_path_unsupported",
                "WorkspaceEntriesWindowsPathUnsupportedError",
                format!(
                    "Windows-style workspace path '{partial}' is not supported on '{platform}'{suffix}."
                ),
                json!({"partialPath":partial,"platform":platform,"cwd":cwd}),
                None,
            ));
        }
        let explicit = matches!(partial, "." | "..")
            || ["./", "../", ".\\", "..\\"]
                .iter()
                .any(|prefix| partial.starts_with(prefix));
        let resolved = if explicit {
            let cwd=cwd.ok_or_else(||error("current_project_required","WorkspaceEntriesCurrentProjectRequiredError",format!("A current project is required to browse relative workspace path '{partial}'."),json!({"partialPath":partial}),None))?;
            normalize(&self.resolve(cwd).join(partial))
        } else {
            self.resolve(partial)
        };
        let directory_mode = partial.ends_with(['/', '\\']) || partial == "~";
        let parent = if directory_mode {
            resolved.clone()
        } else {
            resolved.parent().unwrap_or(&resolved).to_path_buf()
        };
        let prefix = if directory_mode {
            String::new()
        } else {
            resolved
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        let children = match read(&parent) {
            Ok(entries) => entries,
            Err(cause) if cause.kind() == io::ErrorKind::PermissionDenied => vec![],
            Err(cause) => return Err(directory_error(cwd, partial, &parent, cause)),
        };
        let show_hidden = directory_mode || prefix.starts_with('.');
        let prefix = prefix.to_lowercase();
        let mut entries: Vec<_> = children
            .into_iter()
            .filter(|(name, directory)| {
                *directory
                    && name.to_lowercase().starts_with(&prefix)
                    && (show_hidden || !name.starts_with('.'))
            })
            .map(|(name, _)| json!({"fullPath":parent.join(&name).to_string_lossy(),"name":name}))
            .collect();
        entries.sort_by(|a, b| collate(a["name"].as_str().unwrap(), b["name"].as_str().unwrap()));
        Ok(json!({"parentPath":parent.to_string_lossy(),"entries":entries}))
    }
    pub fn search(&self, input: &ProjectSearchEntriesInput) -> Result<Value, EntriesError> {
        let root = self.root(input.cwd.as_str())?;
        let index = self.index(&root, Variant::Paths)?;
        let incomplete = index.incomplete();
        let guard = index
            .picker
            .read()
            .map_err(|cause| self.search_error(&root, cause))?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| self.search_error(&root, "index released"))?;
        let query = trim_wire_string(input.query.0.as_str())
            .trim_start_matches(['@', '.', '/'])
            .to_lowercase();
        let limit = input.limit.0 as usize;
        let image_only = input.image_only.flatten().unwrap_or(false);
        let kind = input.kind.flatten();
        let page_size = if image_only {
            MAX_ENTRIES + 2
        } else {
            limit + 1
        };
        let options = FuzzySearchOptions {
            project_path: Some(picker.base_path()),
            combo_boost_score_multiplier: 100,
            min_combo_count: 3,
            pagination: PaginationArgs {
                offset: 0,
                limit: page_size,
            },
            ..Default::default()
        };
        let (mut entries, truncated) = if kind == Some(ProjectEntryKind::File) || image_only {
            let parser = QueryParser::default();
            let query = parser.parse(&query);
            let result = picker.fuzzy_search(&query, None, options);
            let entries: Vec<_> = result
                .items
                .iter()
                .filter_map(|item| entry(item.relative_path(picker), "file"))
                .filter(|entry| !image_only || is_image(entry["path"].as_str().unwrap()))
                .collect();
            let truncated = entries.len() > limit || result.total_matched > result.items.len();
            (entries, truncated)
        } else if kind == Some(ProjectEntryKind::Directory) {
            let parser = QueryParser::new(fff_search::DirSearchConfig);
            let query = parser.parse(&query);
            let result = picker.fuzzy_search_directories(
                &query,
                FuzzySearchOptions {
                    combo_boost_score_multiplier: 0,
                    min_combo_count: 0,
                    ..options
                },
            );
            let roots = usize::from(
                result
                    .items
                    .iter()
                    .any(|item| item.relative_path(picker).is_empty()),
            );
            (
                result
                    .items
                    .iter()
                    .filter_map(|item| entry(item.relative_path(picker), "directory"))
                    .collect(),
                result.total_matched.saturating_sub(roots) > limit,
            )
        } else {
            let parser = QueryParser::new(fff_search::MixedSearchConfig);
            let query = parser.parse(&query);
            let result = picker.fuzzy_search_mixed(&query, None, options);
            mixed(result.items, picker, result.total_matched, limit)
        };
        entries.truncate(limit);
        Ok(json!({"entries":entries,"truncated":incomplete||truncated}))
    }
    fn search_error(&self, cwd: &Path, cause: impl std::fmt::Display) -> EntriesError {
        error(
            "search_index_search_failed",
            "WorkspaceSearchIndexSearchFailed",
            format!("Workspace search failed for '{}'.", cwd.display()),
            json!({"normalizedCwd":cwd.to_string_lossy(),"detail":cause.to_string()}),
            Some(cause.to_string()),
        )
    }
    pub fn list_index(&self, cwd: &str) -> Result<Value, EntriesError> {
        let root = self.root(cwd)?;
        let index = self.index(&root, Variant::Paths)?;
        let incomplete = index.incomplete();
        let guard = index
            .picker
            .read()
            .map_err(|cause| self.search_error(&root, cause))?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| self.search_error(&root, "index released"))?;
        let parser = QueryParser::new(fff_search::MixedSearchConfig);
        let query = parser.parse("");
        let result = picker.fuzzy_search_mixed(
            &query,
            None,
            FuzzySearchOptions {
                project_path: Some(picker.base_path()),
                combo_boost_score_multiplier: 100,
                min_combo_count: 3,
                pagination: PaginationArgs {
                    offset: 0,
                    limit: MAX_ENTRIES + 2,
                },
                ..Default::default()
            },
        );
        let (entries, truncated) = mixed(result.items, picker, result.total_matched, MAX_ENTRIES);
        let mut entries = entries;
        let mut seen: HashSet<String> = entries
            .iter()
            .map(|entry| entry["path"].as_str().unwrap().to_owned())
            .collect();
        for path in entries
            .iter()
            .map(|entry| entry["path"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
        {
            let mut parent = path.as_str();
            while let Some((ancestor, _)) = parent.rsplit_once('/') {
                if seen.insert(ancestor.into()) {
                    entries.push(json!({"path":ancestor,"kind":"directory"}));
                }
                parent = ancestor;
            }
        }
        entries.sort_by(|a, b| collate(a["path"].as_str().unwrap(), b["path"].as_str().unwrap()));
        let truncated = incomplete || truncated || entries.len() > MAX_ENTRIES;
        entries.truncate(MAX_ENTRIES);
        Ok(json!({"entries":entries,"truncated":truncated}))
    }
    pub async fn list(&self, input: &ProjectListEntriesInput) -> Result<Value, EntriesError> {
        let service = self.clone();
        let cwd = input.cwd.as_str().to_owned();
        let directory = input
            .directory_path
            .as_ref()
            .and_then(Option::as_ref)
            .map(|path| path.as_str().to_owned());
        let Some(directory) = directory else {
            return blocking(move || service.list_index(&cwd)).await;
        };
        let (root, mut entries) = blocking(move || {
            let root = service.root(&cwd)?;
            let failure = |cause: String| {
                directory_error(Some(&cwd), &directory, &root.join(&directory), cause)
            };
            let (target, relative) = if directory.is_empty() {
                (root.clone(), String::new())
            } else {
                relative_target(&root.to_string_lossy(), &directory)
                    .map_err(|cause| failure(cause.to_string()))?
            };
            let canonical_root =
                fs::canonicalize(&root).map_err(|cause| failure(cause.to_string()))?;
            let canonical =
                fs::canonicalize(&target).map_err(|cause| failure(cause.to_string()))?;
            if !canonical.starts_with(&canonical_root)
                || canonical
                    .strip_prefix(&canonical_root)
                    .unwrap()
                    .components()
                    .any(|part| part.as_os_str() == ".git")
                || relative.split('/').any(|part| part == ".git")
            {
                return Err(failure(
                    "Directory must be inside the workspace and outside .git.".into(),
                ));
            }
            let children = fs::read_dir(&canonical).map_err(|cause| failure(cause.to_string()))?;
            let mut entries = Vec::new();
            for child in children {
                let child = child.map_err(|cause| failure(cause.to_string()))?;
                let name = child.file_name().to_string_lossy().into_owned();
                let kind = child
                    .file_type()
                    .map_err(|cause| failure(cause.to_string()))?;
                if name == ".git" || !kind.is_file() && !kind.is_dir() {
                    continue;
                }
                let path = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                entries.push(json!({"path":path,"kind":if kind.is_dir(){"directory"}else{"file"}}));
            }
            Ok((root, entries))
        })
        .await?;
        let mut ignored = HashSet::new();
        for chunk in entries.chunks(1000) {
            let paths = chunk
                .iter()
                .map(|entry| entry["path"].as_str().unwrap())
                .collect::<Vec<_>>()
                .join("\0")
                + "\0";
            let Some(output) = git_ignore(&root, paths.into_bytes()).await else {
                break;
            };
            ignored.extend(
                output
                    .split(|byte| *byte == 0)
                    .filter_map(|path| std::str::from_utf8(path).ok())
                    .map(str::to_owned),
            );
        }
        for entry in &mut entries {
            if ignored.contains(entry["path"].as_str().unwrap()) {
                entry["ignored"] = json!(true);
            }
        }
        Ok(json!({"entries":entries,"truncated":false}))
    }
    pub fn search_contents(
        &self,
        input: &ProjectSearchContentsInput,
    ) -> Result<Value, EntriesError> {
        let root = self.root(input.cwd.as_str())?;
        let index = self.index(&root, Variant::Content)?;
        let guard = index
            .picker
            .read()
            .map_err(|cause| self.search_error(&root, cause))?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| self.search_error(&root, "index released"))?;
        let query = if input.case_sensitive {
            input.query.0.clone()
        } else if input.use_regex {
            format!("(?i){}", input.query.0)
        } else {
            input.query.0.to_lowercase()
        };
        let parsed = fff_search::grep::parse_grep_query(&query);
        let limit = input.limit.0 as usize;
        let page_size = if input.whole_word {
            limit.max(100)
        } else {
            limit
        };
        let deadline = Instant::now() + Duration::from_millis(250);
        let mut next = 0;
        let mut matches = vec![];
        let mut regex_error = None;
        loop {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_micros()
                .div_ceil(1000)
                .max(1) as u64;
            let result = picker.grep(
                &parsed,
                &GrepSearchOptions {
                    max_file_size: 10 * 1024 * 1024,
                    max_matches_per_file: 100.min(page_size),
                    smart_case: !input.case_sensitive && !input.use_regex,
                    file_offset: next,
                    page_limit: page_size,
                    mode: if input.use_regex {
                        GrepMode::Regex
                    } else {
                        GrepMode::PlainText
                    },
                    time_budget_ms: remaining,
                    ..Default::default()
                },
            );
            for found in result.matches {
                let ranges:Vec<_>=found.match_byte_offsets.iter().filter(|(start,end)|!input.whole_word||whole_word(&found.line_content,*start as usize,*end as usize)).map(|(start,end)|json!({"start":string_index(&found.line_content,*start as usize),"end":string_index(&found.line_content,*end as usize)})).collect();
                if ranges.is_empty() {
                    continue;
                }
                matches.push(json!({"path":result.files[found.file_index].relative_path(picker).replace('\\',"/"),"lineNumber":found.line_number,"lineContent":found.line_content,"matchRanges":ranges}));
            }
            next = result.next_file_offset;
            if regex_error.is_none() {
                regex_error = result.regex_fallback_error;
            }
            if matches.len() >= limit || next == 0 || Instant::now() >= deadline {
                break;
            }
        }
        let truncated = matches.len() > limit || next != 0;
        matches.truncate(limit);
        let mut result = json!({"matches":matches,"truncated":truncated});
        if let Some(error) = regex_error {
            result["regexFallbackError"] = json!(error);
        }
        Ok(result)
    }
    pub fn refresh(&self, cwd: &str) {
        let root = self.root(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
        let slots: Vec<_> = {
            let mut cache = self.0.cache.lock().unwrap();
            cache
                .iter_mut()
                .filter(|((path, _), entry)| {
                    path == &root && matches!(entry.slot.get(), Some(Ok(_)))
                })
                .map(|(key, value)| {
                    value.active += 1;
                    CacheLease {
                        owner: Arc::downgrade(&self.0),
                        key: key.clone(),
                        slot: value.slot.clone(),
                    }
                })
                .collect()
        };
        for lease in slots {
            let key = &lease.key;
            let slot = &lease.slot;
            let Some(Ok(index)) = slot.get() else {
                continue;
            };
            let _guard = index.refresh.lock().unwrap();
            let refreshed = index
                .picker
                .trigger_full_rescan_async(&index.frecency)
                .is_ok()
                && index.picker.wait_for_indexing_complete(self.0.scan_timeout);
            if !refreshed {
                tracing::warn!(cwd=%root.display(),"Failed to refresh workspace search index; invalidating it");
                let mut cache = self.0.cache.lock().unwrap();
                if cache
                    .get(key)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.slot, slot))
                {
                    cache.remove(key);
                }
            }
        }
    }
}
fn entry(path: String, kind: &str) -> Option<Value> {
    let path = path.replace('\\', "/");
    let path = path.strip_suffix('/').unwrap_or(&path);
    (!path.is_empty()).then(|| json!({"path":path,"kind":kind}))
}
fn mixed(
    items: Vec<MixedItemRef<'_>>,
    picker: &FilePicker,
    total: usize,
    limit: usize,
) -> (Vec<Value>, bool) {
    let roots =
        usize::from(items.iter().any(
            |item| matches!(item,MixedItemRef::Dir(dir)if dir.relative_path(picker).is_empty()),
        ));
    let entries = items
        .into_iter()
        .filter_map(|item| match item {
            MixedItemRef::File(item) => entry(item.relative_path(picker), "file"),
            MixedItemRef::Dir(item) => entry(item.relative_path(picker), "directory"),
        })
        .take(limit)
        .collect();
    (entries, total.saturating_sub(roots) > limit)
}
fn is_image(path: &str) -> bool {
    let path = path.to_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico", ".avif",
    ]
    .iter()
    .any(|extension| path.ends_with(extension))
}
fn string_index(line: &str, byte: usize) -> usize {
    String::from_utf8_lossy(&line.as_bytes()[..byte.min(line.len())])
        .encode_utf16()
        .count()
}
fn whole_word(line: &str, start: usize, end: usize) -> bool {
    if end <= start || !line.is_char_boundary(start) || !line.is_char_boundary(end) {
        return false;
    }
    let is_word = |character: Option<char>| {
        character.is_some_and(|character| {
            static WORD: OnceLock<regex::Regex> = OnceLock::new();
            WORD.get_or_init(|| regex::Regex::new(r"^[\p{Letter}\p{Mark}\p{Number}_]$").unwrap())
                .is_match(&character.to_string())
        })
    };
    let left = start == 0
        || !is_word(line[..start].chars().next_back())
        || !is_word(line[start..].chars().next());
    let right = end >= line.len()
        || !is_word(line[end..].chars().next())
        || !is_word(line[..end].chars().next_back());
    left && right
}
fn host_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}
// POSIX ICU locale precedence, with Node's en-US fallback for C/POSIX.
pub(crate) fn locale_from_environment() -> icu_locale::Locale {
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
pub(crate) fn collate(left: &str, right: &str) -> std::cmp::Ordering {
    static COLLATOR: OnceLock<icu_collator::CollatorBorrowed<'static>> = OnceLock::new();
    COLLATOR
        .get_or_init(|| {
            icu_collator::Collator::try_new(locale_from_environment().into(), Default::default())
                .expect("compiled ICU collation data")
        })
        .compare(left, right)
}
pub(crate) async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, EntriesError> + Send + 'static,
) -> Result<T, EntriesError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|cause| {
            error(
                "search_index_search_failed",
                "WorkspaceSearchIndexSearchFailed",
                "Workspace operation was interrupted.".into(),
                json!({}),
                Some(cause.to_string()),
            )
        })?
}
async fn git_ignore(cwd: &Path, paths: Vec<u8>) -> Option<Vec<u8>> {
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut child = tokio::process::Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "check-ignore",
            "-z",
            "--stdin",
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let write = async {
            stdin.write_all(&paths).await?;
            stdin.shutdown().await?;
            drop(stdin);
            Ok::<_, io::Error>(())
        };
        let read = async {
            let mut output = Vec::new();
            stdout
                .take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut output)
                .await?;
            if output.len() > 16 * 1024 * 1024 {
                return Err(io::Error::other("git ignore output exceeded byte budget"));
            }
            Ok(output)
        };
        tokio::try_join!(write, read, child.wait())
    })
    .await;
    match result {
        Ok(Ok(((), output, status))) if matches!(status.code(), Some(0 | 1)) => Some(output),
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn service(root: &Path) -> WorkspaceEntries {
        WorkspaceEntries::new(root.to_owned(), root.to_owned())
    }
    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn search_input(root: &Path, extra: Value) -> ProjectSearchEntriesInput {
        let mut value = json!({"cwd":root.to_string_lossy(),"query":"","limit":100});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }
    fn contents_input(root: &Path, extra: Value) -> ProjectSearchContentsInput {
        let mut value = json!({"cwd":root.to_string_lossy(),"query":"square","limit":100,"caseSensitive":false,"wholeWord":false,"useRegex":false});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }
    fn paths(value: &Value) -> Vec<String> {
        value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap().to_owned())
            .collect()
    }
    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn icu_sort_matches_node_default_locale_and_stable_unicode_ties() {
        let fixtures: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/workspace-collation.json"))
                .unwrap();
        for fixture in fixtures {
            let locale: icu_locale::Locale = fixture["locale"].as_str().unwrap().parse().unwrap();
            let collator =
                icu_collator::Collator::try_new(locale.into(), Default::default()).unwrap();
            let mut input: Vec<String> = serde_json::from_value(fixture["input"].clone()).unwrap();
            input.sort_by(|a, b| collator.compare(a, b));
            assert_eq!(json!(input), fixture["sorted"], "{}", fixture["locale"]);
        }
    }
    #[tokio::test]
    async fn browse_resolves_home_relative_prefix_hidden_and_structured_failures() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let service = service(root);
        for name in ["alpha", "Alpine", ".hidden", "日本語"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        write(root, "al-file.txt", "");
        let input =
            serde_json::from_value(json!({"partialPath":"./al","cwd":root.to_string_lossy()}))
                .unwrap();
        let result = service.browse(&input).unwrap();
        assert_eq!(
            result["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["alpha", "Alpine"]
        );
        let home = service
            .browse(&serde_json::from_value(json!({"partialPath":"~"})).unwrap())
            .unwrap();
        assert!(
            home["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == ".hidden")
        );
        let no_project = service
            .browse(&serde_json::from_value(json!({"partialPath":"./"})).unwrap())
            .unwrap_err();
        assert_eq!(no_project.context["failure"], "current_project_required");
        if !cfg!(windows) {
            let windows = service
                .browse(
                    &serde_json::from_value(json!({"partialPath":"C:\\work","cwd":"/project"}))
                        .unwrap(),
                )
                .unwrap_err();
            assert_eq!(
                windows.message,
                format!(
                    "Windows-style workspace path 'C:\\work' is not supported on '{}' from '/project'.",
                    host_platform()
                )
            );
            assert_eq!(windows.cause["platform"], host_platform());
        }
        let denied = service
            .browse_with(&input, |_| {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            })
            .unwrap();
        assert_eq!(denied["entries"], json!([]));
        let missing = service
            .browse_with(&input, |_| {
                Err(io::Error::new(io::ErrorKind::NotFound, "gone"))
            })
            .unwrap_err();
        assert_eq!(missing.cause["cause"]["message"], "gone");
    }
    #[tokio::test]
    async fn native_path_search_preserves_fuzzy_ignore_kind_and_image_before_limit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let service = service(root);
        git(root, &["init", "-q"]);
        write(root, ".gitignore", "ignored/\nnode_modules/\n.convex/\n");
        write(root, "ignored/no.ts", "");
        write(root, "node_modules/pkg/no.ts", "");
        write(root, ".convex/local-storage/no.ts", "");
        write(root, "src/components/Composer.tsx", "");
        for n in 0..205 {
            write(root, &format!("src/file-{n}.ts"), "");
        }
        write(root, "public/icon.svg", "");
        write(root, "public/not-image.bmp", "");
        let result = service
            .search(&search_input(root, json!({"query":"compoesr"})))
            .unwrap();
        assert!(paths(&result).contains(&"src/components/Composer.tsx".into()));
        let result = service
            .search(&search_input(
                root,
                json!({"imageOnly":true,"kind":"directory","limit":1}),
            ))
            .unwrap();
        assert_eq!(paths(&result), vec!["public/icon.svg"]);
        assert_eq!(result["truncated"], false);
        let result = service
            .search(&search_input(root, json!({"limit":200})))
            .unwrap();
        let found = paths(&result);
        assert!(
            found.iter().all(|p| !p.starts_with("ignored/")
                && !p.starts_with("node_modules/")
                && !p.starts_with(".convex/")
                && !p.starts_with(".git/")),
            "{found:?}"
        );
        let result = service
            .search(&search_input(root, json!({"kind":"directory"})))
            .unwrap();
        assert!(paths(&result).contains(&"src/components".into()));
        assert!(
            result["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["kind"] == "directory")
        );
        serde_json::from_value::<ProjectSearchEntriesResult>(result).unwrap();
    }
    #[tokio::test]
    async fn immediate_directory_listing_includes_ignored_and_empty_directories_but_rejects_escape()
    {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let service = service(root);
        git(root, &["init", "-q"]);
        write(root, ".gitignore", "ignored.txt\n");
        write(root, "ignored.txt", "x");
        fs::create_dir(root.join("empty")).unwrap();
        write(root, "visible.txt", "x");
        let input =
            serde_json::from_value(json!({"cwd":root.to_string_lossy(),"directoryPath":""}))
                .unwrap();
        let result = service.list(&input).await.unwrap();
        assert!(paths(&result).contains(&"empty".into()));
        assert!(
            result["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["path"] == "ignored.txt" && e["ignored"] == true)
        );
        assert!(!paths(&result).contains(&".git".into()));
        for path in ["../outside", ".git", root.to_str().unwrap()] {
            assert!(
                service
                    .list(
                        &serde_json::from_value(
                            json!({"cwd":root.to_string_lossy(),"directoryPath":path})
                        )
                        .unwrap()
                    )
                    .await
                    .is_err()
            );
        }
        let result = service
            .list(&serde_json::from_value(json!({"cwd":root.to_string_lossy()})).unwrap())
            .await
            .unwrap();
        assert!(!paths(&result).contains(&"ignored.txt".into()));
        serde_json::from_value::<ProjectListEntriesResult>(result).unwrap();
    }
    #[tokio::test]
    async fn native_contents_match_case_unicode_ranges_whole_words_and_regex_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let service = service(root);
        write(
            root,
            "src/shapes.ts",
            "const square = 4;\nconst Square = 16;\nconst squareSize = 8;\n😀 square 𐐀square square𐐀\n",
        );
        let result = service
            .search_contents(&contents_input(root, json!({"wholeWord":true})))
            .unwrap();
        let rows = result["matches"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2]["matchRanges"], json!([{"start":3,"end":9}]));
        let result = service
            .search_contents(&contents_input(
                root,
                json!({"query":"Square","caseSensitive":true}),
            ))
            .unwrap();
        assert_eq!(result["matches"].as_array().unwrap().len(), 1);
        write(root, "regex.txt", "literal [x\n");
        service.refresh(root.to_str().unwrap());
        let result = service
            .search_contents(&contents_input(
                root,
                json!({"query":"[x","useRegex":true,"caseSensitive":true}),
            ))
            .unwrap();
        assert!(result["regexFallbackError"].is_string());
        assert_eq!(result["matches"].as_array().unwrap().len(), 1);
        serde_json::from_value::<ProjectSearchContentsResult>(result).unwrap();
    }
    #[tokio::test]
    async fn cached_path_and_content_indexes_refresh_after_owned_file_write() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let service = service(root);
        write(root, "before.txt", "before");
        let before = service.list_index(root.to_str().unwrap()).unwrap();
        assert_eq!(paths(&before), vec!["before.txt"]);
        assert_eq!(
            service
                .search_contents(&contents_input(root, json!({"query":"new needle"})))
                .unwrap()["matches"],
            json!([])
        );
        let write_input:ProjectWriteFileInput = serde_json::from_value(json!({"cwd":root.to_string_lossy(),"relativePath":"src/new.txt","contents":"new needle\n"})).unwrap();
        crate::workspace_files::write_file(&write_input).unwrap();
        service.refresh(root.to_str().unwrap());
        let after = service.list_index(root.to_str().unwrap()).unwrap();
        assert!(paths(&after).contains(&"src/new.txt".into()));
        assert!(paths(&after).contains(&"src".into()));
        let found = service
            .search_contents(&contents_input(root, json!({"query":"new needle"})))
            .unwrap();
        assert_eq!(found["matches"][0]["path"], "src/new.txt");
        assert_eq!(service.0.cache.lock().unwrap().len(), 2);
    }
    #[tokio::test]
    async fn native_whole_word_search_continues_filtered_pages_and_caps_dense_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let service = service(root);
        for n in 0..120 {
            write(root, &format!("filtered-{n:03}.txt"), "needles\n");
        }
        write(root, "zz-match.txt", "needle\n");
        let result = service
            .search_contents(&contents_input(
                root,
                json!({"query":"needle","wholeWord":true,"limit":1,"caseSensitive":true}),
            ))
            .unwrap();
        assert_eq!(result["matches"][0]["path"], "zz-match.txt");
        let dense = tempfile::tempdir().unwrap();
        write(dense.path(), "dense.txt", &"needle\n".repeat(300));
        write(dense.path(), "other.txt", "needle\n");
        let result = service
            .search_contents(&contents_input(
                dense.path(),
                json!({"query":"needle","limit":500,"caseSensitive":true}),
            ))
            .unwrap();
        let rows = result["matches"].as_array().unwrap();
        assert_eq!(
            rows.iter().filter(|e| e["path"] == "dense.txt").count(),
            100
        );
        assert_eq!(rows.iter().filter(|e| e["path"] == "other.txt").count(), 1);
    }
    #[tokio::test]
    async fn native_non_git_default_exclusions_and_tracked_gitignore_rules_match_source() {
        let non_git = tempfile::tempdir().unwrap();
        let service = service(non_git.path());
        write(non_git.path(), ".convex/local-storage/data.json", "{}");
        write(non_git.path(), "node_modules/package/index.js", "{}");
        write(non_git.path(), "src/keep.ts", "export {};");
        let result = service
            .search(&search_input(non_git.path(), json!({})))
            .unwrap();
        assert_eq!(paths(&result), vec!["src/keep.ts", "src"]);
        let tracked = tempfile::tempdir().unwrap();
        let root = tracked.path();
        git(root, &["init", "-q"]);
        write(root, ".convex/local-storage/data.json", "{}");
        write(root, "src/keep.ts", "export {};");
        git(
            root,
            &["add", ".convex/local-storage/data.json", "src/keep.ts"],
        );
        write(root, ".gitignore", ".convex/\n");
        let result = service.search(&search_input(root, json!({}))).unwrap();
        assert!(!paths(&result).iter().any(|p| p.starts_with(".convex/")));
        assert!(paths(&result).contains(&"src/keep.ts".into()));
    }
    #[tokio::test]
    async fn project_error_mapping_preserves_source_context_and_nested_root_names() {
        let temp = tempfile::tempdir().unwrap();
        let service = service(temp.path());
        let input =
            json!({"cwd":temp.path().join("missing").to_string_lossy(),"query":"😀","limit":3});
        let cause = service
            .search(&serde_json::from_value(input.clone()).unwrap())
            .unwrap_err();
        let error = cause.rpc_error("ProjectSearchEntriesError", &input);
        assert_eq!(error["queryLength"], 2);
        assert_eq!(error["normalizedCwd"], input["cwd"]);
        assert_eq!(error["cause"]["normalizedWorkspaceRoot"], input["cwd"]);
        assert_eq!(
            error["message"],
            format!(
                "Failed to search workspace entries in '{}'.",
                input["cwd"].as_str().unwrap()
            )
        );
        serde_json::from_value::<ProjectSearchEntriesError>(error).unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn idle_timer_releases_native_index_without_another_request_and_waits_for_last_lease() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "file.txt", "hello");
        let service = service(root);
        let first = service.index(root, Variant::Paths).unwrap();
        let weak = Arc::downgrade(&first.index);
        let second = service.index(root, Variant::Paths).unwrap();
        drop(first);
        let mut evictions = service.0.evictions.subscribe();
        tokio::time::advance(IDLE_TTL + Duration::from_secs(1)).await;
        assert!(weak.upgrade().is_some());
        assert_eq!(service.0.cache.lock().unwrap().len(), 1);
        drop(second);
        tokio::time::advance(IDLE_TTL - Duration::from_secs(1)).await;
        assert!(weak.upgrade().is_some());
        tokio::time::advance(Duration::from_secs(1)).await;
        evictions.changed().await.unwrap();
        assert!(weak.upgrade().is_none());
        assert!(service.0.cache.lock().unwrap().is_empty());
        let weak_service = Arc::downgrade(&service.0);
        drop(service);
        assert!(weak_service.upgrade().is_none());
    }
    #[tokio::test]
    async fn returned_native_creation_diagnostic_has_no_invented_cause_and_can_retry() {
        let temp = tempfile::tempdir().unwrap();
        let service = service(temp.path());
        let missing = temp.path().join("missing");
        let failed = service.index(&missing, Variant::Paths).err().unwrap();
        assert_eq!(failed.context["failure"], "search_index_create_failed");
        assert!(failed.cause.get("cause").is_none());
        assert!(failed.cause["reason"].is_string());
        assert!(service.0.cache.lock().unwrap().is_empty());
        fs::create_dir(&missing).unwrap();
        assert!(service.index(&missing, Variant::Paths).is_ok());
    }
}
