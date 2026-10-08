#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use t3_client::timeline_scroll::*;
    fn js_equal(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| js_equal(a, b))
            }
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(key, value)| b.get(key).is_some_and(|other| js_equal(value, other)))
            }
            _ => a == b,
        }
    }
    fn number(value: &Value) -> Option<f64> {
        match value.as_str() {
            Some("NaN") => Some(f64::NAN),
            Some("Infinity") => Some(f64::INFINITY),
            Some("-Infinity") => Some(f64::NEG_INFINITY),
            _ => value.as_f64(),
        }
    }
    fn measurement(value: &Value) -> Measurements {
        let positions: Vec<_> = value["positions"]
            .as_array()
            .unwrap()
            .iter()
            .map(number)
            .collect();
        Measurements {
            row_ids: (0..positions.len())
                .map(|index| format!("row-{index}"))
                .collect(),
            positions,
            sizes: value["sizes"]
                .as_array()
                .unwrap()
                .iter()
                .map(number)
                .collect(),
            scroll: number(&value["scroll"]).unwrap(),
            viewport_height: number(&value["scrollLength"]).unwrap(),
        }
    }
    #[test]
    fn unchanged_original_geometry_run_observation_end_band_and_fling_anchors() {
        for (index, line) in include_str!("fixtures/timeline-scroll.jsonl")
            .lines()
            .enumerate()
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let actual = match row["kind"].as_str().unwrap() {
                "release" => json!(release_anchor_for_tool_activity(&row["input"])),
                "observe" => serde_json::to_value(observe_run(
                    serde_json::from_value::<Option<RunObservation>>(row["previous"].clone())
                        .unwrap()
                        .as_ref(),
                    &serde_json::from_value(row["input"].clone()).unwrap(),
                ))
                .unwrap(),
                "bottom" => {
                    let state = measurement(&row["state"]);
                    let index = number(&row["index"]).unwrap();
                    serde_json::to_value(
                        if index >= 0.0 && index.is_finite() && index.fract() == 0.0 {
                            state.row_bottom(index as usize)
                        } else {
                            None
                        },
                    )
                    .unwrap()
                }
                "overflow" => json!(measurement(&row["state"]).content_overflows(
                    number(&row["input"]["composerInset"]).unwrap(),
                    number(&row["input"]["anchorOffset"]).unwrap()
                )),
                "metrics" => serde_json::to_value(measurement(&row["state"]).anchored_turn(
                    number(&row["input"]["anchorIndex"]).unwrap(),
                    number(&row["input"]["composerOverlayHeight"]).unwrap(),
                    number(&row["input"]["anchorOffset"]).unwrap(),
                ))
                .unwrap(),
                "anchor" => {
                    serde_json::to_value(measurement(&row["state"]).reading_anchor()).unwrap()
                }
                "end" => {
                    let input = &row["input"];
                    let state = (!input.is_null()).then(|| EndState {
                        is_at_end: input["isAtEnd"].as_bool(),
                        content_length: number(&input["contentLength"]),
                        scroll: number(&input["scroll"]),
                        scroll_length: number(&input["scrollLength"]),
                    });
                    json!(resolve_at_end(state.as_ref()))
                }
                kind => panic!("unknown fixture {kind}"),
            };
            assert!(
                js_equal(&actual, &row["expected"]),
                "source scrolling case {} ({}): actual={actual}, expected={}",
                index + 1,
                row["kind"],
                row["expected"]
            );
        }
    }
    #[test]
    fn reading_anchor_survives_history_prepend_and_expanded_tool_height() {
        let before = Measurements {
            row_ids: vec!["first".into(), "output".into(), "last".into()],
            positions: vec![Some(0.0), Some(33.0), Some(600.0)],
            sizes: vec![Some(33.0), Some(567.0), Some(90.0)],
            scroll: 153.0,
            viewport_height: 700.0,
        };
        let anchor = before.reading_anchor().unwrap();
        assert_eq!(anchor.row_id, "output");
        assert_eq!(anchor.offset_within_row, 120.0);
        let after = Measurements {
            row_ids: vec![
                "history".into(),
                "first".into(),
                "output".into(),
                "last".into(),
            ],
            positions: vec![Some(0.0), Some(900.0), Some(933.0), Some(1700.0)],
            sizes: vec![Some(900.0), Some(33.0), Some(767.0), Some(90.0)],
            scroll: 153.0,
            viewport_height: 700.0,
        };
        assert_eq!(after.restore_offset(&anchor, 2000.0), 1053.0);
        assert_eq!(after.restore_offset(&anchor, 1000.0), 1000.0);
    }
    #[test]
    fn cache_is_thread_scoped_bounded_and_recency_changes_only_on_write() {
        let position = RememberedPosition {
            anchor: RowAnchor {
                row_id: "same-row".into(),
                offset_within_row: 32.0,
                scroll_offset: 932.0,
            },
            at_end: false,
            disclosures: Some(Disclosures::default()),
        };
        let mut cache = PositionCache::default();
        for index in 0..100 {
            cache.remember(format!("a:{index}"), position.clone());
        }
        cache.read("a:0");
        cache.remember(
            "b:0".into(),
            RememberedPosition {
                at_end: true,
                ..position.clone()
            },
        );
        assert_eq!(cache.len(), 100);
        assert!(cache.read("a:0").is_none());
        assert!(!cache.read("a:1").unwrap().at_end);
        assert!(cache.read("b:0").unwrap().at_end);
        cache.remember("a:1".into(), position.clone());
        cache.remember("b:1".into(), position);
        assert!(cache.read("a:2").is_none());
        assert!(cache.read("a:1").is_some());
    }
}
