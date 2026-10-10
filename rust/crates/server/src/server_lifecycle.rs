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
