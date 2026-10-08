use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("Database operation failed: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("Persisted JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Database lock was poisoned")]
    Poisoned,
    #[error("Effect {0} is not owned by this worker")]
    LeaseLost(String),
    #[error("Projection {kind}/{id} has invalid state: {detail}")]
    InvalidProjection {
        kind: String,
        id: String,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub event_id: String,
    pub aggregate_kind: String,
    pub aggregate_id: String,
    pub occurred_at: String,
    pub command_id: Option<String>,
    pub causation_event_id: Option<String>,
    pub correlation_id: Option<String>,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StoredEvent {
    pub sequence: u64,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub command_id: String,
    pub aggregate_kind: String,
    pub aggregate_id: String,
    pub command_type: String,
    pub accepted_at: String,
    pub result_sequence: u64,
    pub status: String,
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Effect {
    pub id: String,
    pub command_id: String,
    pub thread_id: String,
    pub request: Value,
    pub status: String,
    pub attempt_count: u64,
    pub available_at: String,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewEffect {
    pub id: String,
    pub command_id: String,
    pub thread_id: String,
    pub request: Value,
    pub available_at: String,
}

/// The native schema is isolated from the legacy database until migrations are ported.
/// A command's events, read models, receipt and effects commit together.
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    committed: tokio::sync::broadcast::Sender<Vec<StoredEvent>>,
}

pub enum Decision {
    Accepted {
        events: Vec<Event>,
        effects: Vec<NewEffect>,
    },
    Rejected(Value),
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS rust_application_events (
              sequence INTEGER PRIMARY KEY AUTOINCREMENT, event_id TEXT UNIQUE NOT NULL,
              aggregate_kind TEXT NOT NULL, aggregate_id TEXT NOT NULL, command_id TEXT,
              event_type TEXT NOT NULL, occurred_at TEXT NOT NULL, event_json TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS rust_events_aggregate ON rust_application_events(aggregate_kind,aggregate_id,sequence);
            CREATE TABLE IF NOT EXISTS rust_projections (
              aggregate_kind TEXT NOT NULL, aggregate_id TEXT NOT NULL, state_json TEXT NOT NULL,
              PRIMARY KEY(aggregate_kind,aggregate_id));
            CREATE TABLE IF NOT EXISTS rust_command_receipts (
              command_id TEXT PRIMARY KEY, receipt_json TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS rust_effect_outbox (
              id TEXT PRIMARY KEY, command_id TEXT NOT NULL, thread_id TEXT NOT NULL,
              request_json TEXT NOT NULL, status TEXT NOT NULL, attempt_count INTEGER NOT NULL DEFAULT 0,
              available_at TEXT NOT NULL, lease_owner TEXT, lease_expires_at TEXT,
              created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_error TEXT);
            CREATE INDEX IF NOT EXISTS rust_effect_claim ON rust_effect_outbox(status,available_at);")?;
        let (committed, _) = tokio::sync::broadcast::channel(256);
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            committed,
        })
    }

    /// Subscribe before reading a snapshot, then discard events through its sequence.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Vec<StoredEvent>> {
        self.committed.subscribe()
    }

    pub fn latest_sequence(&self) -> Result<u64, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        latest(&connection)
    }

    pub fn receipt(&self, command_id: &str) -> Result<Option<Receipt>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        read_receipt(&connection, command_id)
    }

    pub fn projection(&self, kind: &str, id: &str) -> Result<Option<Value>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        read_projection(&connection, kind, id)
    }

    pub fn projections(&self, kind: &str) -> Result<Vec<Value>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        read_projections(&connection, kind)
    }

    /// The lock and transaction cover state reads and pure planning as well as commit.
    /// Reusing a command ID returns its original receipt even if state has changed.
    pub fn dispatch(
        &self,
        command_id: &str,
        kind: &str,
        aggregate_id: &str,
        command_type: &str,
        now: DateTime<Utc>,
        decide: impl FnOnce(&Transaction<'_>) -> Result<Decision, StoreError>,
        reduce: impl Fn(&Transaction<'_>, &StoredEvent) -> Result<(), StoreError>,
    ) -> Result<Receipt, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(receipt) = read_receipt(&transaction, command_id)? {
            return Ok(receipt);
        }
        let mut stored = Vec::new();
        let (status, error) = match decide(&transaction)? {
            Decision::Rejected(error) => ("rejected", Some(error)),
            Decision::Accepted { events, effects } => {
                for event in events {
                    transaction.execute("INSERT INTO rust_application_events(event_id,aggregate_kind,aggregate_id,command_id,event_type,occurred_at,event_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                        params![event.event_id,event.aggregate_kind,event.aggregate_id,event.command_id,event.event_type,event.occurred_at,serde_json::to_string(&event)?])?;
                    let event = StoredEvent {
                        sequence: transaction.last_insert_rowid() as u64,
                        event,
                    };
                    reduce(&transaction, &event)?;
                    stored.push(event);
                }
                for effect in effects {
                    transaction.execute("INSERT OR IGNORE INTO rust_effect_outbox(id,command_id,thread_id,request_json,status,available_at,created_at,updated_at) VALUES(?1,?2,?3,?4,'pending',?5,?6,?6)",
                        params![effect.id,effect.command_id,effect.thread_id,serde_json::to_string(&effect.request)?,effect.available_at,now.to_rfc3339()])?;
                }
                ("accepted", None)
            }
        };
        let receipt = Receipt {
            command_id: command_id.into(),
            aggregate_kind: kind.into(),
            aggregate_id: aggregate_id.into(),
            command_type: command_type.into(),
            accepted_at: now.to_rfc3339(),
            result_sequence: latest(&transaction)?,
            status: status.into(),
            error,
        };
        transaction.execute(
            "INSERT INTO rust_command_receipts(command_id,receipt_json) VALUES(?1,?2)",
            params![command_id, serde_json::to_string(&receipt)?],
        )?;
        transaction.commit()?;
        // Publication must remain inside the command lock to preserve commit ordering.
        if !stored.is_empty() {
            let _ = self.committed.send(stored);
        }
        Ok(receipt)
    }

    pub fn events(
        &self,
        after: u64,
        through: Option<u64>,
        kind: Option<&str>,
        id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement = connection.prepare("SELECT sequence,event_json FROM rust_application_events WHERE sequence>?1 AND (?2 IS NULL OR sequence<=?2) AND (?3 IS NULL OR aggregate_kind=?3) AND (?4 IS NULL OR aggregate_id=?4) ORDER BY sequence LIMIT ?5")?;
        let rows = statement.query_map(
            params![after, through, kind, id, limit.min(10_000)],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )?;
        rows.map(|row| {
            let (sequence, json) = row?;
            Ok(StoredEvent {
                sequence,
                event: serde_json::from_str(&json)?,
            })
        })
        .collect()
    }

    /// Claim is transactional, so two workers never execute the same leased effect.
    pub fn claim_effect(
        &self,
        owner: &str,
        now: DateTime<Utc>,
        lease: chrono::Duration,
    ) -> Result<Option<Effect>, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let id: Option<String> = transaction.query_row("SELECT id FROM rust_effect_outbox WHERE status IN ('pending','failed') AND available_at<=?1 ORDER BY available_at,created_at,id LIMIT 1",[now.to_rfc3339()],|row| row.get(0)).optional()?;
        let Some(id) = id else { return Ok(None) };
        transaction.execute("UPDATE rust_effect_outbox SET status='running',attempt_count=attempt_count+1,lease_owner=?1,lease_expires_at=?2,updated_at=?3 WHERE id=?4",params![owner,(now+lease).to_rfc3339(),now.to_rfc3339(),id])?;
        let effect = read_effect(&transaction, &id)?;
        transaction.commit()?;
        Ok(effect)
    }

    pub fn finish_effect(
        &self,
        id: &str,
        owner: &str,
        now: DateTime<Utc>,
        outcome: Result<(), &str>,
        retry_at: Option<DateTime<Utc>>,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let (status, error) = match outcome {
            Ok(()) => ("succeeded", None),
            Err(error) => ("failed", Some(error)),
        };
        let affected = connection.execute("UPDATE rust_effect_outbox SET status=?1,last_error=?2,available_at=?3,updated_at=?4,lease_owner=NULL,lease_expires_at=NULL WHERE id=?5 AND status='running' AND lease_owner=?6",params![status,error,retry_at.unwrap_or(now).to_rfc3339(),now.to_rfc3339(),id,owner])?;
        if affected == 0 {
            return Err(StoreError::LeaseLost(id.into()));
        }
        Ok(())
    }

    /// Runtime-bound requests cannot be replayed after process loss. Recovery cancels
    /// those and puts safe effects back in pending state after their lease expires.
    pub fn recover_expired_effects(&self, now: DateTime<Utc>) -> Result<usize, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let pending: Vec<(String, String)> = {
            let mut statement = transaction.prepare("SELECT id,request_json FROM rust_effect_outbox WHERE status='running' AND lease_expires_at<=?1")?;
            statement
                .query_map([now.to_rfc3339()], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        for (id, json) in &pending {
            let request: Value = serde_json::from_str(json)?;
            let replay_safe = matches!(
                request["type"].as_str(),
                Some(
                    "provider-runtime.continue"
                        | "provider-session.detach"
                        | "provider-thread.rollback"
                        | "checkpoint.capture"
                        | "terminal.cleanup"
                        | "attachment.cleanup"
                        | "thread-title.generate"
                        | "delegated-tasks.stop"
                )
            );
            transaction.execute("UPDATE rust_effect_outbox SET status=?1,lease_owner=NULL,lease_expires_at=NULL,updated_at=?2 WHERE id=?3",params![if replay_safe {"pending"} else {"cancelled"},now.to_rfc3339(),id])?;
        }
        transaction.commit()?;
        Ok(pending.len())
    }

    pub fn effect(&self, id: &str) -> Result<Option<Effect>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        read_effect(&connection, id)
    }
}

fn latest(connection: &Connection) -> Result<u64, StoreError> {
    Ok(connection.query_row(
        "SELECT COALESCE(MAX(sequence),0) FROM rust_application_events",
        [],
        |row| row.get(0),
    )?)
}

fn read_receipt(connection: &Connection, id: &str) -> Result<Option<Receipt>, StoreError> {
    connection
        .query_row(
            "SELECT receipt_json FROM rust_command_receipts WHERE command_id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(Into::into)
}

pub fn read_projection(
    connection: &Connection,
    kind: &str,
    id: &str,
) -> Result<Option<Value>, StoreError> {
    connection
        .query_row(
            "SELECT state_json FROM rust_projections WHERE aggregate_kind=?1 AND aggregate_id=?2",
            params![kind, id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(Into::into)
}

pub fn read_projections(connection: &Connection, kind: &str) -> Result<Vec<Value>, StoreError> {
    let mut statement = connection.prepare("SELECT state_json FROM rust_projections WHERE aggregate_kind=?1 ORDER BY json_extract(state_json,'$.createdAt'),aggregate_id")?;
    statement
        .query_map([kind], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect()
}

pub fn write_projection(
    transaction: &Transaction<'_>,
    kind: &str,
    id: &str,
    state: &Value,
) -> Result<(), StoreError> {
    transaction.execute("INSERT INTO rust_projections(aggregate_kind,aggregate_id,state_json) VALUES(?1,?2,?3) ON CONFLICT(aggregate_kind,aggregate_id) DO UPDATE SET state_json=excluded.state_json",params![kind,id,serde_json::to_string(state)?])?;
    Ok(())
}

fn read_effect(connection: &Connection, id: &str) -> Result<Option<Effect>, StoreError> {
    let raw = connection.query_row("SELECT command_id,thread_id,request_json,status,attempt_count,available_at,lease_owner,lease_expires_at,created_at,updated_at,last_error FROM rust_effect_outbox WHERE id=?1",[id],|row| {
        Ok((Effect { id:id.into(),command_id:row.get(0)?,thread_id:row.get(1)?,request:Value::Null,status:row.get(3)?,attempt_count:row.get(4)?,available_at:row.get(5)?,lease_owner:row.get(6)?,lease_expires_at:row.get(7)?,created_at:row.get(8)?,updated_at:row.get(9)?,last_error:row.get(10)? },row.get::<_,String>(2)?))
    }).optional()?;
    raw.map(|(mut effect, json)| {
        effect.request = serde_json::from_str(&json)?;
        Ok(effect)
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn now() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }
    fn event() -> Event {
        Event {
            event_id: "event:1".into(),
            aggregate_kind: "thread".into(),
            aggregate_id: "thread:1".into(),
            occurred_at: now().to_rfc3339(),
            command_id: Some("command:1".into()),
            causation_event_id: None,
            correlation_id: None,
            event_type: "thread.created".into(),
            payload: json!({"id":"thread:1"}),
            metadata: json!({}),
        }
    }
    fn effect(id: &str, kind: &str) -> NewEffect {
        NewEffect {
            id: id.into(),
            command_id: "command:1".into(),
            thread_id: "thread:1".into(),
            request: json!({"type":kind}),
            available_at: now().to_rfc3339(),
        }
    }

    #[test]
    fn failed_projection_rolls_back_events_receipt_effects_and_publication() {
        let store = Store::memory().unwrap();
        let mut subscriber = store.subscribe();
        let outcome = store.dispatch(
            "command:1",
            "thread",
            "thread:1",
            "thread.create",
            now(),
            |_| {
                Ok(Decision::Accepted {
                    events: vec![event()],
                    effects: vec![effect("effect:1", "checkpoint.capture")],
                })
            },
            |transaction, _| {
                write_projection(transaction, "thread", "thread:1", &json!({"partial":true}))?;
                Err(StoreError::InvalidProjection {
                    kind: "thread".into(),
                    id: "thread:1".into(),
                    detail: "test failure".into(),
                })
            },
        );
        assert!(outcome.is_err());
        assert_eq!(store.latest_sequence().unwrap(), 0);
        assert!(store.receipt("command:1").unwrap().is_none());
        assert!(store.projection("thread", "thread:1").unwrap().is_none());
        assert!(store.effect("effect:1").unwrap().is_none());
        assert!(subscriber.try_recv().is_err());
    }

    #[test]
    fn publication_observes_all_committed_state_and_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite");
        let store = Store::open(&path).unwrap();
        let mut subscriber = store.subscribe();
        store
            .dispatch(
                "command:1",
                "thread",
                "thread:1",
                "thread.create",
                now(),
                |_| {
                    Ok(Decision::Accepted {
                        events: vec![event()],
                        effects: vec![effect("effect:1", "checkpoint.capture")],
                    })
                },
                |transaction, event| {
                    write_projection(transaction, "thread", "thread:1", &event.event.payload)
                },
            )
            .unwrap();
        assert_eq!(subscriber.try_recv().unwrap()[0].sequence, 1);
        drop(store);
        let recovered = Store::open(path).unwrap();
        assert_eq!(recovered.latest_sequence().unwrap(), 1);
        assert_eq!(
            recovered.receipt("command:1").unwrap().unwrap().status,
            "accepted"
        );
        assert_eq!(
            recovered.projection("thread", "thread:1").unwrap().unwrap(),
            json!({"id":"thread:1"})
        );
        assert_eq!(
            recovered.effect("effect:1").unwrap().unwrap().status,
            "pending"
        );
    }

    #[test]
    fn exclusive_leases_reject_stale_owners_and_recover_by_effect_policy() {
        let store = Store::memory().unwrap();
        store
            .dispatch(
                "command:1",
                "thread",
                "thread:1",
                "thread.create",
                now(),
                |_| {
                    Ok(Decision::Accepted {
                        events: vec![event()],
                        effects: vec![
                            effect("a-safe", "checkpoint.capture"),
                            effect("b-process", "provider-turn.start"),
                        ],
                    })
                },
                |_, _| Ok(()),
            )
            .unwrap();
        let first = store
            .claim_effect("worker-a", now(), chrono::Duration::seconds(30))
            .unwrap()
            .unwrap();
        let second = store
            .claim_effect("worker-b", now(), chrono::Duration::seconds(30))
            .unwrap()
            .unwrap();
        assert_ne!(first.id, second.id);
        assert!(
            store
                .claim_effect("worker-c", now(), chrono::Duration::seconds(30))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            store.finish_effect(&first.id, "worker-b", now(), Ok(()), None),
            Err(StoreError::LeaseLost(_))
        ));
        assert_eq!(
            store
                .recover_expired_effects(now() + chrono::Duration::seconds(31))
                .unwrap(),
            2
        );
        assert_eq!(store.effect("a-safe").unwrap().unwrap().status, "pending");
        assert_eq!(
            store.effect("b-process").unwrap().unwrap().status,
            "cancelled"
        );
        let claimed = store
            .claim_effect(
                "worker-c",
                now() + chrono::Duration::seconds(32),
                chrono::Duration::seconds(30),
            )
            .unwrap()
            .unwrap();
        assert_eq!(claimed.attempt_count, 2);
        store
            .finish_effect(
                &claimed.id,
                "worker-c",
                now() + chrono::Duration::seconds(33),
                Ok(()),
                None,
            )
            .unwrap();
        assert_eq!(
            store.effect(&claimed.id).unwrap().unwrap().status,
            "succeeded"
        );
    }
}
