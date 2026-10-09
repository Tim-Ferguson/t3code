//! `encodeURIComponent`, for building API paths from repository and project names.

/// Percent-encodes everything but `A-Z a-z 0-9 - _ . ! ~ * ' ( )`, byte for byte as
/// JavaScript's `encodeURIComponent` does.
pub fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}
