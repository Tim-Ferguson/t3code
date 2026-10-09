//! The gate's limits and its cost. The interpreter walks attacker-controlled JSON on the connection's own task, so a
//! payload nested deeper than a stack can follow, or with more nodes than any real one has, is refused; and what the gate
//! costs on the hottest and the largest payloads is measured (`T3_PRINT_RPC_COST=1` prints it).
use crate::rpc_decode_tests::cases;
use serde_json::{Value, json};
use std::time::Instant;

/// The largest payload of `method` (under 200 kB) the original accepts.
fn accepted(method: &str) -> Value {
    cases().into_iter().filter(|case| case.method == method && case.accepted).map(|case| case.input).filter(|input| input.to_string().len() < 200_000).max_by_key(|input| input.to_string().len()).unwrap_or_else(|| panic!("no accepted case for {method}"))
}
fn find_mut<'a>(value: &'a mut Value, key: &str) -> Option<&'a mut Value> {
    match value {
        Value::Object(object) => {
            if object.contains_key(key) {
                return object.get_mut(key);
            }
            object.values_mut().find_map(|found| find_mut(found, key))
        }
        Value::Array(items) => items.iter_mut().find_map(|found| find_mut(found, key)),
        _ => None,
    }
}

/// An accessibility tree nested `depth` deep inside an otherwise accepted attachment payload.
fn nested(depth: usize) -> Value {
    let node = |children: Value| json!({"role":"button","bounds":null,"children":children});
    let mut root = node(json!([]));
    for _ in 0..depth {
        root = node(json!([root]));
    }
    json!({"threadId":"x","messageId":"x","attachments":[{"type":"image","name":"x","mimeType":"image/png","sizeBytes":1,"dataUrl":"data:image/png;base64,AAAA","source":{"kind":"snap-shot","capturedAt":"x","appName":"x","windowTitle":"x","accessibility":{"format":"element-tree","coordinateSpace":"captured-image","imageSize":{"width":1,"height":1},"truncated":false,"root":root}}}]})
}

/// A payload the transport can parse is at most 128 JSON levels deep: the deepest tree that fits is followed on a small stack
/// (and refused or admitted, never overflowed); one built past that limit, which only a test can make, is refused at the
/// interpreter's own depth limit. Dropping a value this deep is itself recursive, so it is leaked.
#[test]
fn the_deepest_payload_the_transport_parses_is_followed_and_a_deeper_one_is_refused() {
    let run = |stack: usize, depth: usize| {
        std::thread::Builder::new()
            .stack_size(stack)
            .spawn(move || {
                let payload = nested(depth);
                let outcome = crate::rpc_admission::admit("assets.persistChatAttachments", &payload).map(|_| ()).map_err(|error| error["message"].as_str().unwrap_or_default().to_owned());
                std::mem::forget(payload);
                outcome
            })
            .unwrap()
            .join()
            .expect("the interpreter did not overflow its stack")
    };
    // About 120 JSON levels: two for each level of the tree, below the attachment's own.
    run(1 << 20, 55).expect("the deepest tree the transport parses is a payload");
    let message = run(16 << 20, 800).expect_err("a tree this deep is not a payload");
    assert!(message.contains("Maximum call stack size exceeded"), "{message}");
    assert!(crate::rpc_admission::admit("assets.persistChatAttachments", &nested(20)).is_ok());
}

#[test]
fn a_payload_with_more_nodes_than_any_real_one_is_refused_quickly() {
    let mut payload = accepted("provider.chatgpt.import-profile");
    let scopes = find_mut(&mut payload, "scopes").expect("a list of scopes");
    *scopes = Value::Array(vec![json!("openid"); 1_500_000]);
    let started = Instant::now();
    let refusal = crate::rpc_admission::admit("provider.chatgpt.import-profile", &payload).expect_err("too many nodes");
    assert!(refusal["message"].as_str().unwrap().starts_with("The payload is too large to decode"), "{refusal}");
    assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
    // Fewer is fine.
    *find_mut(&mut payload, "scopes").unwrap() = Value::Array(vec![json!("openid"); 50_000]);
    assert!(crate::rpc_admission::admit("provider.chatgpt.import-profile", &payload).is_ok());
}

/// Cost per call of the gate on the most frequent payloads (terminal input, status subscriptions, acknowledgements' siblings) and the
/// largest (a command union, a settings patch, an image attachment of several megabytes).
#[test]
fn the_gate_is_cheap_on_hot_and_large_payloads() {
    let mut big = accepted("assets.persistChatAttachments");
    if let Some(data) = find_mut(&mut big, "dataUrl") {
        *data = json!(format!("data:image/png;base64,{}", "A".repeat(5_000_000)));
    }
    let hot: Vec<(&str, Value, usize)> = vec![
        ("terminal.write", json!({"threadId":"thread-1","terminalId":"default","data":"ls -la\n"}), 20_000),
        ("terminal.resize", json!({"threadId":"thread-1","terminalId":"default","cols":120,"rows":40}), 20_000),
        ("subscribeVcsStatus", json!({"cwd":"/work/project"}), 20_000),
        ("orchestration.dispatchCommand", accepted("orchestration.dispatchCommand"), 5_000),
        ("server.updateSettings", accepted("server.updateSettings"), 2_000),
        ("assets.persistChatAttachments (5 MB image)", big.clone(), 20),
    ];
    for (name, payload, rounds) in hot {
        let method = name.split(' ').next().unwrap();
        let started = Instant::now();
        for _ in 0..rounds {
            let _ = std::hint::black_box(crate::rpc_admission::admit(method, std::hint::black_box(&payload)));
        }
        let per_call = started.elapsed() / rounds as u32;
        // What the handler's own serde decode costs on the same payload, for scale.
        let decoder = crate::rpc_handler_decoders::handler_decoder(method);
        let started = Instant::now();
        for _ in 0..rounds {
            let _ = std::hint::black_box(decoder.map(|decode| decode(std::hint::black_box(&payload))));
        }
        let handler = started.elapsed() / rounds as u32;
        if std::env::var("T3_PRINT_RPC_COST").is_ok() {
            eprintln!("COST {name}: gate {per_call:?} per call, handler decode {handler:?} ({} bytes)", payload.to_string().len());
        }
        // A debug build of an interpreter: far from a budget, but it catches a per-call rebuild of a table.
        assert!(per_call.as_millis() < if name.contains("5 MB") { 500 } else { 20 }, "{name}: {per_call:?}");
    }
}
