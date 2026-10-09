//! The port's decoding of every WebSocket RPC payload, against the original's own decoding (through the JSON
//! codec the server reads payloads with) of cases `tools/generate_rpc_decode_cases.mjs` derives from each payload
//! schema in packages/contracts/src/rpc.ts. What matters most is a payload the original refuses and the port carries
//! out; the other direction, and the text of the refusal, are counted too.
use serde_json::Value;
use std::collections::BTreeMap;

/// The generator writes a long run of one string as `{"$repeat": [unit, count]}` (with `"$pad": [before, after]` for a
/// blank on each side), and a long array of one element as `{"$items": [element, count]}`.
pub(crate) fn expand(value: &Value) -> Value {
    match value {
        Value::Object(object) if object.contains_key("$repeat") => {
            let body = object["$repeat"][0].as_str().unwrap().repeat(object["$repeat"][1].as_u64().unwrap() as usize);
            let pad = |at: usize| object.get("$pad").and_then(|pad| pad[at].as_str()).unwrap_or_default();
            Value::String(format!("{}{body}{}", pad(0), pad(1)))
        }
        Value::Object(object) if object.len() == 1 && object.contains_key("$items") => Value::Array(vec![expand(&object["$items"][0]); object["$items"][1].as_u64().unwrap() as usize]),
        Value::Object(object) => Value::Object(object.iter().map(|(key, found)| (key.clone(), expand(found))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(expand).collect()),
        other => other.clone(),
    }
}

/// One generated case: the payload, whether the original decodes it, and the text it fails with.
pub(crate) struct Case {
    pub method: String,
    pub input: Value,
    pub accepted: bool,
    pub message: Option<String>,
}
pub(crate) fn cases() -> Vec<Case> {
    // Kept gzipped in the repository: as text the fixture is 16 MB.
    let mut text = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&include_bytes!("../tests/fixtures/rpc-decode-cases.jsonl.gz")[..]), &mut text).expect("the committed cases unpack");
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let row: Value = serde_json::from_str(line).unwrap();
            Case { method: row["method"].as_str().unwrap().to_owned(), input: expand(&row["input"]), accepted: row["accepted"] == true, message: row["message"].as_str().map(str::to_owned) }
        })
        .collect()
}

/// Per method, with the gate: cases, payloads carried out that the original refuses, payloads refused that the original
/// accepts, and refusals whose text differs.
#[test]
fn the_gate_refuses_what_the_original_refuses_with_its_text() {
    let mut counts: BTreeMap<String, [usize; 4]> = BTreeMap::new();
    let mut examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for case in cases() {
        let refusal = crate::rpc_admission::refuse(&case.method, &case.input);
        let entry = counts.entry(case.method.clone()).or_default();
        entry[0] += 1;
        let shown = || case.input.to_string().chars().take(300).collect::<String>();
        match (&refusal, case.accepted) {
            (None, false) => {
                entry[1] += 1;
                examples.entry(case.method.clone()).or_default().push(format!("carried out, original refuses ({}): {}", case.message.as_deref().unwrap_or_default().replace('\n', " / "), shown()));
            }
            (Some(error), true) => {
                entry[2] += 1;
                examples.entry(case.method.clone()).or_default().push(format!("refused ({}), original accepts: {}", error["message"].as_str().unwrap_or_default().replace('\n', " / "), shown()));
            }
            (Some(error), false) if error["message"].as_str() != case.message.as_deref() => {
                entry[3] += 1;
                examples.entry(case.method.clone()).or_default().push(format!("text differs: ours {:?}, original {:?}: {}", error["message"].as_str().unwrap_or_default(), case.message.as_deref().unwrap_or_default(), shown()));
            }
            _ => {}
        }
    }
    if std::env::var("T3_PRINT_RPC_MISMATCHES").is_ok() {
        for (method, [cases, accepts_refused, refuses_accepted, text]) in &counts {
            eprintln!("{method}: {cases} cases, {accepts_refused} carried out that the original refuses, {refuses_accepted} refused that it accepts, {text} refusals with other text");
            for example in examples.get(method).into_iter().flatten().take(6) {
                eprintln!("    {example}");
            }
        }
    }
    let wrong: Vec<_> = counts.iter().filter(|(_, [_, a, b, c])| a + b + c > 0).map(|(method, counts)| format!("{method}: {counts:?}")).collect();
    assert!(wrong.is_empty(), "{} methods differ: {wrong:#?}", wrong.len());
    // The methods the transport checks are the ones these cases cover.
    let mut covered: Vec<&str> = crate::mcp_tool_input::decoder::rpc_methods().collect();
    covered.sort_unstable();
    assert_eq!(covered, counts.keys().map(String::as_str).collect::<Vec<_>>());
}

/// Why the value a handler decodes from the raw payload differs from the one it decodes from the original's decoded payload.
fn difference(before: &Value, after: &Value) -> &'static str {
    match (before, after) {
        (Value::Object(left), Value::Object(right)) => {
            for (key, found) in left {
                match right.get(key) {
                    None if found.is_null() => return "null read as absent",
                    None => return "unknown key dropped",
                    Some(other) if other != found => return difference(found, other),
                    Some(_) => {}
                }
            }
            if right.keys().any(|key| !left.contains_key(key)) { "default filled in" } else { "other" }
        }
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                return "elements dropped";
            }
            left.iter().zip(right).find(|(a, b)| a != b).map_or("other", |(a, b)| difference(a, b))
        }
        (Value::String(left), Value::String(right)) if left.trim() == right => "string trimmed",
        (Value::String(left), Value::String(right)) if left.to_lowercase() == *right => "key lower-cased",
        _ => "other",
    }
}

/// Per method, the handler's own decode against the original, before the gate and with it. With the gate a payload is
/// carried out when the gate admits it and the handler decodes what the gate hands it. Also the accepted payloads on
/// which the handler acts on a different value when given the original's decoded one.
#[test]
fn the_handlers_decode_what_the_original_decodes() {
    use crate::rpc_handler_decoders::handler_decoder;
    // before gate: [cases, carried out but refused by the original, refused but accepted by the original]; with gate: the same two.
    let mut counts: BTreeMap<String, [usize; 8]> = BTreeMap::new();
    let mut examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unhandled: std::collections::BTreeSet<String> = Default::default();
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for case in cases() {
        let Some(handler) = handler_decoder(&case.method) else {
            unhandled.insert(case.method.clone());
            continue;
        };
        let entry = counts.entry(case.method.clone()).or_default();
        entry[0] += 1;
        let raw = handler(&case.input);
        let shown = || case.input.to_string().chars().take(300).collect::<String>();
        if raw.is_ok() && !case.accepted {
            entry[1] += 1;
            examples.entry(case.method.clone()).or_default().push(format!("handler carries out, original refuses: {}", shown()));
        }
        // JSON cannot carry NaN or an infinity to a Rust number: a field the original reads from "NaN" or "Infinity" is refused here.
        let non_finite = |result: &Result<Value, String>| result.as_ref().err().is_some_and(|error| ["\"NaN\"", "\"Infinity\"", "\"-Infinity\""].iter().any(|text| error.contains(&format!("string {text}"))));
        if raw.is_err() && case.accepted && non_finite(&raw) {
            entry[7] += 1;
        } else if raw.is_err() && case.accepted {
            entry[2] += 1;
            examples.entry(case.method.clone()).or_default().push(format!("handler refuses ({}), original accepts: {}", raw.as_ref().err().unwrap().chars().take(120).collect::<String>(), shown()));
        }
        let admitted = crate::rpc_admission::admit(&case.method, &case.input);
        let decoded_input = match &admitted {
            Ok(Some(decoded)) => decoded.clone(),
            _ => case.input.clone(),
        };
        let with_gate = admitted.is_ok().then(|| handler(&decoded_input));
        let carried = with_gate.as_ref().is_some_and(|result| result.is_ok());
        if carried && !case.accepted {
            entry[3] += 1;
            examples.entry(case.method.clone()).or_default().push(format!("gate and handler carry out, original refuses: {}", shown()));
        }
        if !carried && case.accepted && with_gate.as_ref().is_some_and(non_finite) {
            // counted above, in the non-finite column
        } else if !carried && case.accepted {
            entry[4] += 1;
            if let Some(kind) = case.input["type"].as_str() {
                *by_kind.entry(format!("{} {kind}", case.method)).or_default() += 1;
            }
            examples.entry(case.method.clone()).or_default().push(format!("gate or handler refuse ({:?}), original accepts: {} (handler given {})", with_gate.as_ref().and_then(|result| result.as_ref().err()), shown(), decoded_input.to_string().chars().take(300).collect::<String>()));
        }
        if case.accepted {
            if let (Ok(before), Some(Ok(after))) = (&raw, &with_gate) {
                if before != after {
                    entry[5] += 1;
                    *kinds.entry(format!("{} / {}", case.method, difference(before, after))).or_default() += 1;
                    examples.entry(case.method.clone()).or_default().push(format!("handler acts on a different value when given the decoded one: {} => {}", shown(), decoded_input.to_string().chars().take(300).collect::<String>()));
                }
            }
        }
        entry[6] += case.accepted as usize;
    }
    if std::env::var("T3_PRINT_RPC_MISMATCHES").is_ok() {
        for (method, [cases, a, b, c, d, e, accepted, nan]) in &counts {
            eprintln!("HANDLER {method}: {cases} cases ({accepted} accepted); before the gate {a} carried out that the original refuses, {b} refused that it accepts; with the gate {c}, {d}; {e} act on a different decoded value; {nan} known divergences (a non-finite number)");
            let shown = std::env::var("T3_RPC_EXAMPLES").ok().and_then(|text| text.parse().ok()).unwrap_or(4);
            for example in examples.get(method).into_iter().flatten().take(shown) {
                eprintln!("    {example}");
            }
        }
        eprintln!("HANDLER unhandled by the Rust server: {unhandled:?}");
        for (kind, count) in &kinds {
            eprintln!("HANDLER acts on a different value: {kind}: {count}");
        }
        for (kind, count) in &by_kind {
            eprintln!("HANDLER refused with the gate, by command type: {kind}: {count}");
        }
    }
    let wrong: Vec<_> = counts.iter().filter(|(_, [_, _, _, c, d, _, _, _])| c + d > 0).map(|(method, counts)| format!("{method}: {counts:?}")).collect();
    assert!(wrong.is_empty(), "{} methods differ with the gate on: {wrong:#?}", wrong.len());
}
