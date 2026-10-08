//! Development-only WASM bindings for the Rust theme policy; never linked into the UI.
use serde_json::{Value, json};
use t3_client::themes::{self, Catalog};
use wasm_bindgen::prelude::*;
#[wasm_bindgen]
pub fn color(input: &str) -> String {
    let input: Value = serde_json::from_str(input).unwrap();
    let text = input.as_str();
    json!({"canonical":text.and_then(themes::color::canonical),"hex":text.and_then(themes::color::hex)}).to_string()
}
#[wasm_bindgen]
pub fn vivid(input: &str) -> String {
    let row: Value = serde_json::from_str(input).unwrap();
    serde_json::to_string(&themes::vivid::create(
        &Catalog::default(),
        serde_json::from_value(row["appearance"].clone()).unwrap(),
        row["background"].as_str().unwrap(),
        row["accent"].as_str().unwrap(),
    ))
    .unwrap()
}
#[wasm_bindgen]
pub fn family(input: &str) -> String {
    let row: Value = serde_json::from_str(input).unwrap();
    serde_json::to_string(&themes::vivid::update(
        serde_json::from_value(row["appearance"].clone()).unwrap(),
        &serde_json::from_value(row["colors"].clone()).unwrap(),
        row["role"].as_str().unwrap(),
        row["color"].as_str().unwrap(),
    ))
    .unwrap()
}
#[wasm_bindgen]
pub fn editor(input: &str) -> String {
    let row: Value = serde_json::from_str(input).unwrap();
    let mut catalog = Catalog::default();
    catalog.custom = themes::library::stored_themes(&catalog, row["initial"].as_array().unwrap());
    let value = &row["draft"];
    let draft = themes::editor::Draft {
        editing_id: value["editingId"].as_str().map(str::to_owned),
        name: value["name"].as_str().unwrap().into(),
        appearance: serde_json::from_value(value["appearance"].clone()).unwrap(),
        colors: serde_json::from_value(value["colors"].clone()).unwrap(),
        advanced: value["advanced"].as_bool().unwrap(),
    };
    let existing = draft
        .editing_id
        .as_deref()
        .filter(|id| catalog.custom.iter().any(|t| t.id == *id));
    let target =
        themes::editor::merge_target(&catalog, &draft.name, existing).map(|t| t.id.clone());
    match themes::editor::save(&catalog,&draft){Err(error)=>json!({"target":target,"error":error}).to_string(),Ok(plan)=>json!({"target":target,"saved":themes::library::definition_value(&catalog,&plan.theme),"created":plan.created,"mergedAppearance":plan.merged_appearance}).to_string()}
}
