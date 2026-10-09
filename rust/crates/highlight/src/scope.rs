use crate::scanner::Input;
use std::ops::Range;

/// TextMate scope substitutions are distinct from regex backreferences: missing
/// captures retain the placeholder, and leading dots are removed before casing.
pub fn resolve(template: &str, line: &Input, captures: &[Range<usize>]) -> String {
    let source: Vec<u16> = template.encode_utf16().collect();
    let mut result = Vec::new();
    let mut index = 0;
    while index < source.len() {
        if source[index] != b'$' as u16 { result.push(source[index]); index += 1; continue; }
        let start = index;
        let mut end = index + 1;
        let braced = source.get(end) == Some(&(b'{' as u16));
        if braced { end += 1; }
        let digit_start = end;
        let mut number = Some(0usize);
        while source.get(end).is_some_and(|unit| (b'0' as u16..=b'9' as u16).contains(unit)) {
            number = number.and_then(|n| n.checked_mul(10)).and_then(|n| n.checked_add(usize::from(source[end] - b'0' as u16)));
            end += 1;
        }
        if digit_start == end { result.push(source[index]); index += 1; continue; }
        let mut case = 0;
        if braced {
            let suffix: Vec<u16> = ":/downcase}".encode_utf16().collect();
            let upper: Vec<u16> = ":/upcase}".encode_utf16().collect();
            if source[end..].starts_with(&suffix) { case = -1; end += suffix.len(); }
            else if source[end..].starts_with(&upper) { case = 1; end += upper.len(); }
            else { result.push(source[index]); index += 1; continue; }
        }
        if let Some(capture) = number.and_then(|number| captures.get(number)) {
            let a = capture.start.min(line.len());
            let b = capture.end.min(line.len());
            let captured = &line.units()[a.min(b)..a.max(b)];
            let stripped = captured.iter().position(|unit| *unit != b'.' as u16).unwrap_or(captured.len());
            let captured = &captured[stripped..];
            if case == 0 { result.extend_from_slice(captured); }
            else {
                let text = String::from_utf16_lossy(captured);
                let converted = if case < 0 { text.to_lowercase() } else { text.to_uppercase() };
                result.extend(converted.encode_utf16());
            }
        } else { result.extend_from_slice(&source[start..end]); }
        index = end;
    }
    String::from_utf16_lossy(&result)
}

pub fn push(scopes: &[String], name: Option<&str>, line: &Input, captures: &[Range<usize>]) -> Vec<String> {
    let mut scopes = scopes.to_vec();
    if let Some(name) = name {
        scopes.extend(resolve(name, line, captures).split(' ').filter(|scope| !scope.is_empty()).map(str::to_owned));
    }
    scopes
}
