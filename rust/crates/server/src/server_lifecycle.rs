//! The lifecycle events a client follows to know when the server is up (`serverLifecycleEvents.ts`): a
//! `welcome` once startup has finished its recovery, then `ready` once commands are accepted. Each event
//! carries the sequence it was published at, and a subscriber is first given the latest event of each kind,
//! so a client that connects after startup still sees both. The events belong to the server over a store, as
//! the services over it do, and reach the stream through it.
use crate::persistence::Store;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Mutex};

struct State {
    sequence: u64,
    /// The latest event of each type, newest first.
    events: Vec<Value>,
    live: tokio::sync::broadcast::Sender<Value>,
}

static LIFECYCLES: Mutex<Option<HashMap<usize, State>>> = Mutex::new(None);

fn with<R>(store: &Store, f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = LIFECYCLES.lock().unwrap();
    let state = guard.get_or_insert_with(HashMap::new).entry(store.identity()).or_insert_with(|| State { sequence: 0, events: vec![], live: tokio::sync::broadcast::channel(64).0 });
    f(state)
}

/// `ServerLifecycleEvents.publish`: numbers the event and offers it to every subscriber.
pub fn publish(store: &Store, kind: &str, payload: Value) -> Value {
    with(store, |state| {
        state.sequence += 1;
        let event = json!({"version":1,"sequence":state.sequence,"type":kind,"payload":payload});
        state.events.retain(|entry| entry["type"] != kind);
        state.events.insert(0, event.clone());
        let _ = state.live.send(event.clone());
        event
    })
}

/// The `welcome` payload (`resolveWelcomeBase` and the targets of `resolveAutoBootstrapWelcomeTargets`): where the server runs, and the project
/// and thread it opened when asked to. The contract also declares `bootstrapStatus`, `bootstrapProjectCreated` and `bootstrapThreadCreated`,
/// but the original's server never sends them (the first-run gate treats their absence as a complete bootstrap), so neither does this one,
/// with auto-bootstrap on or off.
pub fn welcome_payload(environment: &Value, cwd: &std::path::Path, bootstrap: Option<(&str, &str)>) -> Value {
    let project_name = cwd.file_name().map_or("project".to_owned(), |name| name.to_string_lossy().into_owned());
    let mut welcome = json!({"environment":environment,"cwd":cwd,"projectName":project_name});
    if let Some((project_id, thread_id)) = bootstrap {
        welcome["bootstrapProjectId"] = json!(project_id);
        welcome["bootstrapThreadId"] = json!(thread_id);
    }
    welcome
}

/// The events to send first, in sequence order, and the stream of those published after.
pub(crate) fn subscribe(store: &Store) -> (Vec<Value>, u64, tokio::sync::broadcast::Receiver<Value>) {
    with(store, |state| {
        let mut events = state.events.clone();
        events.sort_by_key(|event| event["sequence"].as_u64());
        (events, state.sequence, state.live.subscribe())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_welcome_names_the_workspace_and_only_what_the_original_sends_of_the_bootstrap() {
        let environment = json!({"environmentId":"e"});
        let plain = welcome_payload(&environment, std::path::Path::new("/work/app"), None);
        assert_eq!(plain, json!({"environment":environment,"cwd":"/work/app","projectName":"app"}));
        let bootstrapped = welcome_payload(&environment, std::path::Path::new("/work/app"), Some(("project-1", "thread-1")));
        assert_eq!(bootstrapped, json!({"environment":environment,"cwd":"/work/app","projectName":"app","bootstrapProjectId":"project-1","bootstrapThreadId":"thread-1"}));
        for key in ["bootstrapStatus", "bootstrapProjectCreated", "bootstrapThreadCreated"] {
            assert!(plain.get(key).is_none() && bootstrapped.get(key).is_none(), "{key} is not sent by the original");
        }
        assert_eq!(welcome_payload(&environment, std::path::Path::new("/"), None)["projectName"], "project");
    }

    #[test]
    fn a_subscriber_gets_the_latest_event_of_each_kind_in_order_and_then_what_follows() {
        let store = Store::memory().unwrap();
        publish(&store, "welcome", json!({"cwd":"/a"}));
        publish(&store, "ready", json!({"at":"t1"}));
        publish(&store, "ready", json!({"at":"t2"}));
        let (initial, sequence, mut live) = subscribe(&store);
        assert_eq!(initial.iter().map(|event| (event["type"].as_str().unwrap(), event["sequence"].as_u64().unwrap())).collect::<Vec<_>>(), [("welcome", 1), ("ready", 3)]);
        assert_eq!(sequence, 3);
        publish(&store, "legacyThreadMigration", json!({"status":"complete","totalThreadCount":0}));
        assert_eq!(live.try_recv().unwrap()["sequence"], 4);
    }
}
