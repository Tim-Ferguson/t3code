use serde_json::Value;
use t3_terminal::surface_policy;
#[test]
fn original_mouse_dedup_tracking_reset_and_fractional_wheel_policy() {
    for (index, line) in include_str!("fixtures/surface.jsonl").lines().enumerate() {
        let row: Value = serde_json::from_str(line).unwrap();
        let text = |name: &str| row[name].as_str().unwrap();
        let number = |name: &str| row[name].as_f64().unwrap();
        let boolean = |name: &str| row[name].as_bool().unwrap();
        let actual = match text("kind") {
            "click" => serde_json::to_value(surface_policy::advance_click(
                serde_json::from_value(row["previous"].clone()).unwrap(),
                number("time"),
                number("x"),
                number("y"),
            ))
            .unwrap(),
            "mouse" => serde_json::to_value(surface_policy::mouse_data(
                text("action"),
                text("data"),
                text("previous"),
            ))
            .unwrap(),
            "tracking" => serde_json::to_value(surface_policy::mouse_tracking(
                boolean("previous"),
                boolean("tracking"),
                text("data"),
            ))
            .unwrap(),
            "wheel" => serde_json::to_value(surface_policy::wheel(
                number("delta"),
                number("mode") as u32,
                number("height"),
                number("viewportRows") as u16,
                number("remainder"),
            ))
            .unwrap(),
            "arrows" => Value::String(surface_policy::wheel_arrows(
                number("rows") as i32,
                boolean("application"),
            )),
            _ => panic!("unknown fixture"),
        };
        if row["kind"] == "wheel" {
            assert_eq!(actual["rows"].as_i64(), row["expected"]["rows"].as_i64());
            assert_eq!(
                actual["remainder"].as_f64(),
                row["expected"]["remainder"].as_f64(),
                "source surface witness {index}"
            );
        } else if row["kind"] == "click" {
            assert_eq!(actual["count"], row["expected"]["count"]);
            for name in ["time", "x", "y"] {
                assert_eq!(actual[name].as_f64(), row["expected"][name].as_f64());
            }
        } else {
            assert_eq!(actual, row["expected"], "source surface witness {index}");
        }
    }
}
