//! How a thread's document is stored: a skeleton, and a row for each element of each of its lists.
//!
//! A thread is one JSON document in memory (`Value`; the readers of it are many), but its lists (`turnItems`, `messages`, `nodes`,
//! `visibleTurnItems`, `runs`, and the rest) grow with the thread, and a streamed delta changes one element of three of them.
//! Stored as one blob, each delta serialised and wrote the whole thread. So `rust_projections.state_json` holds the *skeleton*,
//! the document with every top-level list replaced by `{"$rows":N}` (N its length, in the place the list had), and
//! `rust_projection_rows` holds the lists, one row per element keyed by (kind, id, list, position). A read puts them together again
//! into the same `Value` the document was, order and content; a write stores the skeleton and only the rows the reducer touched.
//! A cold read costs what it did, the size of the thread; a write costs the size of what changed plus the skeleton, which grows
//! with the number of runs (the counters of `shellState`) and not with the number of items.
//!
//! The rows a write stores come from what the reducer reports it touched ([`Touched`]), which can be wrong by being too small. Three
//! things keep that from corrupting a thread: the safe answer is the default ([`Touched::all`] rewrites every list, and a list a
//! reducer cannot account for is rewritten whole); an element past the last stored position is always written, so an append cannot
//! be missed; and in debug builds every write is read back from the database and compared with the document it came from
//! ([`verify`]), which every test of the suite then does.
//!
//! What this module knows of the database: the skeleton and a document's rows are written in the same transaction, the layout of
//! what is stored is kept with the parsed document ([`Layout`], row sizes per list), and a document the cache holds is by
//! construction what the database holds. A document stored whole by an earlier build (lists inline) is read as it is, and its lists
//! are written out as rows the first time they are written; opening a database converts them all at once ([`migrate`]).
use crate::persistence::StoreError;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};

/// The key of the object that stands for a list in a skeleton.
const MARKER: &str = "$rows";

/// `WITHOUT ROWID`: a thread's rows are read in key order, and clustered they are one range of the table. SQLite's update hook,
/// which tells the projection cache that something else wrote, is not called for such a table, but nothing writes the rows without
/// the skeleton in the same run of statements: `execute` does both, and the hook sees the skeleton's row.
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS rust_projection_rows (
  aggregate_kind TEXT NOT NULL, aggregate_id TEXT NOT NULL, list TEXT NOT NULL, pos INTEGER NOT NULL, row_json TEXT NOT NULL,
  PRIMARY KEY(aggregate_kind,aggregate_id,list,pos)) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS rust_projection_rows_cascade AFTER DELETE ON rust_projections BEGIN
  DELETE FROM rust_projection_rows WHERE aggregate_kind=old.aggregate_kind AND aggregate_id=old.aggregate_id;
END;";

/// Only a thread's lists are rows: every other kind of projection is small and read whole.
fn splits(kind: &str) -> bool {
    kind == "thread"
}

/// What is stored of a document: the bytes of its skeleton and of each row of each list. The size of the document for the cache
/// (`bytes`), and what a write needs to know to store only what changed.
#[derive(Default, Clone, Debug)]
pub(crate) struct Layout {
    skeleton: usize,
    rows: usize,
    lists: HashMap<String, Vec<u32>>,
}
impl Layout {
    /// Stored bytes of the whole document.
    pub(crate) fn bytes(&self) -> usize {
        self.skeleton + self.rows
    }
}

/// A parsed document and how it is stored.
pub(crate) struct Held {
    pub value: Value,
    pub layout: Layout,
}

/// What a reducer changed in a document: the elements it wrote, the lists it rebuilt, or everything. The reducer says what it
/// knows; what it does not, it leaves to [`Touched::all`] or [`Touched::list`].
#[derive(Default, Clone, Debug)]
pub(crate) struct Touched {
    all: bool,
    /// `None` is a whole list.
    lists: HashMap<String, Option<BTreeSet<usize>>>,
}
impl Touched {
    /// The document is new or changed in ways nobody tracked: every list is written out.
    pub(crate) fn all() -> Self {
        Self { all: true, lists: HashMap::new() }
    }
    /// Element `index` of `list` was written (added, or replaced in place).
    pub(crate) fn row(&mut self, list: &str, index: usize) {
        if self.all {
            return;
        }
        if let Some(Some(rows)) = self.lists.get_mut(list) {
            rows.insert(index);
        } else if !self.lists.contains_key(list) {
            self.lists.insert(list.to_owned(), Some(BTreeSet::from([index])));
        }
    }
    /// `list` was rebuilt, or elements were removed from it or moved within it.
    pub(crate) fn list(&mut self, list: &str) {
        if !self.all {
            self.lists.insert(list.to_owned(), None);
        }
    }
    pub(crate) fn merge(&mut self, other: Touched) {
        if other.all {
            *self = other;
            return;
        }
        for (list, rows) in other.lists {
            match rows {
                None => self.list(&list),
                Some(rows) => rows.into_iter().for_each(|index| self.row(&list, index)),
            }
        }
    }
}

/// What a write stores, and the SQL to do it.
#[derive(Default)]
pub(crate) struct Plan {
    skeleton: String,
    /// Every row of the document goes first (it is new, or tracked by nobody).
    clear: bool,
    /// Lists whose stored rows, if any, are stale: stored whole by an earlier build, or dropped from the document.
    reset: Vec<String>,
    /// Rows past the new end of a list are removed.
    truncate: Vec<(String, usize)>,
    rows: Vec<(String, usize, String)>,
}
impl Plan {
    /// Bytes this write hands to SQLite.
    pub(crate) fn bytes(&self) -> usize {
        self.skeleton.len() + self.rows.iter().map(|(_, _, json)| json.len()).sum::<usize>()
    }
}

struct Marker(usize);
impl Serialize for Marker {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(MARKER, &self.0)?;
        map.end()
    }
}
/// The document with its lists stood for by markers, serialised without copying them.
struct Skeleton<'a>(&'a Map<String, Value>);
impl Serialize for Skeleton<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0 {
            match value {
                Value::Array(rows) => map.serialize_entry(key, &Marker(rows.len()))?,
                other => map.serialize_entry(key, other)?,
            }
        }
        map.end()
    }
}

fn invalid(kind: &str, id: &str, detail: impl Into<String>) -> StoreError {
    StoreError::InvalidProjection { kind: kind.into(), id: id.into(), detail: detail.into() }
}

/// What to store for `doc`, given what is stored of it (`layout`, updated to what will be stored) and what changed since.
pub(crate) fn plan(kind: &str, id: &str, doc: &Value, layout: &mut Layout, touched: &Touched) -> Result<Plan, StoreError> {
    let object = match doc {
        Value::Object(object) if splits(kind) => object,
        _ => {
            let skeleton = serde_json::to_string(doc)?;
            *layout = Layout { skeleton: skeleton.len(), ..Layout::default() };
            return Ok(Plan { skeleton, ..Plan::default() });
        }
    };
    let mut plan = Plan { skeleton: serde_json::to_string(&Skeleton(object))?, clear: touched.all, ..Plan::default() };
    if touched.all {
        *layout = Layout::default();
    }
    for (list, value) in object {
        match value {
            Value::Array(rows) => {
                // A list the layout does not know was stored whole by an earlier build, or not at all: it is written in full.
                let known = layout.lists.contains_key(list);
                let sizes = layout.lists.entry(list.clone()).or_default();
                let whole = !known || matches!(touched.lists.get(list), Some(None));
                if !known && !touched.all {
                    plan.reset.push(list.clone());
                }
                let stored_len = sizes.len();
                let first_new = if known { stored_len } else { 0 };
                let mut positions: Vec<usize> = Vec::new();
                if whole {
                    positions.extend(0..rows.len());
                } else {
                    if let Some(Some(rows_touched)) = touched.lists.get(list) {
                        positions.extend(rows_touched.range(..first_new.min(rows.len())));
                    }
                    // Whatever lies past the stored end is written whether or not the reducer said so.
                    positions.extend(first_new..rows.len());
                }
                if whole {
                    layout.rows -= sizes.drain(..).map(|size| size as usize).sum::<usize>();
                }
                for position in positions {
                    let json = serde_json::to_string(&rows[position])?;
                    let size = json.len() as u32;
                    match sizes.get_mut(position) {
                        Some(old) => {
                            layout.rows = layout.rows - *old as usize + size as usize;
                            *old = size;
                        }
                        None => {
                            layout.rows += size as usize;
                            sizes.push(size);
                        }
                    }
                    plan.rows.push((list.clone(), position, json));
                }
                if sizes.len() > rows.len() {
                    layout.rows -= sizes[rows.len()..].iter().map(|&size| size as usize).sum::<usize>();
                    sizes.truncate(rows.len());
                }
                if stored_len > rows.len() {
                    plan.truncate.push((list.clone(), rows.len()));
                }
            }
            Value::Object(fields) if fields.contains_key(MARKER) => {
                return Err(invalid(kind, id, format!("{list} holds a key the store keeps for itself")));
            }
            _ => {}
        }
    }
    // A list stored before and not in the document any more.
    let gone: Vec<String> = layout.lists.keys().filter(|list| !matches!(object.get(*list), Some(Value::Array(_)))).cloned().collect();
    for list in gone {
        if let Some(sizes) = layout.lists.remove(&list) {
            layout.rows -= sizes.iter().map(|&size| size as usize).sum::<usize>();
        }
        plan.reset.push(list);
    }
    layout.skeleton = plan.skeleton.len();
    Ok(plan)
}

/// Stores `plan` as the content of `kind`/`id`.
pub(crate) fn execute(connection: &Connection, kind: &str, id: &str, plan: &Plan) -> Result<(), StoreError> {
    connection
        .prepare_cached("INSERT INTO rust_projections(aggregate_kind,aggregate_id,state_json) VALUES(?1,?2,?3) ON CONFLICT(aggregate_kind,aggregate_id) DO UPDATE SET state_json=excluded.state_json")?
        .execute(params![kind, id, plan.skeleton])?;
    if plan.clear {
        connection.prepare_cached("DELETE FROM rust_projection_rows WHERE aggregate_kind=?1 AND aggregate_id=?2")?.execute(params![kind, id])?;
    }
    for list in &plan.reset {
        connection.prepare_cached("DELETE FROM rust_projection_rows WHERE aggregate_kind=?1 AND aggregate_id=?2 AND list=?3")?.execute(params![kind, id, list])?;
    }
    for (list, from) in &plan.truncate {
        connection.prepare_cached("DELETE FROM rust_projection_rows WHERE aggregate_kind=?1 AND aggregate_id=?2 AND list=?3 AND pos>=?4")?.execute(params![kind, id, list, *from as i64])?;
    }
    if !plan.rows.is_empty() {
        let mut upsert = connection.prepare_cached("INSERT INTO rust_projection_rows(aggregate_kind,aggregate_id,list,pos,row_json) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(aggregate_kind,aggregate_id,list,pos) DO UPDATE SET row_json=excluded.row_json")?;
        for (list, position, json) in &plan.rows {
            upsert.execute(params![kind, id, list, *position as i64, json])?;
        }
    }
    Ok(())
}

/// Stores `doc` as the whole content of `kind`/`id`, whatever was stored before.
pub(crate) fn write_whole(connection: &Connection, kind: &str, id: &str, doc: &Value) -> Result<Plan, StoreError> {
    let plan = plan(kind, id, doc, &mut Layout::default(), &Touched::all())?;
    execute(connection, kind, id, &plan)?;
    Ok(plan)
}

/// The document from its stored skeleton and rows; `None` when `kind`/`id` has none. Also the bytes read.
pub(crate) fn read(connection: &Connection, kind: &str, id: &str) -> Result<Option<(Held, usize)>, StoreError> {
    let Some(skeleton) = connection
        .prepare_cached("SELECT state_json FROM rust_projections WHERE aggregate_kind=?1 AND aggregate_id=?2")?
        .query_row(params![kind, id], |row| row.get::<_, String>(0))
        .optional()?
    else {
        return Ok(None);
    };
    assemble(connection, kind, id, &skeleton).map(Some)
}

/// `skeleton`, the stored `state_json` of `kind`/`id`, put together with its rows.
pub(crate) fn assemble(connection: &Connection, kind: &str, id: &str, skeleton: &str) -> Result<(Held, usize), StoreError> {
    let mut value: Value = serde_json::from_str(skeleton)?;
    let mut layout = Layout { skeleton: skeleton.len(), ..Layout::default() };
    let mut read = skeleton.len();
    if let Value::Object(object) = &mut value {
        let counts: Vec<(String, usize)> = object
            .iter()
            .filter_map(|(list, value)| value.as_object().filter(|fields| fields.len() == 1).and_then(|fields| fields.get(MARKER)).and_then(Value::as_u64).map(|count| (list.clone(), count as usize)))
            .collect();
        if !counts.is_empty() && splits(kind) {
            let mut statement = connection.prepare_cached("SELECT list,pos,row_json FROM rust_projection_rows WHERE aggregate_kind=?1 AND aggregate_id=?2 ORDER BY list,pos")?;
            let mut rows = statement.query(params![kind, id])?;
            // The rows come list by list, in order: each list is built as its rows pass, and parsed from the text SQLite holds.
            let mut finished: HashMap<String, Vec<Value>> = HashMap::new();
            let mut building: Option<(String, Vec<Value>, Vec<u32>)> = None;
            while let Some(row) = rows.next()? {
                let (list, position, json) = (row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?, row.get_ref(1)?.as_i64().map_err(rusqlite::Error::from)?, row.get_ref(2)?.as_str().map_err(rusqlite::Error::from)?);
                if building.as_ref().is_none_or(|(name, ..)| name != list) {
                    if let Some((name, values, sizes)) = building.take() {
                        layout.lists.insert(name.clone(), sizes);
                        finished.insert(name, values);
                    }
                    let capacity = counts.iter().find(|(name, _)| name == list).map(|(_, count)| *count);
                    // Rows of a list the skeleton does not name are left over from a document stored whole since.
                    building = capacity.map(|capacity| (list.to_owned(), Vec::with_capacity(capacity), Vec::with_capacity(capacity)));
                }
                let Some((name, values, sizes)) = building.as_mut() else { continue };
                if position as usize != values.len() {
                    return Err(invalid(kind, id, format!("{name} has no row {}", values.len())));
                }
                read += json.len();
                layout.rows += json.len();
                sizes.push(json.len() as u32);
                values.push(serde_json::from_str(json)?);
            }
            if let Some((name, values, sizes)) = building.take() {
                layout.lists.insert(name.clone(), sizes);
                finished.insert(name, values);
            }
            for (list, count) in counts {
                let values = finished.remove(&list).unwrap_or_default();
                if values.len() != count {
                    return Err(invalid(kind, id, format!("{list} should have {count} rows and has {}", values.len())));
                }
                layout.lists.entry(list.clone()).or_default();
                object.insert(list, Value::Array(values));
            }
        }
    }
    Ok((Held { value, layout }, read))
}

/// Debug builds: the document as the database now holds it must be the document the write came from, order and content. Whoever
/// reports a touched set that is too small is told which list and which element differ.
#[cfg(debug_assertions)]
pub(crate) fn verify(connection: &Connection, kind: &str, id: &str, expected: &Value) {
    let stored = read(connection, kind, id).expect("a written projection can be read").expect("a written projection exists").0.value;
    if serde_json::to_string(&stored).unwrap() == serde_json::to_string(expected).unwrap() {
        return;
    }
    let (Value::Object(stored), Value::Object(expected)) = (&stored, expected) else { panic!("{kind}/{id}: the stored projection differs from the one written") };
    for (key, wanted) in expected {
        let held = stored.get(key);
        if held == Some(wanted) {
            continue;
        }
        if let (Some(Value::Array(held)), Value::Array(wanted)) = (held, wanted) {
            let at = (0..held.len().max(wanted.len())).find(|&at| held.get(at) != wanted.get(at));
            panic!("{kind}/{id}: {key} was written with a row missing or stale at {at:?} (stored {} rows, expected {})", held.len(), wanted.len());
        }
        panic!("{kind}/{id}: {key} differs from what was written");
    }
    panic!("{kind}/{id}: the stored projection differs from the one written in the order of its keys");
}

/// Converts the thread documents an earlier build stored whole (lists inline) to skeleton and rows. One transaction, so an
/// interrupted conversion changes nothing; a database that has been converted is marked, so opening it costs one pragma.
pub(crate) fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    const DONE: i64 = 1;
    let version = |connection: &Connection| connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0));
    if version(connection)? >= DONE {
        return Ok(());
    }
    let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Another process may have converted it while this one waited for the lock.
    if version(&transaction)? >= DONE {
        return Ok(());
    }
    let ids: Vec<String> = transaction
        .prepare("SELECT aggregate_id FROM rust_projections WHERE aggregate_kind='thread'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    for id in ids {
        let skeleton: String = transaction.query_row("SELECT state_json FROM rust_projections WHERE aggregate_kind='thread' AND aggregate_id=?1", [&id], |row| row.get(0))?;
        let document: Value = serde_json::from_str(&skeleton)?;
        if document.as_object().is_some_and(|object| object.values().any(Value::is_array)) {
            write_whole(&transaction, "thread", &id, &document)?;
        }
    }
    transaction.pragma_update(None, "user_version", DONE)?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{Store, write_projection};
    use serde_json::json;

    fn document() -> Value {
        json!({
            "thread": {"id": "t", "tags": ["a", "b"]},
            "updatedAt": "2026-01-01T00:00:00.000Z",
            "runs": [],
            "messages": [{"id": "m1", "text": "hello"}, {"id": "m2", "text": "world"}, {"id": "m3", "text": "again"}],
            "shellState": {"items": 3, "runItems": {"r": 3}},
            "gap": null,
            "turnItems": [{"id": "i1"}, {"id": "i2"}]
        })
    }
    fn text(value: &Value) -> String {
        serde_json::to_string(value).unwrap()
    }
    fn count(store: &Store, sql: &str) -> i64 {
        store.read(|connection| Ok(connection.query_row(sql, [], |row| row.get(0))?)).unwrap()
    }
    fn stored_rows(store: &Store) -> i64 {
        count(store, "SELECT COUNT(*) FROM rust_projection_rows")
    }

    /// The same `Value` comes back, key order included, and the skeleton holds none of the lists' content.
    #[test]
    fn a_document_reads_back_as_it_was_written_and_its_skeleton_holds_no_rows() {
        let store = Store::memory().unwrap();
        store.transaction(|transaction| write_projection(transaction, "thread", "t", &document())).unwrap();
        assert_eq!(text(&store.projection("thread", "t").unwrap().unwrap()), text(&document()));
        let skeleton: String = store.read(|connection| Ok(connection.query_row("SELECT state_json FROM rust_projections", [], |row| row.get(0))?)).unwrap();
        assert!(skeleton.contains(r#""messages":{"$rows":3}"#) && skeleton.contains(r#""runs":{"$rows":0}"#) && !skeleton.contains("hello"), "{skeleton}");
        assert_eq!(stored_rows(&store), 5);
        // Other kinds are small and stay whole.
        store.transaction(|transaction| write_projection(transaction, "project", "p", &json!({"scripts": [1, 2]}))).unwrap();
        assert_eq!(stored_rows(&store), 5);
        assert_eq!(store.projection("project", "p").unwrap().unwrap(), json!({"scripts": [1, 2]}));
    }

    /// A write stores the skeleton and the rows that were touched, and whatever lies past the end of a list.
    #[test]
    fn a_write_stores_the_touched_rows_the_appended_ones_and_nothing_else() {
        let store = Store::memory().unwrap();
        store.transaction(|transaction| write_projection(transaction, "thread", "t", &document())).unwrap();
        let (mut held, _) = store.read(|connection| read(connection, "thread", "t")).unwrap().unwrap();
        let bytes_before = held.layout.bytes();
        // One row replaced.
        held.value["messages"][1]["text"] = json!("changed");
        let mut touched = Touched::default();
        touched.row("messages", 1);
        let plan = plan("thread", "t", &held.value, &mut held.layout, &touched).unwrap();
        assert_eq!(plan.rows.iter().map(|(list, position, _)| (list.as_str(), *position)).collect::<Vec<_>>(), [("messages", 1)]);
        assert!(plan.reset.is_empty() && plan.truncate.is_empty() && !plan.clear);
        store.transaction(|transaction| execute(transaction, "thread", "t", &plan)).unwrap();
        assert_eq!(held.layout.bytes(), bytes_before - 5 + 7, "the layout follows the row that grew");
        // Rows appended to two lists are stored although nobody said so.
        held.value["messages"].as_array_mut().unwrap().push(json!({"id": "m4"}));
        held.value["turnItems"].as_array_mut().unwrap().push(json!({"id": "i3"}));
        let plan_two = plan_for(&mut held, &Touched::default());
        assert_eq!(plan_two.rows.len(), 2);
        store.transaction(|transaction| execute(transaction, "thread", "t", &plan_two)).unwrap();
        // A row removed from the end is deleted from the table.
        held.value["messages"].as_array_mut().unwrap().truncate(2);
        let plan_three = plan_for(&mut held, &Touched::default());
        assert!(plan_three.rows.is_empty() && plan_three.truncate == [("messages".to_owned(), 2)]);
        store.transaction(|transaction| execute(transaction, "thread", "t", &plan_three)).unwrap();
        // A list rebuilt (touched whole) is rewritten, and rows past its new end go.
        *held.value["turnItems"].as_array_mut().unwrap() = vec![json!({"id": "z"})];
        let mut rebuilt = Touched::default();
        rebuilt.list("turnItems");
        let plan_four = plan_for(&mut held, &rebuilt);
        store.transaction(|transaction| execute(transaction, "thread", "t", &plan_four)).unwrap();
        assert_eq!(text(&store.projection("thread", "t").unwrap().unwrap()), text(&held.value));
        // A list that is gone from the document leaves no rows.
        held.value.as_object_mut().unwrap().shift_remove("messages");
        let plan_five = plan_for(&mut held, &Touched::default());
        store.transaction(|transaction| execute(transaction, "thread", "t", &plan_five)).unwrap();
        assert_eq!(text(&store.projection("thread", "t").unwrap().unwrap()), text(&held.value));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM rust_projection_rows WHERE list='messages'"), 0);
        // The sizes the layout holds are what the database holds.
        let (reread, _) = store.read(|connection| read(connection, "thread", "t")).unwrap().unwrap();
        assert_eq!(reread.layout.bytes(), held.layout.bytes());
    }
    fn plan_for(held: &mut Held, touched: &Touched) -> Plan {
        plan("thread", "t", &held.value, &mut held.layout, touched).unwrap()
    }

    /// A touched set that is too small must show: the write is read back and compared in debug builds.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "stale")]
    fn a_row_the_reducer_did_not_report_is_caught_when_the_write_is_checked() {
        let store = Store::memory().unwrap();
        store.transaction(|transaction| write_projection(transaction, "thread", "t", &document())).unwrap();
        let (mut held, _) = store.read(|connection| read(connection, "thread", "t")).unwrap().unwrap();
        held.value["messages"][0]["text"] = json!("changed without a word");
        let plan = plan("thread", "t", &held.value, &mut held.layout, &Touched::default()).unwrap();
        store
            .transaction(|transaction| {
                execute(transaction, "thread", "t", &plan)?;
                verify(transaction, "thread", "t", &held.value);
                Ok(())
            })
            .unwrap();
    }

    /// A document stored whole by an earlier build is read as it is, and its lists become rows the first time it is written,
    /// whatever rows were left in the table.
    #[test]
    fn a_document_with_its_lists_inline_is_read_and_converted_by_its_first_write() {
        let store = Store::memory().unwrap();
        store
            .read(|connection| {
                connection.execute("INSERT INTO rust_projections VALUES('thread','t',?1)", [text(&document())])?;
                connection.execute("INSERT INTO rust_projection_rows VALUES('thread','t','messages',9,'{\"stale\":true}')", [])?;
                Ok(())
            })
            .unwrap();
        let (mut held, _) = store.read(|connection| read(connection, "thread", "t")).unwrap().unwrap();
        assert_eq!(text(&held.value), text(&document()));
        held.value["messages"][0]["text"] = json!("changed");
        let mut touched = Touched::default();
        touched.row("messages", 0);
        let plan = plan("thread", "t", &held.value, &mut held.layout, &touched).unwrap();
        assert_eq!(plan.rows.len(), 5, "every list is written out, not just the touched row");
        store.transaction(|transaction| execute(transaction, "thread", "t", &plan)).unwrap();
        assert_eq!(stored_rows(&store), 5);
        assert_eq!(text(&store.projection("thread", "t").unwrap().unwrap()), text(&held.value));
    }

    #[test]
    fn a_key_the_store_keeps_for_itself_is_refused() {
        let store = Store::memory().unwrap();
        let outcome = store.transaction(|transaction| write_projection(transaction, "thread", "t", &json!({"thread": {}, "runs": [], "oops": {"$rows": 1}})));
        assert!(matches!(outcome, Err(StoreError::InvalidProjection { .. })), "{outcome:?}");
    }

    /// Removing a projection takes its rows with it, and a statement of the connection's own that changes a projection is seen by the
    /// cache.
    #[test]
    fn rows_follow_their_projection_and_a_statement_outside_the_cache_is_seen() {
        let store = Store::memory().unwrap();
        store.transaction(|transaction| write_projection(transaction, "thread", "t", &document())).unwrap();
        let first = store.with_projection("thread", "t", |view| view["messages"][2]["text"].clone()).unwrap().unwrap();
        assert_eq!(first, "again");
        let edited = json!({"thread": {"id": "t"}, "messages": [1, 2]}).to_string();
        store.read(|connection| Ok(connection.execute("UPDATE rust_projections SET state_json=?1", [edited])?)).unwrap();
        let second = store.with_projection("thread", "t", |view| view["messages"].clone()).unwrap().unwrap();
        assert_eq!(second, json!([1, 2]), "the cache saw a write to the skeleton");
        store.read(|connection| Ok(connection.execute("DELETE FROM rust_projections", [])?)).unwrap();
        assert_eq!(stored_rows(&store), 0);
        assert!(store.with_projection("thread", "t", |_| ()).unwrap().is_none());
    }

    fn earlier_build(path: &std::path::Path, documents: &[(&str, &str)]) {
        let connection = Connection::open(path).unwrap();
        connection.execute_batch("CREATE TABLE rust_projections (aggregate_kind TEXT NOT NULL, aggregate_id TEXT NOT NULL, state_json TEXT NOT NULL, PRIMARY KEY(aggregate_kind,aggregate_id));").unwrap();
        for (id, json) in documents {
            connection.execute("INSERT INTO rust_projections VALUES('thread',?1,?2)", [id, json]).unwrap();
        }
    }

    /// A database written before the rows existed opens, with its threads converted in one transaction and only once.
    #[test]
    fn opening_a_database_stored_whole_converts_it_once_and_a_failure_changes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rust-state.sqlite");
        let mut other = document();
        other["thread"]["id"] = json!("u");
        earlier_build(&path, &[("t", &text(&document())), ("u", &text(&other))]);
        let store = Store::open(&path).unwrap();
        assert_eq!(text(&store.projection("thread", "t").unwrap().unwrap()), text(&document()));
        assert_eq!(text(&store.projection("thread", "u").unwrap().unwrap()), text(&other));
        assert_eq!(stored_rows(&store), 10);
        assert_eq!(count(&store, "PRAGMA user_version"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM rust_projections WHERE state_json LIKE '%hello%'"), 0, "the lists left the documents");
        drop(store);
        // Opened again, nothing is converted again (a row removed by hand stays removed).
        let connection = Connection::open(&path).unwrap();
        connection.execute("DELETE FROM rust_projection_rows WHERE list='turnItems' AND pos=1", []).unwrap();
        drop(connection);
        let store = Store::open(&path).unwrap();
        assert_eq!(stored_rows(&store), 8);
        drop(store);

        // A document that cannot be converted stops the whole conversion: the ones before it are as they were.
        let broken = directory.path().join("broken.sqlite");
        earlier_build(&broken, &[("a", &text(&document())), ("b", "not json")]);
        assert!(Store::open(&broken).is_err());
        let connection = Connection::open(&broken).unwrap();
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        let whole: String = connection.query_row("SELECT state_json FROM rust_projections WHERE aggregate_id='a'", [], |row| row.get(0)).unwrap();
        let rows: i64 = connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='rust_projection_rows'", [], |row| row.get(0)).unwrap();
        assert_eq!((version, whole, rows > 0), (0, text(&document()), true));
        let stored: i64 = connection.query_row("SELECT COUNT(*) FROM rust_projection_rows", [], |row| row.get(0)).unwrap();
        assert_eq!(stored, 0);
    }
}
