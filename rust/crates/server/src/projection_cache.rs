//! Parsed thread projections kept between writes, so that a streamed delta does not parse the whole thread.
//!
//! A thread is stored as one JSON document (`rust_projections`). Every write used to parse that document (the adapter's planning
//! read, then the reducer's), change a row of it, serialise it again and hand it to SQLite, and every shell subscriber then
//! parsed it once more to build the thread's shell. This keeps the parsed document of the last write, so the next write and the
//! subscribers read it from memory. The database stays the truth; the cache is an accelerator that is dropped whenever there is
//! doubt:
//! - A write inside `Store::dispatch` takes the document out ([`take`]) and puts the changed one back with the same transaction
//!   ([`stage`]). Staged documents join the cache only when the command committed ([`Stage::finish`]); a failed command drops
//!   the staged ones and the cache with them.
//! - Anything else that changes `rust_projections` through this connection (a plain `write_projection`, a statement of a test)
//!   fires the update hook and the cache is cleared before its next use. A commit made through another connection to the same
//!   file changes `PRAGMA data_version` and has the same effect.
//! - The cache holds at most [`BUDGET`] bytes of stored JSON, least recently used first, and no document above [`ENTRY_MAX`].
use rusqlite::Connection;
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Stored JSON the cache may hold in all. A parsed document takes several times its JSON in memory.
const BUDGET: usize = 32 << 20;
/// A document larger than this is parsed on every write rather than kept.
const ENTRY_MAX: usize = 16 << 20;

type Key = (String, String);

struct Entry {
    value: Value,
    len: usize,
    used: u64,
}

pub(crate) struct ProjectionCache {
    entries: HashMap<Key, Entry>,
    bytes: usize,
    tick: u64,
    /// `PRAGMA data_version` when the entries were read: another connection's commit changes it.
    data_version: Option<u32>,
    /// Set by the update hook when `rust_projections` changed by any other way than a staged write.
    external: Arc<AtomicBool>,
}

thread_local! {
    /// The command being dispatched on this thread, if any.
    static STAGE: RefCell<Option<Staged>> = const { RefCell::new(None) };
    /// Set while a staged write runs, so the update hook does not take it for an outside change.
    static OWN_WRITE: Cell<bool> = const { Cell::new(false) };
}

struct Staged {
    cache: Arc<Mutex<ProjectionCache>>,
    documents: HashMap<Key, (Value, usize)>,
    /// The documents the command changed and has not yet written to the database: a command that writes the same thread for
    /// several events (a node and its item) serialises it once, when the events are all reduced (`write_pending`).
    pending: Vec<Key>,
}

fn data_version(connection: &Connection) -> Option<u32> {
    connection.query_row("PRAGMA data_version", [], |row| row.get(0)).ok()
}

impl ProjectionCache {
    /// A cache for `connection`, which it watches for changes it did not make.
    pub(crate) fn watching(connection: &Connection) -> Self {
        let external = Arc::new(AtomicBool::new(false));
        let flag = external.clone();
        connection.update_hook(Some(move |_action, _database: &str, table: &str, _row| {
            if table == "rust_projections" && !OWN_WRITE.with(Cell::get) {
                flag.store(true, Ordering::Release);
            }
        }));
        Self { entries: HashMap::new(), bytes: 0, tick: 0, data_version: None, external }
    }

    /// Forgets what may be out of date. Called with the connection locked, before the cache is used.
    fn validate(&mut self, connection: &Connection) {
        let version = data_version(connection);
        if self.external.swap(false, Ordering::AcqRel) || version != self.data_version {
            self.clear();
        }
        self.data_version = version;
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn insert(&mut self, key: Key, value: Value, len: usize) {
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.len;
        }
        if len > ENTRY_MAX {
            return;
        }
        self.tick += 1;
        self.bytes += len;
        self.entries.insert(key, Entry { value, len, used: self.tick });
        while self.bytes > BUDGET {
            let Some(oldest) = self.entries.iter().min_by_key(|(_, entry)| entry.used).map(|(key, _)| key.clone()) else { break };
            if let Some(old) = self.entries.remove(&oldest) {
                self.bytes -= old.len;
            }
        }
    }

    fn take(&mut self, key: &Key) -> Option<(Value, usize)> {
        let entry = self.entries.remove(key)?;
        self.bytes -= entry.len;
        Some((entry.value, entry.len))
    }

    /// `read` on the document of `key`: the cached one, else the one `load` reads from the database (which is as committed, the
    /// connection being locked), kept for the next reader.
    pub(crate) fn get<R>(
        &mut self,
        connection: &Connection,
        key: &Key,
        load: impl FnOnce() -> Result<Option<(Value, usize)>, crate::persistence::StoreError>,
        read: impl FnOnce(&Value) -> R,
    ) -> Result<Option<R>, crate::persistence::StoreError> {
        #[cfg(test)]
        if crate::persistence::legacy::on() {
            return Ok(load()?.map(|(value, _)| read(&value)));
        }
        self.validate(connection);
        self.tick += 1;
        let tick = self.tick;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.used = tick;
            return Ok(Some(read(&entry.value)));
        }
        let Some((value, len)) = load()? else { return Ok(None) };
        if len > ENTRY_MAX {
            return Ok(Some(read(&value)));
        }
        self.insert(key.clone(), value, len);
        let entry = self.entries.get(key).expect("just inserted");
        Ok(Some(read(&entry.value)))
    }
}

/// The start of a command: the staged documents of it live here until it is known to have committed.
pub(crate) struct Stage {
    cache: Arc<Mutex<ProjectionCache>>,
}

impl Stage {
    /// Called with the connection locked, before the command's transaction.
    pub(crate) fn begin(cache: &Arc<Mutex<ProjectionCache>>, connection: &Connection) -> Self {
        if let Ok(mut guard) = cache.lock() {
            guard.validate(connection);
        }
        STAGE.with(|stage| *stage.borrow_mut() = Some(Staged { cache: cache.clone(), documents: HashMap::new(), pending: Vec::new() }));
        Stage { cache: cache.clone() }
    }

    /// The command ended: its staged documents become the cache's when it committed; otherwise nothing it touched is trusted.
    pub(crate) fn finish(self, committed: bool) {
        let staged = STAGE.with(|stage| stage.borrow_mut().take());
        let Ok(mut cache) = self.cache.lock() else { return };
        match staged {
            Some(staged) if committed => {
                // A statement outside the staged writes (a plain `write_projection` of the command) leaves the cache suspect.
                if cache.external.swap(false, Ordering::AcqRel) {
                    cache.clear();
                } else {
                    for (key, (value, len)) in staged.documents {
                        cache.insert(key, value, len);
                    }
                }
            }
            _ => cache.clear(),
        }
    }
}

/// Takes the document of `kind`/`id` from what the current command staged or the cache holds.
pub(crate) fn take(kind: &str, id: &str) -> Option<(Value, usize)> {
    #[cfg(test)]
    if crate::persistence::legacy::on() {
        return None;
    }
    let key = (kind.to_owned(), id.to_owned());
    STAGE.with(|stage| {
        let mut stage = stage.borrow_mut();
        let stage = stage.as_mut()?;
        let mut cache = stage.cache.lock().ok()?;
        // A projection the command wrote outside the staged path may differ from anything held: trust none of it, except what
        // the command changed and has not written yet, which is newer than the database.
        if cache.external.load(Ordering::Acquire) {
            let pending = &stage.pending;
            stage.documents.retain(|held, _| pending.contains(held));
            cache.clear();
        }
        if let Some(staged) = stage.documents.remove(&key) {
            return Some(staged);
        }
        cache.take(&key)
    })
}

/// Stages `value`, the new content of `kind`/`id` as stored (or unchanged), for the cache when the command commits.
pub(crate) fn stage(kind: &str, id: &str, value: Value, len: usize) {
    STAGE.with(|stage| {
        if let Some(stage) = stage.borrow_mut().as_mut() {
            stage.documents.insert((kind.to_owned(), id.to_owned()), (value, len));
        }
    });
}

/// Runs a write of `rust_projections` that is the cache's own.
pub(crate) fn own_write<R>(write: impl FnOnce() -> R) -> R {
    OWN_WRITE.with(|flag| flag.set(true));
    let result = write();
    OWN_WRITE.with(|flag| flag.set(false));
    result
}

/// Whether a command is being dispatched on this thread, so that a changed document can wait to be written.
pub(crate) fn in_command() -> bool {
    #[cfg(test)]
    if crate::persistence::legacy::on() {
        return false;
    }
    STAGE.with(|stage| stage.borrow().is_some())
}

/// Keeps `value` as the new content of `kind`/`id` and notes that the database does not have it yet. A document too large to be
/// kept cannot wait, since it would be lost with the command: the caller writes it then.
pub(crate) fn stage_changed(kind: &str, id: &str, value: Value, len: usize) -> Result<(), Value> {
    if len > ENTRY_MAX {
        return Err(value);
    }
    STAGE.with(|stage| {
        let mut stage = stage.borrow_mut();
        let stage = stage.as_mut().expect("in a command");
        let key = (kind.to_owned(), id.to_owned());
        if !stage.pending.contains(&key) {
            stage.pending.push(key.clone());
        }
        stage.documents.insert(key, (value, len));
    });
    Ok(())
}

/// The documents the command changed and has not written, taken out of the staged ones with their keys, for the caller to
/// write: they stay staged, and are the cache's if the command commits.
pub(crate) fn pending_writes() -> Vec<(Key, String)> {
    STAGE.with(|stage| {
        let mut stage = stage.borrow_mut();
        let Some(stage) = stage.as_mut() else { return Vec::new() };
        let keys = std::mem::take(&mut stage.pending);
        let mut writes = Vec::new();
        for key in keys {
            let Some(json) = stage.documents.get(&key).and_then(|(value, _)| serde_json::to_string(value).ok()) else { continue };
            // Now its size is known: one that has outgrown the cache is written and not kept.
            if json.len() > ENTRY_MAX {
                stage.documents.remove(&key);
            } else if let Some(document) = stage.documents.get_mut(&key) {
                document.1 = json.len();
            }
            writes.push((key, json));
        }
        writes
    })
}

/// Whether the command has changed a document and not yet written it.
pub(crate) fn has_pending() -> bool {
    STAGE.with(|stage| stage.borrow().as_ref().is_some_and(|stage| !stage.pending.is_empty()))
}
