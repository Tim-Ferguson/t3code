use serde_json::Value;
use t3_client::themes::*;
#[test]
fn original_theme_catalog_storage_and_resolution_witnesses() {
    let cases: Vec<Value> = include_str!("fixtures/themes.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let setup = cases.iter().find(|case| case["kind"] == "catalog").unwrap();
    let catalog = Catalog {
        custom: serde_json::from_value(setup["custom"].clone()).unwrap(),
        environment: serde_json::from_value(setup["environment"].clone()).unwrap(),
        ..Default::default()
    };
    for case in cases {
        let theme = case["theme"].as_str().unwrap_or("");
        match case["kind"].as_str().unwrap() {
            "chrome" => {
                assert_eq!(
                    serde_json::json!(case["value"].as_str().and_then(normalize_browser_color)),
                    case["expected"]
                );
            }
            "preference" => {
                assert_eq!(canonical_preference(theme), case["canonical"]);
                assert_eq!(
                    serde_json::json!(catalog.preference_mode(theme)),
                    case["mode"]
                );
                assert_eq!(serde_json::json!(catalog.known(theme)), case["known"]);
                assert_eq!(
                    serde_json::json!(catalog.stored_preference(Some(theme))),
                    case["stored"]
                );
            }
            "resolve" => {
                let halves: Option<Halves> =
                    serde_json::from_value(case["halves"].clone()).unwrap();
                let mode: Option<Mode> = serde_json::from_value(case["mode"].clone()).unwrap();
                let follow = case["follow"].as_bool();
                assert_eq!(
                    serde_json::json!(catalog.resolve(
                        theme,
                        case["systemDark"].as_bool().unwrap(),
                        follow,
                        mode,
                        halves.as_ref()
                    )),
                    case["appearance"],
                    "{case}"
                );
                assert_eq!(
                    serde_json::json!(catalog.desktop_mode(theme, follow, mode, halves.as_ref())),
                    case["desktop"],
                    "{case}"
                );
            }
            "storedMode" => assert_eq!(
                serde_json::json!(catalog.stored_mode(
                    theme,
                    case["mode"].as_str(),
                    case["follow"].as_str()
                )),
                case["expected"],
                "{case}"
            ),
            "halves" => assert_eq!(
                serde_json::json!(catalog.parse_halves(case["raw"].as_str())),
                case["expected"],
                "{case}"
            ),
            "catalog" => {}
            kind => panic!("unknown {kind}"),
        }
    }
}
#[test]
fn builtin_palettes_cover_every_role_and_raw_halves_retain_unpublished_ids() {
    let catalog = Catalog::default();
    assert_eq!(catalog.data.builtin.len(), 5);
    for theme in &catalog.data.builtin {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let colors = theme.colors(appearance).unwrap();
            for role in &catalog.data.roles {
                assert!(colors.contains_key(role), "{} {role}", theme.id);
                assert!(catalog.data.variables.contains_key(role));
            }
        }
    }
    assert_eq!(
        raw_halves(Some(
            r#"{"light":"future-published","dark":"ocean","other":"keep-original"}"#
        ))
        .light
        .as_deref(),
        Some("future-published")
    );
}

#[test]
fn original_theme_transactions_preserve_raw_mix_and_settle_failures() {
    use std::{
        collections::BTreeMap,
        future::Future,
        task::{Context, Poll, Waker},
    };
    use t3_client::themes::storage::{self, Action, ReadState, Storage};
    struct Memory {
        saved: BTreeMap<String, String>,
        ops: Vec<Value>,
        writes: u64,
        fail: u64,
    }
    impl Storage for Memory {
        async fn get(&mut self, key: &str) -> Result<Option<String>, String> {
            Ok(self.saved.get(key).cloned())
        }
        async fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
            self.ops
                .push(serde_json::json!({"type":"set","key":key,"value":value}));
            self.writes += 1;
            if self.writes == self.fail {
                return Err("injected".into());
            }
            self.saved.insert(key.into(), value.into());
            Ok(())
        }
        async fn remove(&mut self, key: &str) -> Result<(), String> {
            self.ops
                .push(serde_json::json!({"type":"remove","key":key}));
            self.writes += 1;
            if self.writes == self.fail {
                return Err("injected".into());
            }
            self.saved.remove(key);
            Ok(())
        }
    }
    let catalog = Catalog::default();
    for (line, raw) in include_str!("fixtures/theme-transactions.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(raw).unwrap();
        let mut memory = Memory {
            saved: serde_json::from_value(row["initial"].clone()).unwrap(),
            ops: vec![],
            writes: 0,
            fail: row["failAt"].as_u64().unwrap(),
        };
        let action = &row["action"];
        let selected = match action["type"].as_str().unwrap() {
            "theme" => Action::Theme(action["value"].as_str().unwrap().into()),
            "mode" => Action::Mode(serde_json::from_value(action["value"].clone()).unwrap()),
            "half" => Action::Half(
                serde_json::from_value(action["appearance"].clone()).unwrap(),
                action["value"].as_str().map(Into::into),
            ),
            "clear" => Action::ClearHalves,
            _ => panic!("action"),
        };
        let mut reads = ReadState::default();
        let result = {
            let mut future =
                std::pin::pin!(storage::apply(&mut memory, &catalog, &mut reads, selected));
            match future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                Poll::Ready(result) => result,
                Poll::Pending => panic!("memory store should be synchronous"),
            }
        };
        assert_eq!(
            result.is_ok(),
            row["success"].as_bool().unwrap(),
            "transaction {line}"
        );
        assert_eq!(
            serde_json::to_value(memory.saved).unwrap(),
            row["saved"],
            "saved {line}"
        );
        // Object key order is irrelevant, but the original write order is not.
        let normalize = |ops: Vec<Value>| {
            ops.into_iter()
                .map(|op| (op["type"].clone(), op["key"].clone(), op["value"].clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            normalize(memory.ops),
            normalize(serde_json::from_value(row["ops"].clone()).unwrap()),
            "operations {line}"
        );
    }
}
