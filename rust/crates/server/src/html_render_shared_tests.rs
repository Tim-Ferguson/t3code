//! The shared HTML render helpers against what the original's own code produces for the same inputs (`html-render-shared.jsonl`, written by
//! `tools/generate_html_render_fixtures.mjs`): the height clamp, the theme variables and fragment, and where and how the bootstrap is injected.
use crate::html_render_shared::*;
use serde_json::Value;

fn rows(kind: &str) -> Vec<Value> {
    include_str!("../tests/fixtures/html-render-shared.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == kind)
        .collect()
}

#[test]
fn heights_clamp_as_the_original_rounds_and_clamps_them() {
    let cases = rows("clamp");
    assert!(cases.len() >= 10);
    for case in cases {
        assert_eq!(clamp_html_render_height(case["input"].as_f64().unwrap()), case["output"].as_i64().unwrap(), "{}", case["input"]);
    }
}

#[test]
fn the_theme_a_page_is_handed_matches_the_originals_variables_and_fragment() {
    let cases = rows("theme");
    assert_eq!(cases.len(), 4);
    for case in cases {
        let appearance = if case["appearance"] == "dark" { Appearance::Dark } else { Appearance::Light };
        let fonts = if case["fonts"] == "default" { HTML_RENDER_DEFAULT_FONTS } else { HTML_RENDER_MEASURE_FONTS };
        let theme = html_render_theme(theme_colors(appearance), appearance, fonts);
        let variables: Vec<Value> = theme.variables.iter().map(|(name, value)| serde_json::json!([name, value])).collect();
        assert_eq!(Value::Array(variables), case["variables"], "{} {}", case["appearance"], case["fonts"]);
        assert_eq!(html_render_theme_fragment(&theme), case["fragment"].as_str().unwrap(), "{} {}", case["appearance"], case["fonts"]);
    }
}

#[test]
fn the_bootstrap_lands_where_the_original_puts_it_for_every_shape_of_page() {
    let cases = rows("inject");
    assert!(cases.len() >= 70, "{} cases", cases.len());
    let mut mismatches = Vec::new();
    for case in &cases {
        let html = case["html"].as_str().unwrap();
        let expected = case["output"].as_str().unwrap();
        let actual = inject_html_render_bootstrap(html);
        if actual != expected {
            let at = actual.bytes().zip(expected.bytes()).position(|(a, b)| a != b).unwrap_or(actual.len().min(expected.len()));
            let show = |text: &str| text.chars().skip(at.saturating_sub(30)).take(80).collect::<String>();
            mismatches.push(format!("{:?}: differs at {at}: got {:?} wanted {:?}", html.chars().take(60).collect::<String>(), show(&actual), show(expected)));
        }
    }
    assert!(mismatches.is_empty(), "{} of {} differ:\n{}", mismatches.len(), cases.len(), mismatches.join("\n"));
}

#[test]
fn the_bootstrap_goes_ahead_of_the_pages_own_head_content_and_wraps_a_fragment() {
    let injected = inject_html_render_bootstrap("<!doctype html><html><head><style>:root{--background:red}</style></head><body>x</body></html>");
    let theme_at = injected.find("<style id=\"t3-theme\">").unwrap();
    assert!(theme_at > injected.find("<head>").unwrap());
    assert!(theme_at < injected.find(":root{--background:red}").unwrap());
    assert!(injected.contains("<meta charset=\"utf-8\">") && injected.contains("name=\"viewport\""));

    let fragment = inject_html_render_bootstrap("<meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><p>hi</p>");
    assert!(fragment.starts_with("<!doctype html><head>") && fragment.ends_with("<p>hi</p>"));
    assert_eq!(fragment.matches("charset").count(), 1);
    assert_eq!(fragment.matches("name=\"viewport\"").count(), 1);
}
