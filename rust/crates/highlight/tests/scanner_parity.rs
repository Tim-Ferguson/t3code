use serde_json::{Value, json};
use t3_highlight::{regex::Source, scanner::{Engine, FindOptions, Input, Scanner}};

fn input(value: &Value) -> Input {
    Input::from_units(serde_json::from_value(value.clone()).unwrap())
}
fn units(value: &[u16]) -> Value { json!(value) }
fn verify<E: Engine>(engine: &mut E) -> usize {
    let mut rows = 0;
    for (index, line) in include_str!("fixtures/scanner.jsonl").lines().enumerate() {
        let row: Value = serde_json::from_str(line).unwrap();
        let value = &row["input"];
        let result = match row["kind"].as_str().unwrap() {
            "input" => {
                let text = input(&value["units"]);
                json!({ "bytes": text.bytes(),
                    "toByte": (0..text.len()+2).map(|i| text.byte_offset(i)).collect::<Vec<_>>(),
                    "toUnit": (0..text.bytes().len()+2).map(|i| text.unit_offset(i)).collect::<Vec<_>>() })
            }
            "regex" => {
                let mut source = Source::new(&input(&value["pattern"]));
                if !value["replacement"].is_null() {
                    source.set_source(input(&value["replacement"]).units().to_vec());
                }
                let captures = value["captures"].as_array().unwrap().iter().map(|capture|
                    capture["start"].as_u64().unwrap() as usize..capture["end"].as_u64().unwrap() as usize).collect::<Vec<_>>();
                let anchors = [false, true].into_iter().flat_map(|a| [false, true].into_iter().map(move |g| (a,g)))
                    .map(|(a,g)| units(source.anchors(a,g).units())).collect::<Vec<_>>();
                json!({ "source": source.units(), "hasAnchor": source.has_anchor,
                    "hasBackreferences": source.has_backreferences,
                    "anchors": anchors,
                    "backreferences": source.backreferences(&input(&value["line"]), &captures).units() })
            }
            "match" => {
                let patterns = value["patterns"].as_array().unwrap().iter().map(input).collect::<Vec<_>>();
                let mut scanner = engine.compile(&patterns).unwrap();
                let matched = scanner.find(&input(&value["text"]), value["start"].as_u64().unwrap(),
                    FindOptions(value["options"].as_u64().unwrap() as u32)).unwrap();
                matched.map(|matched| json!({ "index": matched.pattern, "captureIndices": matched.captures.iter()
                    .map(|range| json!({ "start": range.start, "end": range.end, "length": range.end-range.start }))
                    .collect::<Vec<_>>() })).unwrap_or(Value::Null)
            }
            "invalid" => {
                let patterns = value["patterns"].as_array().unwrap().iter().map(input).collect::<Vec<_>>();
                json!(engine.compile(&patterns).is_err())
            }
            kind => panic!("unknown original oracle kind {kind}"),
        };
        assert_eq!(result, row["expected"], "original scanner row {}", index+1);
        rows += 1;
    }
    assert_eq!(rows, 2364);
    rows
}

#[test]
fn original_scanner_utf16_offsets_regex_sources_and_native_matches() {
    verify(&mut t3_highlight::native::NativeEngine);
}

#[test]
fn search_positions_cannot_escape_input_even_when_ascii_capture_sentinels_are_preserved() {
    let mut engine = t3_highlight::native::NativeEngine;
    let mut scanner = engine.compile(&[Input::from_str(".")]).unwrap();
    for text in [Input::from_str("abc"), Input::from_str("😀"), Input::from_units(vec![0xd800])] {
        for start in [text.len() as u64 + 1, u64::MAX] {
            assert_eq!(scanner.find(&text, start, FindOptions::NONE).unwrap(), None);
        }
    }
    let mut empty = engine.compile(&[Input::from_str("")]).unwrap();
    let text = Input::from_str("😀");
    assert_eq!(empty.find(&text, 1000000, FindOptions::NONE).unwrap().unwrap().captures, vec![2..2]);
    assert_eq!(empty.find(&text, u64::MAX, FindOptions::NONE).unwrap(), None);
}

#[test]
fn malformed_regexes_are_rejected_instead_of_reproducing_upstream_null_dereference() {
    let observed: Value = serde_json::from_str(include_str!("fixtures/invalid-observations.json")).unwrap();
    let mut engine = t3_highlight::native::NativeEngine;
    for witness in observed["observations"].as_array().unwrap() {
        assert_eq!(witness["constructorRejected"], false);
        let error = match engine.compile(&[input(&witness["pattern"])]) {
            Ok(_) => panic!("malformed pattern must fail safely"), Err(error) => error,
        };
        assert!(!error.0.is_empty());
        assert!(engine.compile(&[Input::from_str("valid"), input(&witness["pattern"])]).is_err());
        let mut subsequent = engine.compile(&[Input::from_str("valid")]).unwrap();
        assert!(subsequent.find(&Input::from_str("valid"), 0, FindOptions::NONE).unwrap().is_some());
    }
}
