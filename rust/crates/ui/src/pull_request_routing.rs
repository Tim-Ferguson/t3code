//! Pull requests through another environment (`createPullRequestRouter` of `packages/client-runtime/src/state/pullRequestRouting.ts`):
//! when the person has let two machines share their signed-in GitHub accounts (the Connections page's "GitHub sharing"), a read or an act
//! on a pull request is tried on the machines that may answer it, nearest first, with the origin as the last resort for a read. Credentials
//! stay on their machines: the origin says which host and account the project uses (`pullRequests.routing`), each other machine proves it
//! has the same one (`pullRequests.routingIdentity`), and the request carries that account id so a machine whose signed-in account has
//! changed since refuses it before it starts. Every pull request panel asks through [`request`]; with nothing shared it is the plain
//! request to the window's environment.
use crate::{
    fleet,
    runtime::{self, TransportHandle, UiModel},
};
use dioxus::prelude::*;
use futures_util::{
    FutureExt,
    future::{Either, LocalBoxFuture, Shared, select},
};
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::{cell::RefCell, collections::{BTreeMap, BTreeSet, HashMap}, future::Future};
use t3_client::{connection::ConnectionStatus, github_routing as rules};
use t3_contracts::EnvironmentId;

type Reply = Result<Value, String>;
type Cached = Shared<LocalBoxFuture<'static, Reply>>;

const INVALIDATE: &str = "pullRequests.invalidate";
const ROUTING: &str = "pullRequests.routing";
const ROUTING_IDENTITY: &str = "pullRequests.routingIdentity";
const CHECKS: &str = "pullRequests.checks";
const SUMMARY: &str = "pullRequests.summary";
const DETAIL: &str = "pullRequests.detail";
const SET_FILES_VIEWED: &str = "pullRequests.setFilesViewed";
/// How long a read waits for the machine that answers it before the next is tried.
const READ_MS: u64 = 30_000;
/// How long the account of a machine is asked for.
const PROBE_MS: u64 = 2_000;
/// A machine that has been told of a change is not waited on longer than this.
const INVALIDATE_MS: u64 = 1_000;
const USED_LIMIT: usize = 256;

/// A pull request read or acted on through machines other than the origin, kept so that a change made through one reaches the caches
/// of the others (`routedReads`).
struct Routed {
    origin: String,
    reference: Value,
    targets: BTreeSet<String>,
}

thread_local! {
    static USED: RefCell<IndexMap<String, Routed>> = RefCell::new(IndexMap::new());
    /// Probes under way, shared by the reads that ask for the same answer at once (`shareMetadataRead`).
    static PENDING: RefCell<HashMap<String, Shared<LocalBoxFuture<'static, Option<Value>>>>> = RefCell::new(HashMap::new());
}

async fn within<T>(milliseconds: u64, future: impl Future<Output = T>) -> Option<T> {
    match select(Box::pin(future), Box::pin(runtime::delay(milliseconds))).await {
        Either::Left((value, _)) => Some(value),
        Either::Right(_) => None,
    }
}

/// `request` of the environment `id`, on its own connection.
async fn run_on(transport: &TransportHandle, state: Store<UiModel>, id: &str, method: &str, payload: Value) -> Reply {
    let link = EnvironmentId::new(id).ok().and_then(|environment| transport.link_of(&environment)).ok_or("The environment was removed.")?;
    runtime::request_value_reported(link, state, method, payload).await
}

/// The environments this window knows that may be asked, with whether each runs on this machine.
fn environments(state: Store<UiModel>) -> Vec<(EnvironmentId, bool)> {
    let model = state.peek();
    model
        .environments
        .records
        .values()
        .filter(|record| model.saved.entry(&record.id).is_none_or(|entry| entry.enabled))
        .map(|record| (record.id.clone(), rules::is_local(&record.address)))
        .collect()
}

fn permission(state: Store<UiModel>, id: &EnvironmentId) -> rules::Permission {
    crate::connection_store::github_permission(state, id)
}

/// `routingAllowed`: whether the person lets `origin` use `target`'s account (and the reverse) for a read or an act.
fn routing_allowed(state: Store<UiModel>, origin: &EnvironmentId, target: &EnvironmentId, write: bool) -> bool {
    let known = environments(state);
    known.iter().any(|(id, _)| id == origin) && known.iter().any(|(id, _)| id == target) && rules::allowed(permission(state, origin), permission(state, target), write)
}

fn typed_config_supports_checks(state: Store<UiModel>, id: &EnvironmentId) -> bool {
    let model = state.peek();
    let config = if model.destination.as_ref() == Some(id) { model.typed_config.clone() } else { model.parked.get(id).and_then(|slice| slice.typed_config.clone()) };
    config.is_some_and(|config| config.environment.capabilities.pull_request_checks == Some(true))
}

/// Runs a probe once for all who ask for the same answer while it runs. Completed probes are not kept: the next ask reads the account again.
async fn shared_read(key: String, probe: LocalBoxFuture<'static, Option<Value>>) -> Option<Value> {
    struct Done(String);
    impl Drop for Done {
        fn drop(&mut self) {
            PENDING.with(|pending| pending.borrow_mut().remove(&self.0));
        }
    }
    if let Some(running) = PENDING.with(|pending| pending.borrow().get(&key).cloned()) {
        return running.await;
    }
    let shared = probe.shared();
    PENDING.with(|pending| pending.borrow_mut().insert(key.clone(), shared.clone()));
    let _done = Done(key);
    shared.await
}

fn key_of(parts: &[Option<String>]) -> String {
    serde_json::to_string(parts).unwrap_or_default()
}

fn reference_key(origin: &str, reference: &Value) -> String {
    key_of(&[
        Some(origin.to_owned()),
        reference["projectId"].as_str().map(str::to_owned),
        reference["host"].as_str().map(str::to_lowercase),
        reference["repository"].as_str().map(str::to_lowercase),
        Some(reference["number"].to_string()),
    ])
}

/// Tells `target`'s caches that a pull request changed. GitHub has already accepted the change, so a machine that is slow to hear it
/// does not hold the confirmation up.
async fn invalidate_target(transport: &TransportHandle, state: Store<UiModel>, origin: &EnvironmentId, target: &str, inputs: Vec<Value>) {
    if let Ok(target_id) = EnvironmentId::new(target) {
        if &target_id != origin && !routing_allowed(state, origin, &target_id, false) {
            return;
        }
    }
    for input in inputs {
        let _ = within(INVALIDATE_MS, run_on(transport, state, target, INVALIDATE, input)).await;
    }
}

/// An invalidation reaches the origin and every machine that answered for the pull request it names.
async fn invalidate(transport: TransportHandle, state: Store<UiModel>, origin: EnvironmentId, payload: Value) -> Reply {
    let result = runtime::request_value_reported(transport.clone(), state, INVALIDATE, payload.clone()).await?;
    let reference = payload.get("reference").filter(|reference| rules::is_reference(reference)).cloned();
    let mut targets: BTreeMap<String, Value> = BTreeMap::new();
    USED.with(|used| {
        for entry in used.borrow().values().filter(|entry| entry.origin == origin.as_str()) {
            if reference.as_ref().is_some_and(|filter| !rules::matches_reference(&entry.reference, filter)) {
                continue;
            }
            for target in &entry.targets {
                targets.insert(target.clone(), entry.reference.clone());
            }
            if reference.is_some() {
                targets.insert(origin.to_string(), entry.reference.clone());
            }
        }
    });
    let work = targets.into_iter().map(|(target, reference_of)| {
        let mut input = payload.clone();
        if reference.is_some() {
            input["reference"] = reference_of;
        }
        let (transport, origin) = (transport.clone(), origin.clone());
        async move { invalidate_target(&transport, state, &origin, &target, vec![input]).await }
    });
    futures_util::future::join_all(work).await;
    Ok(result)
}

/// A request about a pull request, routed as far as the person has allowed (`readOrWrite`). Anything that is not about one goes
/// as it is. The answer is the one `runtime::request_value_reported` gives, whichever machine gave it.
pub(crate) async fn request(transport: TransportHandle, state: Store<UiModel>, method: &str, payload: Value) -> Reply {
    let transport = transport.pinned();
    let Some(origin) = runtime::environment_of(&transport, state) else {
        return runtime::request_value_reported(transport, state, method, payload).await;
    };
    if method == INVALIDATE {
        return invalidate(transport, state, origin, payload).await;
    }
    let (read, write) = (rules::is_read(method), rules::is_write(method));
    if !(read || write) || !rules::is_reference(&payload) {
        return runtime::request_value_reported(transport, state, method, payload).await;
    }
    if !read {
        return routed(transport, state, origin, method, payload, None).await;
    }
    let others: Vec<EnvironmentId> = environments(state).into_iter().map(|(id, _)| id).filter(|id| *id != origin).collect();
    if others.is_empty() || !others.iter().any(|id| routing_allowed(state, &origin, id, false)) {
        return runtime::request_value_reported(transport, state, method, payload).await;
    }
    // A routed read must be current: an older answer from the origin's cache could be of another account.
    let mut strict = payload.clone();
    strict["allowStale"] = json!(false);
    let source: Cached = {
        let (transport, strict) = (transport.clone(), strict.clone());
        let method = method.to_owned();
        async move { within(READ_MS, runtime::request_value_reported(transport, state, &method, strict)).await.unwrap_or_else(|| Err("The environment did not respond to the PR request.".into())) }.boxed_local().shared()
    };
    let origin_local = rules::is_local(&state.peek().environments.records.get(&origin).map(|record| record.address.clone()).unwrap_or_default());
    let routed_read = routed(transport.clone(), state, origin.clone(), method, strict, Some(source.clone()));
    // A read the origin can do on this machine costs nothing, so it is tried first; its failure goes on to the others.
    let answer = if origin_local {
        match source.await {
            Ok(value) => Ok(value),
            Err(_) => routed_read.await,
        }
    } else {
        routed_read.await
    };
    match answer {
        Err(_) if payload["allowStale"] != json!(false) && (method == SUMMARY || method == DETAIL) => runtime::request_value_reported(transport, state, method, payload).await,
        other => other,
    }
}

/// `routedRequest`: the machines that may answer, asked in the order the rules give, each after proving it has the account.
async fn routed(transport: TransportHandle, state: Store<UiModel>, origin: EnvironmentId, method: &str, payload: Value, source_read: Option<Cached>) -> Reply {
    let write = rules::is_write(method);
    let plain = |input: Value| -> Cached {
        let (transport, method) = (transport.clone(), method.to_owned());
        async move { runtime::request_value_reported(transport, state, &method, input).await }.boxed_local().shared()
    };
    let source = source_read.clone().unwrap_or_else(|| plain(payload.clone()));
    let finish = |answer: Reply| {
        let (transport, payload, origin) = (transport.clone(), payload.clone(), origin.clone());
        async move {
            if answer.is_ok() && write {
                after_write(&transport, state, &origin, method, &payload).await;
            }
            answer
        }
    };
    let mut alternatives = Vec::new();
    for (id, local) in environments(state) {
        if id == origin || !routing_allowed(state, &origin, &id, write) || fleet::status(state, &id) != ConnectionStatus::Connected {
            continue;
        }
        if method == CHECKS && !typed_config_supports_checks(state, &id) {
            continue;
        }
        alternatives.push(rules::Alternative { environment: id.to_string(), local });
    }
    if alternatives.is_empty() {
        return finish(source.await).await;
    }
    // Which host and account the project uses at the origin. Old servers and unknown accounts keep the plain path.
    let identity_probe: LocalBoxFuture<'static, Option<Value>> = {
        let (transport, payload) = (transport.clone(), payload.clone());
        async move { within(PROBE_MS, runtime::request_value_reported(transport, state, ROUTING, payload)).await.and_then(Result::ok) }.boxed_local()
    };
    let identity = if write {
        identity_probe.await
    } else {
        shared_read(key_of(&[Some(origin.to_string()), Some(ROUTING.into()), payload["projectId"].as_str().map(str::to_owned), payload["host"].as_str().map(str::to_lowercase), payload["repository"].as_str().map(str::to_lowercase)]), identity_probe).await
    };
    let Some(identity) = identity.filter(|identity| identity["provider"] == "github") else { return finish(source.await).await };
    let (host, account) = (identity["host"].as_str().unwrap_or_default().to_owned(), identity["accountId"].as_str().unwrap_or_default().to_owned());
    let mut guarded_input = payload.clone();
    guarded_input["host"] = json!(host);
    guarded_input["expectedAccountId"] = json!(account);
    let guarded_source = source_read.unwrap_or_else(|| plain(guarded_input.clone()));
    let origin_local = rules::is_local(&state.peek().environments.records.get(&origin).map(|record| record.address.clone()).unwrap_or_default());
    let candidates = rules::candidates(origin.as_str(), origin_local, &alternatives, write);
    let ref_key = reference_key(origin.as_str(), &payload);
    for (index, id) in candidates.iter().enumerate() {
        let last = index + 1 == candidates.len();
        if id.as_str() != origin.as_str() {
            let Ok(target) = EnvironmentId::new(id) else { continue };
            if !routing_allowed(state, &origin, &target, write) {
                continue;
            }
            // An older server would discard expectedAccountId. Verify it implements the guard first.
            let probe: LocalBoxFuture<'static, Option<Value>> = {
                let (transport, id, host) = (transport.clone(), id.clone(), host.clone());
                async move { within(PROBE_MS, run_on(&transport, state, &id, ROUTING_IDENTITY, json!({"host": host}))).await.and_then(Result::ok) }.boxed_local()
            };
            let alternate = if write { probe.await } else { shared_read(key_of(&[Some(id.clone()), Some(ROUTING_IDENTITY.into()), Some(host.to_lowercase())]), probe).await };
            if !alternate.as_ref().is_some_and(|alternate| rules::same_account(&identity, alternate)) || !routing_allowed(state, &origin, &target, write) {
                continue;
            }
        }
        let attempt: Reply = if id.as_str() == origin.as_str() {
            guarded_source.clone().await
        } else {
            let operation = run_on(&transport, state, id, method, guarded_input.clone());
            if write { operation.await } else { within(READ_MS, operation).await.unwrap_or_else(|| Err("The environment did not respond to the PR request.".into())) }
        };
        match attempt {
            Ok(mut result) => {
                remember(&ref_key, origin.as_str(), &payload, &host, id);
                if result.get("projectId").is_some() {
                    result["projectId"] = payload["projectId"].clone();
                    if method == DETAIL {
                        result["projectTitle"] = identity["projectTitle"].clone();
                        result["workspaceRoot"] = identity["workspaceRoot"].clone();
                    }
                }
                return finish(Ok(result)).await;
            }
            // A read goes on to the next machine whatever went wrong; an act only if the machine refused before it started.
            Err(message) if !last && (!write || rules::rejected_before_dispatch(&message)) => continue,
            Err(message) => return finish(Err(message)).await,
        }
    }
    finish(guarded_source.await).await
}

/// Notes which machine answered for a pull request, so a later change reaches it.
fn remember(key: &str, origin: &str, reference: &Value, host: &str, answered_by: &str) {
    USED.with(|used| {
        let mut used = used.borrow_mut();
        if !used.contains_key(key) {
            if used.len() >= USED_LIMIT {
                used.shift_remove_index(0);
            }
            let mut named = reference.clone();
            named["host"] = json!(host);
            used.insert(key.to_owned(), Routed { origin: origin.to_owned(), reference: named, targets: BTreeSet::new() });
        }
        if let Some(entry) = used.get_mut(key) {
            entry.targets.insert(answered_by.to_owned());
        }
    });
}

/// A change was made: every machine that has answered for the pull request, and the origin, drop what they hold of it. A file marked
/// viewed only changes the viewed marks.
async fn after_write(transport: &TransportHandle, state: Store<UiModel>, origin: &EnvironmentId, method: &str, reference: &Value) {
    let mut targets: BTreeMap<String, Vec<Value>> = BTreeMap::from([(origin.to_string(), vec![reference.clone()])]);
    USED.with(|used| {
        for entry in used.borrow().values().filter(|entry| entry.origin == origin.as_str() && rules::matches_reference(&entry.reference, reference)) {
            for target in entry.targets.iter().cloned().chain(std::iter::once(origin.to_string())) {
                let references = targets.entry(target).or_default();
                if !references.iter().any(|existing| existing["host"] == entry.reference["host"]) {
                    references.push(entry.reference.clone());
                }
            }
        }
    });
    if USED.with(|used| used.borrow().is_empty()) {
        return;
    }
    let viewed = method == SET_FILES_VIEWED;
    let work = targets.into_iter().map(|(target, references)| {
        let mut inputs: Vec<Value> = references.iter().map(|reference| if viewed { json!({"reference": reference_of(reference), "filesViewedOnly": true}) } else { json!({"reference": reference_of(reference)}) }).collect();
        if !viewed {
            inputs.push(json!({}));
        }
        let (transport, origin) = (transport.clone(), origin.clone());
        async move { invalidate_target(&transport, state, &origin, &target, inputs).await }
    });
    futures_util::future::join_all(work).await;
}

/// The part of a reference an invalidation names.
fn reference_of(reference: &Value) -> Value {
    let mut named = json!({"projectId": reference["projectId"], "repository": reference["repository"], "number": reference["number"]});
    if !reference["host"].is_null() {
        named["host"] = reference["host"].clone();
    }
    named
}

/// The machines that have answered for a pull request this window asked about from `origin`.
#[cfg(test)]
pub(crate) fn answered_by(origin: &str, reference: &Value) -> Vec<String> {
    USED.with(|used| used.borrow().get(&reference_key(origin, reference)).map(|entry| entry.targets.iter().cloned().collect()).unwrap_or_default())
}
