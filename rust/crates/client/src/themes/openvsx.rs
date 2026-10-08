//! Portable Open VSX metadata and checksummed VSIX theme ingestion.
//! All limits and validation order follow the original openVsxThemes.ts importer.
use super::{Catalog, Definition, color, vscode};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use std::{collections::BTreeMap, io::Read};
use url::Url;

pub const SEARCH_URL: &str = "https://open-vsx.org/api/-/search";
pub const MAX_PACKAGE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_SEARCH_BYTES: usize = 512 * 1024;
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_THEMES: usize = 40;
const MAX_FILES: usize = 5000;
const MAX_EXPANDED: u64 = 100 * 1024 * 1024;
const USED: &[&str] = &[
    "activityBar.background",
    "activityBarBadge.background",
    "badge.background",
    "button.background",
    "button.foreground",
    "contrastBorder",
    "descriptionForeground",
    "disabledForeground",
    "dropdown.background",
    "dropdown.border",
    "editor.background",
    "editor.foreground",
    "editor.selectionBackground",
    "editorCursor.foreground",
    "editorError.foreground",
    "editorGroup.border",
    "editorPane.background",
    "editorWarning.foreground",
    "editorWidget.background",
    "errorForeground",
    "focusBorder",
    "foreground",
    "input.border",
    "input.placeholderForeground",
    "list.activeSelectionBackground",
    "list.hoverBackground",
    "list.inactiveSelectionBackground",
    "menu.background",
    "panel.background",
    "panel.border",
    "progressBar.background",
    "quickInput.background",
    "scrollbarSlider.background",
    "sideBar.background",
    "sideBar.border",
    "sideBar.foreground",
    "terminal.background",
    "terminal.foreground",
    "terminal.selectionBackground",
    "terminalCursor.foreground",
    "textCodeBlock.background",
    "textLink.foreground",
];
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Extension {
    pub id: String,
    pub collection_id: String,
    pub name: String,
    pub publisher: String,
    pub description: String,
    pub download_count: f64,
    pub icon_url: Option<String>,
    pub source_url: Option<String>,
    pub manifest_url: String,
    pub sha256_url: String,
    pub vsix_url: String,
    pub version: String,
    pub license: String,
}
fn hash(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}
fn short_hash(value: &str) -> String {
    hash(value.as_bytes())[..12].to_owned()
}
pub fn theme_id(extension: &str, source: &str) -> String {
    format!("ovx-theme-{}", short_hash(&format!("{extension}:{source}")))
}
pub fn collection_id(extension: &str) -> String {
    let id = format!("open-vsx:{}", extension.to_lowercase());
    if id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".:-".contains(&b))
    {
        id
    } else {
        format!("open-vsx:{}", short_hash(extension))
    }
}
pub fn trusted_url(value: &Value) -> Option<String> {
    let url = Url::parse(value.as_str()?).ok()?;
    (url.scheme() == "https" && url.host_str() == Some("open-vsx.org")).then(|| url.to_string())
}
pub fn source_url(value: &Value) -> Option<String> {
    let raw = value.as_str().or_else(|| value["url"].as_str())?;
    let url = Url::parse(raw).ok()?;
    (url.scheme() == "https" && url.username().is_empty() && url.password().is_none())
        .then(|| url.to_string())
}
pub fn detail(value: &Value) -> Result<Option<Extension>, String> {
    let malformed = || "Open VSX returned malformed theme details.".to_owned();
    if !value.is_object() || !value["files"].is_object() {
        return Err(malformed());
    }
    let text = |key| value[key].as_str().map(color::trim).unwrap_or("");
    let namespace = text("namespace");
    let name = text("name");
    let version = text("version");
    let license = text("license");
    let manifest_url = trusted_url(&value["files"]["manifest"]);
    let sha256_url = trusted_url(&value["files"]["sha256"]);
    let vsix_url = trusted_url(&value["files"]["download"]);
    if namespace.is_empty()
        || name.is_empty()
        || version.is_empty()
        || manifest_url.is_none()
        || sha256_url.is_none()
        || vsix_url.is_none()
    {
        return Err(malformed());
    }
    if ![
        "0BSD",
        "Apache-2.0",
        "BSD-2-Clause",
        "BSD-3-Clause",
        "CC0-1.0",
        "ISC",
        "MIT",
        "MPL-2.0",
        "Unlicense",
    ]
    .contains(&license)
    {
        return Ok(None);
    }
    let id = format!("{namespace}.{name}");
    let display = text("displayName");
    Ok(Some(Extension {
        id: id.clone(),
        collection_id: collection_id(&id),
        name: if display.is_empty() { name } else { display }.into(),
        publisher: namespace.into(),
        description: value["description"].as_str().unwrap_or("").into(),
        download_count: value["downloadCount"]
            .as_f64()
            .filter(|v| v.is_finite())
            .unwrap_or(0.),
        icon_url: trusted_url(&value["files"]["icon"]),
        source_url: source_url(&value["repository"])
            .or_else(|| source_url(&value["homepage"]))
            .or_else(|| source_url(&value["url"])),
        manifest_url: manifest_url.unwrap(),
        sha256_url: sha256_url.unwrap(),
        vsix_url: vsix_url.unwrap(),
        version: version.into(),
        license: license.into(),
    }))
}
pub fn contributions(manifest: &Value) -> Vec<&Value> {
    manifest["contributes"]["themes"]
        .as_array()
        .map(|items| items.iter().filter(|v| v.is_object()).collect())
        .unwrap_or_default()
}
pub fn license_matches(manifest: &Value, license: &str) -> bool {
    manifest["license"]
        .as_str()
        .is_some_and(|v| color::trim(v).to_lowercase() == license.to_lowercase())
}
pub fn normalize_path(path: &str, relative_to: &str) -> Result<String, String> {
    if path.encode_utf16().count() > 1024
        || path.contains('\0')
        || path.starts_with('/')
        || (path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && path.as_bytes().get(1) == Some(&b':'))
    {
        return Err("Theme path is not a safe relative package path.".into());
    }
    let normalized = path.replace('\\', "/");
    let mut segments: Vec<_> = relative_to.split('/').collect();
    segments.pop();
    for segment in normalized.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.len() <= 1 {
                    return Err("Theme path escapes the extension package.".into());
                }
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    if segments.first() != Some(&"extension") {
        segments.insert(0, "extension");
    }
    Ok(segments.join("/"))
}
/// JSONC's comments and trailing commas are accepted; strings are never rewritten.
pub fn jsonc(source: &str, description: &str) -> Result<Value, String> {
    super::jsonc::parse(source)
        .map(|value| value.wire())
        .map_err(|_| format!("{description} is not valid JSON."))
}
fn jsonc_node(source: &str, description: &str) -> Result<super::jsonc::Node, String> {
    super::jsonc::parse(source).map_err(|_| format!("{description} is not valid JSON."))
}
pub fn manifest(source: &str) -> Result<Value, String> {
    let node = jsonc_node(source, "Extension manifest")?;
    let mut value = Map::new();
    for key in ["publisher", "name", "version", "license"] {
        if let Some(field) = node.get(key) {
            value.insert(key.into(), field.wire());
        }
    }
    if let Some(items) = node
        .get("contributes")
        .and_then(|v| v.get("themes"))
        .and_then(|v| v.array())
    {
        let items: Vec<_> = items
            .iter()
            .filter(|v| v.own().is_some())
            .map(|v| {
                let mut fields = Map::new();
                for key in ["path", "label", "uiTheme"] {
                    if let Some(field) = v.get(key) {
                        fields.insert(key.into(), field.wire());
                    }
                }
                Value::Object(fields)
            })
            .collect();
        value.insert("contributes".into(), json!({"themes":items}));
    }
    Ok(Value::Object(value))
}
fn sanitized_node(node: &super::jsonc::Node) -> Value {
    let mut colors = Map::new();
    if let Some(fields) = node.get("colors").and_then(|v| v.own()) {
        for (key, field) in fields {
            if USED.contains(&key.as_str())
                && field
                    .string()
                    .is_some_and(|value| value.encode_utf16().count() <= 128)
            {
                colors.insert(key.clone(), field.wire());
            }
        }
    }
    let mut value = json!({"colors":colors});
    if let Some(include) = node.get("include").and_then(|v| v.string()) {
        value["include"] = json!(include);
    }
    value
}
pub fn decode_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
}
/// JSON/JSONC scanners accept decimal overflow as a JS Number. These metadata
/// consumers discard non-string fields (and nonfinite download counts); represent
/// overflow as null, matching JSON.stringify without weakening numeric syntax.
pub fn json_value(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    static DECIMAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?$").unwrap()
    });
    let mut output = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut quoted = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if quoted {
            output.push(b);
            if escaped {
                escaped = false
            } else if b == b'\\' {
                escaped = true
            } else if b == b'"' {
                quoted = false
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            quoted = true;
            output.push(b);
            i += 1;
            continue;
        }
        if b == b'-' || b.is_ascii_digit() {
            let start = i;
            while bytes.get(i).is_some_and(|c| {
                !matches!(
                    c,
                    b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}' | b':' | b'[' | b'{' | b'"'
                )
            }) {
                i += 1;
            }
            let token = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
            if DECIMAL.is_match(token) && token.parse::<f64>().is_ok_and(|v| !v.is_finite()) {
                output.extend_from_slice(b"null");
            } else {
                output.extend_from_slice(&bytes[start..i]);
            }
            continue;
        }
        output.push(b);
        i += 1;
    }
    serde_json::from_slice(&output)
}
pub fn sanitize(value: &Value) -> Value {
    let mut colors = Map::new();
    if let Some(input) = value["colors"].as_object() {
        for (key, value) in input {
            if USED.contains(&key.as_str())
                && value
                    .as_str()
                    .is_some_and(|v| v.encode_utf16().count() <= 128)
            {
                colors.insert(key.clone(), value.clone());
            }
        }
    }
    let mut result = json!({"colors":colors});
    if value["include"].is_string() {
        result["include"] = value["include"].clone();
    }
    result
}
#[derive(Clone, Debug)]
struct Entry {
    compressed: usize,
    expanded: usize,
    method: u16,
    data: usize,
}
pub struct Archive<'a> {
    bytes: &'a [u8],
    entries: BTreeMap<String, Entry>,
}
fn u16le(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn u32le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn field16(bytes: &[u8], base: usize, field: usize) -> Option<u16> {
    u16le(bytes, base.checked_add(field)?)
}
fn field32(bytes: &[u8], base: usize, field: usize) -> Option<u32> {
    u32le(bytes, base.checked_add(field)?)
}
pub fn inspect_directory(bytes: &[u8]) -> Result<usize, String> {
    let minimum = bytes.len().saturating_sub(65557);
    let end = (minimum..=bytes.len().saturating_sub(22))
        .rev()
        .find(|&offset| {
            u32le(bytes, offset) == Some(0x06054b50)
                && field16(bytes, offset, 20).is_some_and(|comment| {
                    offset
                        .checked_add(22)
                        .and_then(|v| v.checked_add(comment as usize))
                        == Some(bytes.len())
                })
        })
        .ok_or("That extension package has no ZIP directory.")?;
    let invalid = || "That extension package has an invalid ZIP directory.".to_owned();
    let size = field32(bytes, end, 12).ok_or_else(invalid)? as usize;
    let mut offset = field32(bytes, end, 16).ok_or_else(invalid)? as usize;
    if offset.checked_add(size) != Some(end) {
        return Err(invalid());
    }
    let mut count = 0;
    let mut total = 0u64;
    while offset < end {
        if offset.checked_add(46).is_none_or(|v| v > end)
            || u32le(bytes, offset) != Some(0x02014b50)
        {
            return Err(invalid());
        }
        count += 1;
        if count > MAX_FILES {
            return Err("That extension package has too many files.".into());
        }
        let compressed = field32(bytes, offset, 20).ok_or_else(invalid)?;
        let expanded = field32(bytes, offset, 24).ok_or_else(invalid)?;
        if compressed == u32::MAX || expanded == u32::MAX {
            return Err("That extension package has unsupported ZIP64 metadata.".into());
        }
        total += expanded as u64;
        if total > MAX_EXPANDED {
            return Err("That extension package expands beyond the safe import limit.".into());
        }
        if expanded > 0 && (compressed == 0 || expanded as f64 / compressed as f64 > 200.) {
            return Err("That extension package has an unsafe compression ratio.".into());
        }
        offset = offset
            .checked_add(
                46 + field16(bytes, offset, 28).ok_or_else(invalid)? as usize
                    + field16(bytes, offset, 30).ok_or_else(invalid)? as usize
                    + field16(bytes, offset, 32).ok_or_else(invalid)? as usize,
            )
            .ok_or_else(invalid)?;
    }
    if offset != end {
        return Err(invalid());
    }
    Ok(end)
}
impl<'a> Archive<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self, String> {
        let end = inspect_directory(bytes)?;
        let bytes = &bytes[..end
            .checked_add(22)
            .ok_or("That extension package has an invalid ZIP directory.")?];
        let mut offset = field32(bytes, end, 16).unwrap() as usize;
        let mut entries = BTreeMap::new();
        let invalid = || "That Open VSX extension package could not be opened.".to_owned();
        while offset < end {
            let size = field16(bytes, offset, 28).unwrap() as usize;
            let extra = field16(bytes, offset, 30).unwrap() as usize;
            let comment = field16(bytes, offset, 32).unwrap() as usize;
            let raw = String::from_utf8_lossy(
                bytes
                    .get(
                        offset.checked_add(46).ok_or_else(invalid)?
                            ..offset
                                .checked_add(46)
                                .and_then(|v| v.checked_add(size))
                                .ok_or_else(invalid)?,
                    )
                    .ok_or_else(invalid)?,
            )
            .into_owned();
            normalize_path(&raw, "extension/").map_err(|_| invalid())?;
            let mut segments = Vec::new();
            for segment in raw.split('/') {
                match segment {
                    "" | "." => {}
                    ".." => {
                        segments.pop();
                    }
                    _ => segments.push(segment),
                }
            }
            let name = segments.join("/");
            let local = field32(bytes, offset, 42).unwrap() as usize;
            if u32le(bytes, local) != Some(0x04034b50)
                || field16(bytes, offset, 8).is_some_and(|flags| flags & 1 != 0)
            {
                return Err(invalid());
            }
            let method = field16(bytes, offset, 10).unwrap();
            if !matches!(method, 0 | 8) {
                return Err(invalid());
            }
            let data = local
                .checked_add(
                    30 + field16(bytes, local, 26).ok_or_else(invalid)? as usize
                        + field16(bytes, local, 28).ok_or_else(invalid)? as usize,
                )
                .ok_or_else(invalid)?;
            let compressed = field32(bytes, offset, 20).unwrap() as usize;
            let expanded = field32(bytes, offset, 24).unwrap() as usize;
            if data
                .checked_add(compressed)
                .is_none_or(|length| length > bytes.len())
            {
                return Err(invalid());
            }
            if !raw.ends_with('/') {
                entries.insert(
                    name.clone(),
                    Entry {
                        compressed,
                        expanded,
                        method,
                        data,
                    },
                );
            }
            offset = offset
                .checked_add(46 + size + extra + comment)
                .ok_or_else(invalid)?;
        }
        Ok(Self { bytes, entries })
    }
    pub fn text(&self, path: &str, description: &str) -> Result<String, String> {
        let entry = self
            .entries
            .get(path)
            .ok_or_else(|| format!("{description} is missing from the extension package."))?;
        if entry.expanded > MAX_TEXT_BYTES {
            return Err(format!("{description} is too large."));
        }
        let raw = self
            .bytes
            .get(
                entry.data
                    ..entry
                        .data
                        .checked_add(entry.compressed)
                        .ok_or("That Open VSX extension package could not be opened.")?,
            )
            .ok_or("That Open VSX extension package could not be opened.")?;
        let mut bytes = Vec::new();
        if entry.method == 0 {
            if raw.len() > MAX_TEXT_BYTES {
                return Err(format!("{description} is too large."));
            }
            bytes.extend_from_slice(raw);
        } else {
            flate2::read::DeflateDecoder::new(raw)
                .take(MAX_TEXT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| format!("{description} could not be decompressed."))?;
        }
        if bytes.len() > MAX_TEXT_BYTES {
            return Err(format!("{description} is too large."));
        }
        if bytes.len() != entry.expanded {
            return Err("Bug : uncompressed data size mismatch".into());
        }
        let text = String::from_utf8_lossy(&bytes);
        Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
    }
    fn theme(
        &self,
        path: &str,
        cache: &mut BTreeMap<String, Value>,
        files: &mut usize,
        ancestors: &mut Vec<String>,
    ) -> Result<Value, String> {
        if ancestors.len() >= 8 {
            return Err("Theme includes are nested too deeply.".into());
        }
        if ancestors.iter().any(|v| v == path) {
            return Err("Theme includes contain a cycle.".into());
        }
        if let Some(value) = cache.get(path) {
            return Ok(value.clone());
        }
        *files += 1;
        if *files > 320 {
            return Err("That extension references too many theme files.".into());
        }
        let mut value = sanitized_node(&jsonc_node(&self.text(path, path)?, path)?);
        if let Some(include) = value["include"].as_str() {
            let include = normalize_path(include, path)?;
            ancestors.push(path.into());
            let result = self.theme(&include, cache, files, ancestors);
            ancestors.pop();
            let base = result?;
            let mut colors = base["colors"].as_object().cloned().unwrap_or_default();
            colors.extend(value["colors"].as_object().unwrap().clone());
            value["colors"] = json!(colors);
        }
        cache.insert(path.into(), value.clone());
        Ok(value)
    }
}
pub fn validate_manifest(source: &str) -> Result<Value, String> {
    let value = manifest(source)?;
    let count = contributions(&value).len();
    if count == 0 {
        return Err("That extension does not contain color themes.".into());
    }
    if count > MAX_THEMES {
        return Err("That extension contains too many color themes to import safely.".into());
    }
    Ok(value)
}
pub fn import_package(
    catalog: &Catalog,
    extension: &Extension,
    manifest: &str,
    bytes: &[u8],
    checksum: &str,
) -> Result<Vec<Definition>, String> {
    validate_manifest(manifest)?;
    if bytes.len() > MAX_PACKAGE_BYTES {
        return Err("That theme extension is too large to import safely.".into());
    }
    if checksum.len() > 256 {
        return Err("That Open VSX checksum response is invalid.".into());
    }
    let checksum = color::trim(checksum)
        .split(|c: char| color::trim(&c.to_string()).is_empty())
        .next()
        .unwrap_or("");
    if checksum.len() != 64 || !checksum.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("That Open VSX theme has an invalid checksum.".into());
    }
    if !hash(bytes).eq_ignore_ascii_case(checksum) {
        return Err("That Open VSX theme failed its integrity check.".into());
    }
    let archive = Archive::open(bytes)?;
    let packaged = self::manifest(&archive.text("extension/package.json", "Extension manifest")?)?;
    let publisher = packaged["publisher"].as_str();
    let name = packaged["name"].as_str();
    if publisher.is_none_or(|v| v.to_lowercase() != extension.publisher.to_lowercase())
        || name.is_none()
        || format!("{}.{}", publisher.unwrap_or(""), name.unwrap_or("")).to_lowercase()
            != extension.id.to_lowercase()
        || packaged["version"].as_str() != Some(&extension.version)
    {
        return Err("That extension package does not match the selected Open VSX theme.".into());
    }
    if !license_matches(&packaged, &extension.license) {
        return Err("That extension package does not match its advertised license.".into());
    }
    let contributions = contributions(&packaged);
    if contributions.is_empty() {
        return Err("That extension does not contain color themes.".into());
    }
    if contributions.len() > MAX_THEMES {
        return Err("That extension contains too many color themes to import safely.".into());
    }
    let mut parsed = Vec::new();
    let mut paths = Vec::new();
    let mut cache = BTreeMap::new();
    let mut files = 0;
    for contribution in contributions {
        let outcome = (|| {
            let path = normalize_path(
                contribution["path"]
                    .as_str()
                    .ok_or("theme path is missing")?,
                "extension/",
            )?;
            let mut value = archive.theme(&path, &mut cache, &mut files, &mut Vec::new())?;
            let label = contribution["label"]
                .as_str()
                .map(color::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or(&extension.name);
            value["displayName"] = json!(label);
            if let Some(mode) = match contribution["uiTheme"].as_str() {
                Some("vs") => Some("light"),
                Some("vs-dark") => Some("dark"),
                Some("hc-black") => Some("hc-black"),
                Some("hc-light") => Some("hc-light"),
                _ => None,
            } {
                value["type"] = json!(mode);
            }
            if !vscode::is_file(&value) {
                return Err("not a VS Code color theme".to_owned());
            }
            let theme = vscode::import(catalog, &value)?;
            Ok((theme, path))
        })();
        match outcome {
            Ok((theme, path)) => {
                parsed.push(vscode::Entry {
                    theme,
                    source_name: path.split('/').next_back().map(str::to_owned),
                });
                paths.push(path);
            }
            Err(_) => {
                return Err(
                    "One or more color themes in that extension could not be imported safely."
                        .into(),
                );
            }
        }
    }
    if parsed.is_empty() {
        return Err("That extension has no compatible color themes.".into());
    }
    let extension_id = extension.id.to_lowercase();
    let mut occurrences = BTreeMap::<String, usize>::new();
    let mut resolved = vscode::resolve_collisions(catalog, &parsed);
    for (theme, path) in resolved.iter_mut().zip(paths) {
        let occurrence = occurrences.entry(path.clone()).or_default();
        let identity = if *occurrence == 0 {
            path
        } else {
            format!("{path}\0{occurrence}")
        };
        *occurrence += 1;
        theme.id = theme_id(&extension_id, &identity);
    }
    let pair_id = |light: &Definition, dark: &Definition| {
        let mut ids = [light.id.as_str(), dark.id.as_str()];
        ids.sort();
        theme_id(&extension_id, &ids.join(":"))
    };
    let paired = vscode::pair(catalog, &resolved, Some(&pair_id));
    let mut themes = vscode::resolve_collisions(
        catalog,
        &paired
            .into_iter()
            .map(|theme| vscode::Entry {
                theme,
                source_name: None,
            })
            .collect::<Vec<_>>(),
    );
    let mut length = 0;
    let name: String = extension
        .name
        .chars()
        .take_while(|c| {
            length += c.len_utf16();
            length <= 48
        })
        .collect();
    for theme in &mut themes {
        theme.collection = Some(json!({"id":extension.collection_id,"label":name}));
    }
    Ok(themes)
}
/// Number(header) semantics used by the source's declared-length guards.
pub fn header_number(raw: &str) -> Option<f64> {
    static NUMBER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^[+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$").unwrap()
    });
    let raw = color::trim(raw);
    if raw.is_empty() {
        return Some(0.);
    }
    match raw {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    if let Some((digits, base)) = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .map(|v| (v, 16))
        .or_else(|| {
            raw.strip_prefix("0b")
                .or_else(|| raw.strip_prefix("0B"))
                .map(|v| (v, 2))
        })
        .or_else(|| {
            raw.strip_prefix("0o")
                .or_else(|| raw.strip_prefix("0O"))
                .map(|v| (v, 8))
        })
    {
        if digits.is_empty() {
            return None;
        }
        let mut result = 0.;
        for ch in digits.chars() {
            let digit = ch.to_digit(base)?;
            if !ch.is_ascii() {
                return None;
            }
            result = result * base as f64 + digit as f64;
        }
        return Some(result);
    }
    NUMBER
        .is_match(raw)
        .then(|| raw.parse::<f64>().ok())
        .flatten()
}
pub fn search_text(query: &str) -> &str {
    color::trim(query)
}
pub fn query_url(query: &str, sort: &str) -> Option<String> {
    let query = color::trim(query);
    if query.is_empty() {
        return None;
    }
    let mut url = Url::parse(SEARCH_URL).unwrap();
    url.query_pairs_mut().extend_pairs([
        ("query", query),
        ("category", "Themes"),
        ("sortBy", sort),
        ("sortOrder", "desc"),
        ("size", "16"),
    ]);
    Some(url.to_string())
}
pub fn identities(value: &Value) -> Result<Vec<(String, String)>, String> {
    let items = value["extensions"]
        .as_array()
        .filter(|_| value.is_object())
        .ok_or("Open VSX returned an unreadable search response.")?;
    Ok(items
        .iter()
        .filter(|v| v.is_object())
        .filter_map(|v| {
            let namespace = v["namespace"].as_str().filter(|v| !v.is_empty())?;
            let name = v["name"].as_str().filter(|v| !v.is_empty())?;
            Some((namespace.into(), name.into()))
        })
        .collect())
}
pub fn detail_url(namespace: &str, name: &str) -> String {
    let encode = |value: &str| {
        value
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect::<String>()
    };
    format!(
        "https://open-vsx.org/api/{}/{}",
        encode(namespace),
        encode(name)
    )
}
