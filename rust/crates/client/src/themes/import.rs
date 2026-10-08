//! Source file-import policy, shared by the dialog and batch/package callers.
use super::{Catalog, Definition, library, vscode};
use serde_json::{Value, json};

pub const MAX_FILE_BYTES: usize = 256 * 1024;
pub fn preferred_name(file: &str) -> String {
    let stem = match file.rfind('.') {
        Some(index) if index + 1 < file.len() => &file[..index],
        _ => file,
    };
    vscode::humanize_name(stem)
}
pub fn oversized(bytes: usize) -> Option<String> {
    fn size(bytes: usize) -> String {
        if bytes >= 1024 * 1024 {
            format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
        } else if bytes >= 1024 {
            format!("{} KB", (bytes as f64 / 1024. + 0.5).floor() as usize)
        } else {
            format!("{bytes} bytes")
        }
    }
    (bytes > MAX_FILE_BYTES).then(|| {
        format!(
            "That file is {}. Theme files are only a few KB, so this one was not read (limit {}).",
            size(bytes),
            size(MAX_FILE_BYTES)
        )
    })
}
pub fn parse(catalog: &Catalog, text: &str) -> Result<Definition, String> {
    if let Some(error) = oversized(text.encode_utf16().count()) {
        return Err(error);
    }
    let value: Value = serde_json::from_str(text).map_err(|cause| cause.to_string())?;
    if vscode::is_file(&value) {
        vscode::import(catalog, &value)
    } else {
        library::import(catalog, &value)
    }
}
fn prefix(value: &str, length: usize) -> String {
    // JS slice may bisect an astral pair at this limit. Keep valid UTF-8 here;
    // exact lone-surrogate persistence remains an explicit porting gap.
    let mut units = 0;
    value
        .chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= length
        })
        .collect()
}
fn renamed(catalog: &Catalog, theme: &Definition, name: &str) -> Result<Definition, String> {
    let mut value =
        json!({"version":1,"name":name,"appearance":theme.appearance,"colors":theme.colors});
    if let Some(variants) = &theme.variants {
        value["variants"] = json!(variants);
    }
    if theme.managed == Some(true) {
        value["managed"] = json!(true);
    }
    library::import(catalog, &value)
}
pub fn versioned_copy(
    catalog: &Catalog,
    theme: &Definition,
    preferred_name: Option<&str>,
) -> Result<Definition, String> {
    if let Some(name) = preferred_name
        .filter(|name| !name.is_empty() && name.to_lowercase() != theme.label.to_lowercase())
    {
        let candidate = renamed(catalog, theme, &prefix(name, 48))?;
        if !catalog.custom.iter().any(|theme| theme.id == candidate.id) {
            return Ok(candidate);
        }
    }
    for copy in 1..100 {
        let suffix = format!(" ({copy})");
        let candidate = renamed(
            catalog,
            theme,
            &(prefix(&theme.label, 48 - suffix.len()) + &suffix),
        )?;
        if !catalog.custom.iter().any(|theme| theme.id == candidate.id) {
            return Ok(candidate);
        }
    }
    Err(format!("Too many copies of \"{}\".", theme.label))
}
