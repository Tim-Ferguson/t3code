//! Secrets an agent asks the user for (port of `secrets/SecretRequests.ts`).
//!
//! The user's answer goes straight to the server's secret store under a one-use ref; orchestration
//! only records the request and its status. A tool that needs the value takes the ref and consumes
//! it, so the value never reaches the transcript, projections, clients, or model context. A value
//! nobody used within a day is dropped, at the first look and at each hourly sweep.
//!
//! The ref a request gets is derived from where it was asked (an HMAC of thread and card under the
//! server's own salt), so its length never depends on the thread id and the requesting tool finds
//! it again without a second record. It is unguessable without the salt.
use crate::{
    persistence::{Decision, Store, StoreError, read_projection},
    server_secret_store::ServerSecretStore,
};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::path::PathBuf;

pub const METHODS: [&str; 1] = ["secrets.answerRequest"];
const SECRET_REF_PREFIX: &str = "secret-ref:";
/// A value nobody used within this long is dropped; the agent can ask again.
const SECRET_REF_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Why answering a request or using its ref failed (`SecretRequestError`); the message is shown to
/// users and agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRequestError {
    pub reason: &'static str,
    pub cause: Option<String>,
}

impl SecretRequestError {
    fn new(reason: &'static str) -> Self {
        Self { reason, cause: None }
    }
    fn caused(reason: &'static str, cause: impl ToString) -> Self {
        Self { reason, cause: Some(cause.to_string()) }
    }
    pub fn message(&self) -> &'static str {
        match self.reason {
            "load_failed" => "Could not load the secret request.",
            "not_found" => "This secret request no longer exists.",
            "already_answered" => "This secret request was already answered.",
            "agent_stopped" => "The agent that asked has stopped, so this secret can't be used.",
            "store_failed" => "Could not store the secret.",
            "record_failed" => "Saved the secret, but could not update the request.",
            "invalid_ref" => "That secretRef is not valid.",
            "read_failed" => "Could not read the secret.",
            "ref_unavailable" => "That secretRef was already used or does not exist. Ask the user again with request_secret.",
            "ref_expired" => "That secretRef expired. Ask the user again with request_secret.",
            _ => "Could not use that secretRef. Try again.",
        }
    }
    pub fn wire(&self) -> Value {
        let mut wire = json!({"_tag":"SecretRequestError","reason":self.reason});
        if let Some(cause) = &self.cause {
            wire["cause"] = json!({"name":"Error","message":cause});
        }
        wire
    }
}

/// The per-server handle. The secret store is the state directory's `secrets` folder, the same
/// one that holds the asset signing key.
#[derive(Clone)]
pub struct SecretRequests {
    store: Store,
    secrets_dir: PathBuf,
}

/// The value as it is kept: the project it was entered for, the value, and when it was saved.
fn encode_stored(project_id: &str, value: &str, saved_at: i64) -> Vec<u8> {
    json!({"projectId":project_id,"value":value,"savedAt":saved_at}).to_string().into_bytes()
}
fn decode_stored(bytes: &[u8]) -> Option<(String, String, i64)> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    Some((value["projectId"].as_str()?.to_owned(), value["value"].as_str()?.to_owned(), value["savedAt"].as_i64()?))
}

fn is_valid_ref(reference: &str) -> bool {
    reference.strip_prefix(SECRET_REF_PREFIX).is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
}
/// Store name for a ref's value; refs are fixed-length hex, so names stay short and safe.
fn store_name(reference: &str) -> String {
    format!("secret-request-{}", &reference[SECRET_REF_PREFIX.len()..])
}

impl SecretRequests {
    /// `None` for a store without a state directory, which has nowhere to keep a secret.
    pub fn for_store(store: &Store) -> Option<Self> {
        Some(Self { store: store.clone(), secrets_dir: store.state_dir()?.join("secrets") })
    }

    fn secrets(&self) -> Result<ServerSecretStore, crate::server_secret_store::SecretStoreError> {
        ServerSecretStore::open(&self.secrets_dir)
    }

    /// The ref this request's answer is kept under.
    fn reference(&self, thread_id: &str, turn_item_id: &str) -> Result<String, crate::server_secret_store::SecretStoreError> {
        let salt = self.secrets()?.get_or_create_random("secret-request-salt", 32)?;
        let hex: String = salt.iter().map(|byte| format!("{byte:02x}")).collect();
        // The key is the salt's hex spelling, as the original passes a string to `createHmac`.
        let mut mac = Hmac::<Sha256>::new_from_slice(hex.as_bytes()).expect("HMAC accepts every key length");
        mac.update(format!("{thread_id}\0{turn_item_id}").as_bytes());
        let digest: String = mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(format!("{SECRET_REF_PREFIX}{}", &digest[..32]))
    }

    /// Removes a stored value; a failure is logged, since the value is still on disk.
    fn remove_logged(&self, name: &str) -> bool {
        match self.secrets().and_then(|secrets| secrets.remove(name)) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(error_tag = error.operation.tag(), "Could not delete a secret request value");
                false
            }
        }
    }

    /// Answers a pending request in a thread. Saving stores the value under the request's ref,
    /// which the requesting tool reads from the request's status.
    pub fn answer(&self, thread_id: &str, turn_item_id: &str, answer: &Value) -> Result<(), SecretRequestError> {
        let projection = self
            .store
            .projection("thread", thread_id)
            .map_err(|error| SecretRequestError::caused("load_failed", error))?
            .ok_or_else(|| SecretRequestError::caused("load_failed", "Thread not found."))?;
        let rows = |key: &str| projection[key].as_array().cloned().unwrap_or_default();
        let item = rows("turnItems").into_iter().find(|candidate| candidate["id"] == turn_item_id).filter(|item| item["type"] == "secret_request" && !item["runId"].is_null() && !item["nodeId"].is_null()).ok_or_else(|| SecretRequestError::new("not_found"))?;
        if item["secretStatus"] != "pending" {
            return Err(SecretRequestError::new("already_answered"));
        }
        // The agent is waiting inside the run that asked; once it has ended nobody will ever
        // receive the ref, so a value saved now would be lost.
        let run = rows("runs").into_iter().find(|candidate| candidate["id"] == item["runId"]);
        if run.as_ref().is_none_or(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "cancelled" | "interrupted" | "rolled_back"))) {
            return Err(SecretRequestError::new("agent_stopped"));
        }
        let run = run.expect("checked above");
        let reference = self.reference(thread_id, turn_item_id).map_err(|error| SecretRequestError::caused("store_failed", error))?;
        let name = store_name(&reference);
        let saving = answer["type"] == "save";
        // Store first: the card only says saved once the value is kept. Create, not set: a second
        // answer racing this one must not replace the value. A value already there on a card still
        // pending is an earlier save whose record failed and could not be cleaned up; recording it
        // finishes that save.
        if saving {
            let project_id = projection["thread"]["projectId"].as_str().unwrap_or_default();
            let bytes = encode_stored(project_id, answer["secret"].as_str().unwrap_or_default(), Utc::now().timestamp_millis());
            match self.secrets().and_then(|secrets| secrets.create(&name, &bytes)) {
                Ok(()) => {}
                Err(error) if error.already_exists() => {}
                Err(error) => return Err(SecretRequestError::caused("store_failed", error)),
            }
        }
        let status = if saving { "saved" } else { "declined" };
        if let Err(error) = self.record(thread_id, &item, &run, status) {
            // The card still says pending, so the user can save again; the value stored above
            // would make that retry look already answered.
            if saving {
                self.remove_logged(&name);
            }
            return Err(SecretRequestError::caused("record_failed", error));
        }
        if !saving {
            return Ok(());
        }
        // A request is answered once: if the agent's wait closed the card between the checks above
        // and this record, the record changed nothing. Nobody will receive the ref, so the value is
        // deleted rather than left to expire.
        let recorded = self.store.projection("thread", thread_id).map_err(|error| SecretRequestError::caused("record_failed", error))?;
        let card = recorded.as_ref().and_then(|projection| projection["turnItems"].as_array()).and_then(|items| items.iter().find(|candidate| candidate["id"] == turn_item_id));
        if card.is_some_and(|card| card["type"] == "secret_request" && card["secretStatus"] != "saved") {
            self.remove_logged(&name);
            return Err(SecretRequestError::new("agent_stopped"));
        }
        Ok(())
    }

    /// `secret_request.record`: the card for the request, now saved or declined. The command id
    /// names the card and the outcome, so a retried answer finds its receipt.
    fn record(&self, thread_id: &str, item: &Value, run: &Value, status: &str) -> Result<(), StoreError> {
        let command_id = format!("secret-request:{}:{status}", item["id"].as_str().unwrap_or_default());
        let now = Utc::now();
        self.store.dispatch(
            &command_id,
            "thread",
            thread_id,
            "secret_request.record",
            now,
            |transaction| {
                let projection = read_projection(transaction, "thread", thread_id)?.ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
                if run["rootNodeId"] != item["nodeId"] {
                    return Err(StoreError::InvalidCommand(format!("Node {} is not the root of run {}.", item["nodeId"].as_str().unwrap_or_default(), run["id"].as_str().unwrap_or_default())));
                }
                let existing = projection["turnItems"].as_array().and_then(|items| items.iter().find(|candidate| candidate["id"] == item["id"]));
                // A request is answered once: a retry finds its card as it was asked.
                let unchanged = existing.is_some_and(|existing| existing["secretStatus"] != "pending");
                let timestamp = crate::execution::at(now);
                let mut card = item.clone();
                if !unchanged {
                    card["status"] = json!(if status == "saved" { "completed" } else { "cancelled" });
                    card["completedAt"] = json!(timestamp);
                    card["updatedAt"] = json!(timestamp);
                    card["secretStatus"] = json!(status);
                } else if let Some(existing) = existing {
                    card = existing.clone();
                }
                let payload = crate::execution::checked::<t3_contracts::TurnItem>(card)?;
                Ok(Decision::Accepted { events: vec![crate::execution::event(thread_id, &command_id, "turn-item.updated", payload, now)], effects: vec![] })
            },
            crate::thread::reduce,
        )?;
        Ok(())
    }

    /// `secret_request.record` for the asking side (`request_secret`): the card in the asking run,
    /// `pending` while the user decides and `cancelled` once the agent stops waiting. It holds the
    /// request and its status only, never a value. A card already answered stays as answered, and
    /// a retry finds its card as it was asked.
    pub fn record_request(&self, command_id: &str, thread_id: &str, run_id: &str, node_id: &str, turn_item_id: &str, request: &Value, status: &str) -> Result<(), StoreError> {
        let now = Utc::now();
        self.store.dispatch(
            command_id,
            "thread",
            thread_id,
            "secret_request.record",
            now,
            |transaction| {
                let projection = read_projection(transaction, "thread", thread_id)?.ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
                let rows = |field: &str| projection[field].as_array().map(Vec::as_slice).unwrap_or(&[]);
                let run = rows("runs").iter().find(|run| run["id"] == run_id).filter(|run| run["rootNodeId"] == node_id).ok_or_else(|| StoreError::InvalidCommand(format!("Node {node_id} is not the root of run {run_id}.")))?;
                let existing = rows("turnItems").iter().find(|item| item["id"] == turn_item_id);
                if existing.is_some_and(|item| item["type"] != "secret_request") {
                    return Err(StoreError::InvalidCommand(format!("Turn item {turn_item_id} is not a secret request.")));
                }
                if existing.is_some_and(|item| item["runId"] != run_id) {
                    return Err(StoreError::InvalidCommand(format!("Secret request {turn_item_id} belongs to another run.")));
                }
                let timestamp = crate::execution::at(now);
                let pending = status == "pending";
                let unchanged = existing.is_some_and(|item| item["secretStatus"] != "pending" || pending);
                let card = if unchanged {
                    existing.unwrap().clone()
                } else {
                    let provider_turn = run["activeAttemptId"].as_str().and_then(|attempt| {
                        rows("providerTurns").iter().rev().find(|turn| turn["runAttemptId"] == attempt).or_else(|| rows("attempts").iter().find(|candidate| candidate["id"] == attempt).and_then(|candidate| rows("providerTurns").iter().find(|turn| turn["id"] == candidate["providerTurnId"])))
                    });
                    let ordinal = existing.map(|item| item["ordinal"].clone()).unwrap_or_else(|| json!(rows("turnItems").iter().filter_map(|item| item["ordinal"].as_u64()).max().map_or(0, |ordinal| ordinal + 1)));
                    let mut card = json!({"id":turn_item_id,"threadId":thread_id,"runId":run_id,"nodeId":node_id,"providerThreadId":run["providerThreadId"],"providerTurnId":provider_turn.map_or(Value::Null, |turn| turn["id"].clone()),"nativeItemRef":null,"parentItemId":null,"ordinal":ordinal,"status":if pending {"waiting"} else if status == "saved" {"completed"} else {"cancelled"},"title":request["label"],"startedAt":existing.map_or(json!(timestamp), |item| item["startedAt"].clone()),"completedAt":if pending {Value::Null} else {json!(timestamp)},"updatedAt":timestamp,"type":"secret_request","label":request["label"],"reason":request["reason"],"secretStatus":status});
                    if let Some(placeholder) = request.get("placeholder").filter(|placeholder| !placeholder.is_null()) {
                        card["placeholder"] = placeholder.clone();
                    }
                    card
                };
                let payload = crate::execution::checked::<t3_contracts::TurnItem>(card)?;
                Ok(Decision::Accepted { events: vec![crate::execution::event(thread_id, command_id, "turn-item.updated", payload, now)], effects: vec![] })
            },
            crate::thread::reduce,
        )?;
        Ok(())
    }

    /// The ref minted when this request was saved, for the tool that asked.
    pub fn saved_ref(&self, thread_id: &str, turn_item_id: &str) -> Option<String> {
        let reference = self.reference(thread_id, turn_item_id).ok()?;
        self.secrets().ok()?.get(&store_name(&reference)).ok().flatten().map(|_| reference)
    }

    /// Reads and deletes a ref's value. Fails for unknown or used refs, and for refs entered in
    /// another project.
    pub fn consume(&self, reference: &str, project_id: &str) -> Result<String, SecretRequestError> {
        let result = self.consume_ref(reference, project_id);
        let outcome = if result.is_ok() { "used" } else { "rejected" };
        crate::otlp::count("t3_secret_refs_consumed_total", "Secret refs tools tried to use, by result.", &[("result", outcome)], 1.0);
        result
    }

    fn consume_ref(&self, reference: &str, project_id: &str) -> Result<String, SecretRequestError> {
        // Get and remove are separate store calls; one consumer at a time keeps two concurrent
        // calls from both reading a ref before either deletes it.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _consuming = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !is_valid_ref(reference) {
            return Err(SecretRequestError::new("invalid_ref"));
        }
        let name = store_name(reference);
        let stored = self.secrets().and_then(|secrets| secrets.get(&name)).map_err(|error| SecretRequestError::caused("read_failed", error))?;
        let Some((_, value, saved_at)) = stored.as_deref().and_then(decode_stored).filter(|(stored_project, ..)| stored_project == project_id) else {
            return Err(SecretRequestError::new("ref_unavailable"));
        };
        if Utc::now().timestamp_millis() - saved_at > SECRET_REF_TTL_MS {
            // The hourly sweep retries a removal that fails here.
            self.remove_logged(&name);
            return Err(SecretRequestError::new("ref_expired"));
        }
        // One use: the value moves into whatever consumed it. If it cannot be deleted, it is not
        // handed out, so a ref is never used twice.
        if !self.remove_logged(&name) {
            return Err(SecretRequestError::new("consume_failed"));
        }
        Ok(value)
    }

    /// Drops values nobody used before they expired, so an agent that never consumed its ref does
    /// not leave the user's secret on disk. Returns how many were removed.
    pub fn sweep_expired(&self) -> usize {
        let Ok(secrets) = self.secrets() else { return 0 };
        let now = Utc::now().timestamp_millis();
        let names: Vec<String> = std::fs::read_dir(secrets.directory())
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().to_string_lossy().strip_suffix(".bin").map(str::to_owned))
                    .filter(|name| name.strip_prefix("secret-request-").is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))))
                    .collect()
            })
            .unwrap_or_default();
        let mut removed = 0;
        for name in names {
            let stored = secrets.get(&name).ok().flatten().as_deref().and_then(decode_stored);
            if stored.is_some_and(|(_, _, saved_at)| now - saved_at <= SECRET_REF_TTL_MS) {
                continue;
            }
            if self.remove_logged(&name) {
                removed += 1;
            }
        }
        removed
    }
}

/// The thread, the turn item and the answer of a `secrets.answerRequest` payload.
fn decode_request(payload: &Value) -> Result<(&str, &str, &Value), Value> {
    let decode = |message: &str| json!({"_tag":"SchemaDecodeError","message":message});
    let thread = payload["threadId"].as_str().filter(|id| !t3_contracts::trim_wire_string(id).is_empty()).ok_or_else(|| decode("threadId is required."))?;
    let item = payload["turnItemId"].as_str().filter(|id| !t3_contracts::trim_wire_string(id).is_empty()).ok_or_else(|| decode("turnItemId is required."))?;
    let answer = &payload["answer"];
    match answer["type"].as_str() {
        Some("save") if answer["secret"].as_str().is_some_and(|secret| !t3_contracts::trim_wire_string(secret).is_empty()) => {}
        Some("decline") => {}
        _ => return Err(decode("answer must save a non-empty secret or decline.")),
    }
    Ok((thread, item, answer))
}
pub(crate) fn decode_answer(payload: &Value) -> Result<(), Value> {
    decode_request(payload).map(|_| ())
}

/// Decode, one answer, the original's error envelope.
pub async fn command(store: &Store, tag: &str, payload: Value) -> Result<Value, Value> {
    if tag != "secrets.answerRequest" {
        return Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag}));
    }
    let (thread, item, answer) = decode_request(&payload)?;
    let service = SecretRequests::for_store(store).ok_or_else(|| json!({"_tag":"NativeServiceUnavailableError","message":"Secret storage is not configured."}))?;
    let (thread, item, answer) = (thread.to_owned(), item.to_owned(), answer.clone());
    // The answer is trimmed like every wire string before it is kept.
    let answer = if answer["type"] == "save" { json!({"type":"save","secret":t3_contracts::trim_wire_string(answer["secret"].as_str().unwrap_or_default())}) } else { answer };
    tokio::task::spawn_blocking(move || service.answer(&thread, &item, &answer))
    .await
    .map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))?
    .map(|()| Value::Null)
    .map_err(|error| error.wire())
}

/// Sweeps once now, then hourly, so a value lingers at most an hour past its expiry. Ends with the
/// runtime.
pub fn start_sweeper(store: &Store) {
    let Some(service) = SecretRequests::for_store(store) else { return };
    tokio::spawn(async move {
        loop {
            let sweeper = service.clone();
            let _ = tokio::task::spawn_blocking(move || sweeper.sweep_expired()).await;
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
}
