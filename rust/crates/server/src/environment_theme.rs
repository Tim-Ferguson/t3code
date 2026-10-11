//! The palettes this machine publishes for clients to follow (`environmentTheme.ts`). A desktop that retints its apps when
//! the user switches system theme writes `<state dir>/themes/<id>.json`; this service watches the directory and streams the
//! published set to the clients that asked for it (`subscribeServerConfig` with `environmentThemes`), so a theme change lands
//! without a restart. The filename is the theme id, which stays stable while the machine rewrites the colors underneath.
//! Theming is cosmetic, so every failure degrades to "not published". `t3 theme set|clear|show` (`cli_theme.rs`) is the
//! command that publishes into the same directory and names the environment's default.
//!
//! The service is attached to the store (`install`/`current`), as the other services the transport reaches are, so the shape
//! of `ApiState` does not change.
use crate::persistence::Store;
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use t3_contracts::{EnvironmentTheme, EnvironmentThemeFile, EnvironmentThemeId};
use tokio::sync::{mpsc, watch};

/// Bounds on what a machine can publish. The directory is local, so this is not a trust boundary, but an accidental dump of
/// large files there would otherwise be read in full, streamed to every client and repainted.
pub const MAX_THEME_FILES: usize = 32;
pub const MAX_THEME_FILE_BYTES: u64 = 32 * 1024;
/// The set travels whole to every subscriber, so the sum matters more than any single file.
pub const MAX_THEME_TOTAL_BYTES: usize = 192 * 1024;
const THEME_FILE_SUFFIX: &str = ".json";
/// How long the directory watch waits after an event before it re-reads (`Stream.debounce(Duration.millis(100))`).
pub const WATCH_DEBOUNCE: Duration = Duration::from_millis(100);

/// The palettes the clients ship (`BUILT_IN_THEME_IDS`).
pub const BUILT_IN_THEME_IDS: [&str; 5] = ["t3-chat", "grove", "ocean", "ember", "iris"];
/// The mobile default: a saved theme may carry it, but no client that follows published themes can resolve it.
const MOBILE_DEFAULT_THEME_ID: &str = "t3-code";

/// `UNPUBLISHABLE_THEME_IDS`: the appearance keywords a stored preference uses, every built-in, the legacy aliases older saves
/// still carry, and the mobile default. Taking one would be shadowed by the built-in or capture clients that never chose it.
pub fn is_unpublishable(id: &str) -> bool {
    matches!(id, "system" | "light" | "dark" | "t3-chat-dark" | "t3-grove" | "t3-ocean" | "t3-ember" | "t3-iris" | MOBILE_DEFAULT_THEME_ID) || BUILT_IN_THEME_IDS.contains(&id)
}

/// `isEnvironmentThemeId`: the client-side rule for a theme id (a lowercase id of at most 48 characters, not an appearance keyword).
pub fn is_environment_theme_id(id: &str) -> bool {
    EnvironmentThemeId::new(id).is_ok()
}

/// An id that names a theme a machine may publish.
pub fn is_publishable_id(id: &str) -> bool {
    is_environment_theme_id(id) && !is_unpublishable(id)
}

/// Reads a theme file through one opened handle, so every check binds to the file actually read rather than to a path that may
/// have been swapped since: `O_NOFOLLOW` rejects a symlink outright (a symlinked themes directory stays usable, a symlinked
/// file inside it does not), `O_NONBLOCK` keeps a FIFO from blocking the open, and the type and size gate examines the open
/// descriptor. `None` for anything that is not a small regular file (`readThemeFileGuarded`).
pub fn read_guarded(path: &Path, max_bytes: u64) -> Option<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    {
        // Windows has no such flag: the symlink check is an lstat before the open.
        if std::fs::symlink_metadata(path).ok()?.file_type().is_symlink() {
            return None;
        }
    }
    let file = options.open(path).ok()?;
    let info = file.metadata().ok()?;
    if !info.is_file() || info.len() > max_bytes {
        return None;
    }
    let mut contents = Vec::with_capacity(info.len() as usize);
    file.take(info.len()).read_to_end(&mut contents).ok()?;
    Some(String::from_utf8_lossy(&contents).into_owned())
}

/// Every theme the directory actually publishes. A file that is missing, unreadable, malformed, colorless or misnamed is
/// skipped; the rest of the set is unaffected. The one place that decides what "published" means (`readPublishedThemes`).
pub fn read_published(themes_dir: &Path) -> Vec<EnvironmentTheme> {
    let mut entries: Vec<String> = match std::fs::read_dir(themes_dir) {
        Ok(entries) => entries.filter_map(|entry| entry.ok()?.file_name().into_string().ok()).collect(),
        Err(_) => return Vec::new(),
    };
    // JavaScript's default sort compares UTF-16 code units; every name that gets past the id check below is ASCII.
    entries.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    let mut themes = Vec::new();
    let (mut examined, mut total_bytes) = (0usize, 0usize);
    for entry in entries {
        let Some(id) = entry.strip_suffix(THEME_FILE_SUFFIX) else { continue };
        // A reserved id is either shadowed by a built-in on the client or captures clients that never chose it.
        if !is_publishable_id(id) {
            continue;
        }
        // Counts files examined, not themes accepted: a directory of malformed files must not be opened, read and decoded in
        // full on every refresh and every client connect.
        examined += 1;
        if examined > MAX_THEME_FILES {
            tracing::warn!(path = %themes_dir.display(), limit = MAX_THEME_FILES, "ignoring environment theme files past the limit");
            break;
        }
        let path = themes_dir.join(&entry);
        let Some(raw) = read_guarded(&path, MAX_THEME_FILE_BYTES) else {
            tracing::warn!(path = %path.display(), limit = MAX_THEME_FILE_BYTES, "ignoring unusable environment theme file");
            continue;
        };
        if t3_contracts::trim_wire_string(&raw).is_empty() {
            continue;
        }
        let Some(file) = decode_file(&raw) else {
            tracing::warn!(path = %path.display(), "ignoring invalid environment theme");
            continue;
        };
        if !has_colors(&file) {
            tracing::warn!(path = %path.display(), "ignoring environment theme without colors");
            continue;
        }
        // Counted only once accepted, in bytes: the cap describes wire weight, and a skipped file must not eat the budget of
        // valid themes sorted after it.
        total_bytes += raw.len();
        if total_bytes > MAX_THEME_TOTAL_BYTES {
            tracing::warn!(path = %themes_dir.display(), limit = MAX_THEME_TOTAL_BYTES, "ignoring environment themes past the total size limit");
            break;
        }
        // The id is the filename; an `id` inside the file is ignored (the file's decoding does not carry one).
        let Ok(mut theme) = serde_json::to_value(&file) else { continue };
        theme.as_object_mut().expect("a theme file is an object").shift_insert(0, "id".into(), id.into());
        if let Ok(theme) = serde_json::from_value::<EnvironmentTheme>(theme) {
            themes.push(theme);
        }
    }
    themes
}

/// `Schema.fromJsonString(EnvironmentThemeFile)`: `JSON.parse` (a repeated key keeps the last value) and the schema's decoding.
pub fn decode_file(raw: &str) -> Option<EnvironmentThemeFile> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    // An optional key may be absent but not null (the schema's `optional` takes `undefined`, which JSON cannot carry).
    let null_in = |object: &serde_json::Value, keys: &[&str]| keys.iter().any(|key| object.get(*key).is_some_and(serde_json::Value::is_null));
    if null_in(&value, &["version", "canvas", "accent", "colors", "variants"]) || value.get("variants").is_some_and(|variants| null_in(variants, &["light", "dark"])) {
        return None;
    }
    let mut value = value;
    // A record keeps the entries whose key is role-shaped and drops the rest (so a role a newer build added never breaks an older
    // one); a file left with no colors at all is then refused by `has_colors`.
    let role = |key: &str| key.len() <= 64 && key.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) && key.bytes().all(|c| c.is_ascii_alphanumeric());
    let prune = |record: Option<&mut serde_json::Value>| {
        if let Some(serde_json::Value::Object(record)) = record {
            record.retain(|key, _| role(key));
        }
    };
    prune(value.get_mut("colors"));
    if let Some(variants) = value.get_mut("variants") {
        prune(variants.get_mut("light"));
        prune(variants.get_mut("dark"));
    }
    serde_json::from_value(value).ok()
}

/// `environmentThemeFileHasColors`: whether a theme file carries anything to render. A file with neither seeds nor colors would
/// show as the stock palette wearing a name, which reads as a bug rather than a theme.
pub fn has_colors(file: &EnvironmentThemeFile) -> bool {
    (matches!(file.canvas, Some(Some(_))) && matches!(file.accent, Some(Some(_)))) || matches!(&file.colors, Some(Some(colors)) if !colors.is_empty())
}

/// The published set with the sequence number it was observed at.
#[derive(Clone, Default)]
pub struct Published {
    pub seq: u64,
    pub themes: Arc<Vec<EnvironmentTheme>>,
}

struct Inner {
    dir: PathBuf,
    /// Holds the whole read, compare and publish, not just the state update: two refreshes finishing out of order would
    /// otherwise publish an older read under a higher sequence.
    refresh: tokio::sync::Mutex<()>,
    /// Every update carries the complete set, so a subscriber that stops consuming holds the newest set, not a backlog.
    published: watch::Sender<Published>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

#[derive(Clone)]
pub struct EnvironmentThemes(Arc<Inner>);

impl EnvironmentThemes {
    /// Creates the directory (so the watcher has something to attach to before the first publisher writes into it), seeds the
    /// state, and watches. The re-read follows an event after `debounce`: a theme script emits several events per save and a
    /// watch can fire before the content is flushed.
    pub async fn start(dir: PathBuf, debounce: Duration) -> Self {
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %error, path = %dir.display(), "could not create the environment themes directory");
        }
        let (published, _) = watch::channel(Published::default());
        let service = Self(Arc::new(Inner { dir, refresh: tokio::sync::Mutex::new(()), published, watcher: Mutex::new(None) }));
        // Seeds the dedupe so a watch event that reports no actual change does not retint every client.
        service.refresh().await;
        let (events, mut changes) = mpsc::unbounded_channel::<()>();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok() {
                let _ = events.send(());
            }
        });
        match watcher {
            Ok(mut watcher) => {
                use notify::Watcher;
                if let Err(error) = watcher.watch(&service.0.dir, notify::RecursiveMode::NonRecursive) {
                    tracing::warn!(error = %error, "could not watch the environment themes directory");
                } else {
                    *service.0.watcher.lock().unwrap() = Some(watcher);
                }
            }
            Err(error) => tracing::warn!(error = %error, "could not watch the environment themes directory"),
        }
        let weak: Weak<Inner> = Arc::downgrade(&service.0);
        tokio::spawn(async move {
            while changes.recv().await.is_some() {
                // Debounce: later events inside the window fold into this read.
                tokio::time::sleep(debounce).await;
                while changes.try_recv().is_ok() {}
                let Some(inner) = weak.upgrade() else { return };
                EnvironmentThemes(inner).refresh().await;
            }
        });
        service
    }

    /// Reads the directory and folds it into the sequenced state, publishing only a genuine change. Every reader goes through
    /// here, so the snapshot a client connects on and the events it then receives come from one ordered source.
    pub async fn refresh(&self) -> Published {
        let _turn = self.0.refresh.lock().await;
        let dir = self.0.dir.clone();
        let themes = tokio::task::spawn_blocking(move || read_published(&dir)).await.unwrap_or_default();
        let mut next = self.0.published.borrow().clone();
        if *next.themes != themes {
            next = Published { seq: next.seq + 1, themes: Arc::new(themes) };
            self.0.published.send_replace(next.clone());
        }
        next
    }

    /// The set published right now, read from disk rather than from the watcher's last observation.
    pub async fn current(&self) -> Arc<Vec<EnvironmentTheme>> {
        self.refresh().await.themes
    }

    /// The current set followed by every change, with repeats dropped. The subscription is acquired before the current set is
    /// read, so a publish landing while a client connects is delivered rather than lost.
    pub async fn subscribe(&self) -> ThemeStream {
        let changes = self.0.published.subscribe();
        let snapshot = self.refresh().await;
        ThemeStream { first: Some(snapshot.themes), after: snapshot.seq, changes }
    }
}

pub struct ThemeStream {
    first: Option<Arc<Vec<EnvironmentTheme>>>,
    after: u64,
    changes: watch::Receiver<Published>,
}

impl ThemeStream {
    /// The next set: the snapshot first, then each change newer than it. `None` when the service is gone. Cancel-safe.
    pub async fn next(&mut self) -> Option<Arc<Vec<EnvironmentTheme>>> {
        if let Some(first) = self.first.take() {
            return Some(first);
        }
        loop {
            self.changes.changed().await.ok()?;
            let update = self.changes.borrow_and_update().clone();
            if update.seq > self.after {
                self.after = update.seq;
                return Some(update.themes);
            }
        }
    }
}

#[derive(Default)]
struct Slot(std::sync::OnceLock<EnvironmentThemes>);

/// Installs the server's theme service; a server that never installed one publishes no themes.
pub fn install(store: &Store, themes: EnvironmentThemes) {
    let _ = store.extension(Slot::default).0.set(themes);
}

pub fn current(store: &Store) -> Option<EnvironmentThemes> {
    store.extension(Slot::default).0.get().cloned()
}

/// The `themeEvent` of `subscribeServerConfig`.
pub fn event(themes: &[EnvironmentTheme]) -> serde_json::Value {
    serde_json::json!({"version":1,"type":"environmentThemesUpdated","payload":{"themes":themes}})
}
