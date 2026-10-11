//! Parsed thread projections kept between writes, so that a streamed delta does not read the thread.
//!
//! A thread is stored as a skeleton and a row for each element of its lists (`projection_rows`). Every write used to parse the
//! document (the adapter's planning read, then the reducer's), change a row of it and store it again, and every shell subscriber
//! then parsed it once more to build the thread's shell. This keeps the parsed document of the last write, with how it is stored,
//! so the next write and the subscribers read it from memory and a write stores the rows it changed. The database stays the truth;
//! the cache is an accelerator that is dropped whenever there is doubt:
//! - A write inside `Store::dispatch` takes the document out ([`take`]) and puts the changed one back with the same transaction
//!   ([`stage_changed`], with what the reducer touched). Staged documents join the cache only when the command committed
//!   ([`Stage::finish`]); a failed command drops the staged ones and the cache with them.
//! - Anything else that changes `rust_projections` through this connection (a plain `write_projection`, a statement of a test)
//!   fires the update hook and the cache is cleared before its next use. The rows of a thread (`rust_projection_rows`) are never
//!   written but together with its skeleton, so the hook, which SQLite does not call for that table, need not watch them. A commit made through another
//!   connection to the same file changes `PRAGMA data_version` and has the same effect.
//! - The cache holds at most [`BUDGET`] bytes of stored JSON, least recently used first, and no document above [`ENTRY_MAX`].
//!   What it measures is the document as stored, the size of what a cold read would parse (a parsed document takes several times
//!   that). The most recently used document is kept whatever its size up to [`ENTRY_MAX`]: a long thread is the one being
//!   streamed, and parsing it again for each delta is the cost this exists to avoid. Past that a document is read for every write.
use crate::projection_rows::{Held, Plan, Touched};
use rusqlite::Connection;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Stored JSON the cache may hold in all, besides the most recently used document.
const BUDGET: usize = 64 << 20;
/// A document larger than this is read on every write rather than kept.
const ENTRY_MAX: usize = 256 << 20;

type Key = (String, String);

struct Entry {
    held: Held,
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
    documents: HashMap<Key, Held>,
    /// The documents the command changed and has not yet written to the database, with what was touched: a command that writes
    /// the same thread for several events (a node and its item) stores it once, when the events are all reduced (`pending_writes`).
    pending: Vec<Key>,
    touched: HashMap<Key, Touched>,
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

    fn insert(&mut self, key: Key, held: Held) {
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.held.layout.bytes();
        }
        let len = held.layout.bytes();
        if len > ENTRY_MAX {
            return;
        }
        self.tick += 1;
        self.bytes += len;
        self.entries.insert(key, Entry { held, used: self.tick });
        while self.bytes > BUDGET && self.entries.len() > 1 {
            let Some(oldest) = self.entries.iter().min_by_key(|(_, entry)| entry.used).map(|(key, _)| key.clone()) else { break };
            if let Some(old) = self.entries.remove(&oldest) {
                self.bytes -= old.held.layout.bytes();
            }
        }
    }

    fn take(&mut self, key: &Key) -> Option<Held> {
        let entry = self.entries.remove(key)?;
        self.bytes -= entry.held.layout.bytes();
        Some(entry.held)
    }

    /// `read` on the document of `key`: the cached one, else the one `load` reads from the database (which is as committed, the
    /// connection being locked), kept for the next reader.
    pub(crate) fn get<R>(
        &mut self,
        connection: &Connection,
        key: &Key,
        load: impl FnOnce() -> Result<Option<Held>, crate::persistence::StoreError>,
        read: impl FnOnce(&serde_json::Value) -> R,
    ) -> Result<Option<R>, crate::persistence::StoreError> {
        #[cfg(test)]
        if crate::persistence::legacy::on() {
            return Ok(load()?.map(|held| read(&held.value)));
        }
        self.validate(connection);
        self.tick += 1;
        let tick = self.tick;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.used = tick;
            return Ok(Some(read(&entry.held.value)));
        }
        let Some(held) = load()? else { return Ok(None) };
        if held.layout.bytes() > ENTRY_MAX {
            return Ok(Some(read(&held.value)));
        }
        self.insert(key.clone(), held);
        let entry = self.entries.get(key).expect("just inserted");
        Ok(Some(read(&entry.held.value)))
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
        STAGE.with(|stage| *stage.borrow_mut() = Some(Staged { cache: cache.clone(), documents: HashMap::new(), pending: Vec::new(), touched: HashMap::new() }));
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
                    for (key, held) in staged.documents {
                        cache.insert(key, held);
                    }
                }
            }
            _ => cache.clear(),
        }
    }
}

/// Takes the document of `kind`/`id` from what the current command staged or the cache holds.
pub(crate) fn take(kind: &str, id: &str) -> Option<Held> {
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

/// Stages `held`, the new content of `kind`/`id` as stored (or unchanged), for the cache when the command commits.
pub(crate) fn stage(kind: &str, id: &str, held: Held) {
    STAGE.with(|stage| {
        if let Some(stage) = stage.borrow_mut().as_mut() {
            stage.documents.insert((kind.to_owned(), id.to_owned()), held);
        }
    });
}

/// Runs a write of the projection tables that is the cache's own.
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

/// Keeps `held` as the new content of `kind`/`id`, of which `touched` is what differs from what the database has, and notes that
/// the database does not have it yet. A document too large to be kept cannot wait, since it would be lost with the command: the
/// caller writes it then.
pub(crate) fn stage_changed(kind: &str, id: &str, held: Held, touched: Touched) -> Result<(), (Held, Touched)> {
    if held.layout.bytes() > ENTRY_MAX {
        return Err((held, touched));
    }
    STAGE.with(|stage| {
        let mut stage = stage.borrow_mut();
        let stage = stage.as_mut().expect("in a command");
        let key = (kind.to_owned(), id.to_owned());
        match stage.touched.get_mut(&key) {
            Some(earlier) => earlier.merge(touched),
            None => {
                stage.pending.push(key.clone());
                stage.touched.insert(key.clone(), touched);
            }
        }
        stage.documents.insert(key, held);
    });
    Ok(())
}

/// A document the command changed, with what to store of it.
pub(crate) struct Pending {
    pub key: Key,
    pub plan: Plan,
    /// What the database must hold once the plan is stored (debug builds check it).
    #[cfg(debug_assertions)]
    pub value: serde_json::Value,
}

/// What to store for the documents the command changed and has not written, with their keys: they stay staged (and are the cache's
/// if the command commits), with the layout of what the plans store. A document that is not staged at the moment (the reducer has
/// it out) keeps what was touched of it for when it is back.
pub(crate) fn pending_writes() -> Result<Vec<Pending>, crate::persistence::StoreError> {
    STAGE.with(|stage| {
        let mut stage = stage.borrow_mut();
        let Some(stage) = stage.as_mut() else { return Ok(Vec::new()) };
        let keys = std::mem::take(&mut stage.pending);
        let mut writes = Vec::new();
        for key in keys {
            let Some(held) = stage.documents.get_mut(&key) else {
                stage.pending.push(key);
                continue;
            };
            let touched = stage.touched.remove(&key).unwrap_or_else(Touched::all);
            let plan = crate::projection_rows::plan(&key.0, &key.1, &held.value, &mut held.layout, &touched)?;
            #[cfg(debug_assertions)]
            let value = held.value.clone();
            // Now its size is known: one that has outgrown the cache is written and not kept.
            if held.layout.bytes() > ENTRY_MAX {
                stage.documents.remove(&key);
            }
            writes.push(Pending {
                key,
                plan,
                #[cfg(debug_assertions)]
                value,
            });
        }
        Ok(writes)
    })
}

/// Whether the command has changed a document and not yet written it.
pub(crate) fn has_pending() -> bool {
    STAGE.with(|stage| stage.borrow().as_ref().is_some_and(|stage| !stage.pending.is_empty()))
}
