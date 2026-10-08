//! Account and sign-in terminal policy from ProviderAuthenticationSection and
//! ProviderAuthTerminal. Credentials stay in the mounted UI, never a draft cache.
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub active: bool,
    pub signed_in: bool,
    pub discovering: bool,
    pub needs_external_setup: bool,
    pub description: String,
    pub status_message: Option<String>,
    pub disabled: bool,
    pub draft_id: String,
    pub url: Option<String>,
    pub callback: bool,
    pub method_picker: bool,
    pub selected_method: String,
    pub start_disabled: bool,
    pub start_label: String,
    pub can_logout: bool,
}
pub fn account(
    provider: &Value,
    auth: Option<&Value>,
    query_error: bool,
    read_only: bool,
    pending: bool,
    method: &str,
    environment: &str,
) -> Account {
    let auth = auth.unwrap_or(&Value::Null);
    let phase = auth["phase"].as_str();
    let interaction = &auth["interaction"];
    let kind = interaction["type"].as_str();
    let active = matches!(phase, Some("starting" | "waiting" | "verifying"));
    let signed_in = provider["auth"]["status"] == "authenticated"
        || (provider["auth"]["status"] == "unknown" && phase == Some("succeeded"));
    let methods = auth["methods"].as_array();
    let discovering = provider["driver"] == "acpRegistry"
        && !active
        && !signed_in
        && !query_error
        && auth.get("methods").is_none();
    let needs_external_setup = !active
        && !signed_in
        && (provider["setup"]["canAuthenticate"] == false
            || (provider["driver"] == "acpRegistry" && methods.is_some_and(Vec::is_empty)));
    let description = if active {
        match phase {
            Some("starting") => "Starting sign-in…",
            Some("verifying") => "Checking your account…",
            _ => match kind {
                Some("terminal") => "Complete sign-in in the terminal below.",
                Some("credentials") => "Enter your credentials below.",
                _ => "Finish signing in in your browser.",
            },
        }
        .to_owned()
    } else if signed_in {
        "Signed in.".into()
    } else if discovering {
        "Discovering sign-in methods…".into()
    } else if needs_external_setup {
        "No in-app sign-in advertised. Follow the provider's docs to finish setup.".into()
    } else {
        format!("Sign in on {environment}.")
    };
    let disabled = read_only || pending || query_error || discovering;
    let url = if matches!(kind, Some("browser" | "deviceCode")) {
        interaction["url"].as_str()
    } else {
        auth["authorizationUrl"].as_str()
    }
    .map(str::to_owned);
    let selected_method =
        if methods.is_some_and(|methods| methods.iter().any(|entry| entry["id"] == method)) {
            method.to_owned()
        } else {
            String::new()
        };
    Account {
        active,
        signed_in,
        discovering,
        needs_external_setup,
        description,
        status_message: if phase == Some("failed") {
            auth["message"].as_str().map(str::to_owned)
        } else {
            None
        },
        disabled,
        draft_id: format!(
            "{}:{}",
            auth["flowId"].as_str().unwrap_or_default(),
            interaction["id"].as_str().unwrap_or_default()
        ),
        callback: url.as_ref().is_some_and(|value| !value.is_empty())
            && if kind == Some("browser") {
                interaction["acceptsCallback"] == true
            } else {
                interaction.is_null()
            },
        url,
        method_picker: !active && methods.is_some_and(|methods| methods.len() > 1),
        selected_method,
        start_disabled: disabled
            || provider["enabled"] != true
            || provider["installed"] != true
            || auth.is_null(),
        start_label: if signed_in {
            "Change account"
        } else if matches!(phase, Some("failed" | "cancelled")) {
            "Retry sign-in"
        } else {
            "Sign in"
        }
        .into(),
        can_logout: !active
            && signed_in
            && provider["auth"]
                .get("canLogout")
                .filter(|value| !value.is_null())
                .unwrap_or(&provider["setup"]["canAuthenticate"])
                == true,
    }
}

pub fn redacted_placeholder(value: &str) -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut state = 0x811c9dc5u32;
    for unit in value.encode_utf16() {
        state = (state ^ u32::from(unit)).wrapping_mul(0x01000193);
    }
    value
        .chars()
        .map(|ch| {
            if matches!(ch, '@' | '.' | '-' | '_') {
                ch
            } else {
                state = (state ^ (state >> 13)).wrapping_mul(0x85ebca6b);
                state = (state ^ (state >> 16)).wrapping_mul(0xc2b2ae35);
                char::from(ALPHABET[(state as i32).unsigned_abs() as usize % ALPHABET.len()])
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "lowercase")]
pub enum Paint {
    None,
    Append(String),
    Reset(String),
}
pub fn terminal_paint(written: &mut i64, output: &str, offset: Option<i64>) -> Paint {
    let length = output.encode_utf16().count() as i64;
    let offset = offset.unwrap_or(length);
    let delta = offset - *written;
    *written = offset;
    if delta > 0 && delta <= length {
        let skip = (length - delta) as usize;
        let units = output.encode_utf16().collect::<Vec<_>>();
        Paint::Append(String::from_utf16_lossy(&units[skip..]))
    } else if delta != 0 {
        Paint::Reset(output.into())
    } else {
        Paint::None
    }
}
/// Keep each response inside the wire's UTF-16 bound without splitting a Rust
/// scalar. Original JS can split surrogate pairs; isolated surrogates remain a
/// separate compatibility gap until the JSON boundary can represent them.
pub fn terminal_chunks(data: &str) -> Vec<String> {
    if data.is_empty() {
        return vec![String::new()];
    }
    let mut result = vec![];
    let mut chunk = String::new();
    let mut units = 0;
    for ch in data.chars() {
        if units + ch.len_utf16() > 4096 {
            result.push(std::mem::take(&mut chunk));
            units = 0;
        }
        units += ch.len_utf16();
        chunk.push(ch);
    }
    result.push(chunk);
    result
}

/// String.trim from the source UI (ECMAScript whitespace, not Unicode NEL).
pub fn trim(value: &str) -> &str {
    value.trim_matches(|ch|matches!(ch,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'))
}
