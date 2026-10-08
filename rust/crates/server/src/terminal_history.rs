//! Source-derived bounded PTY history and replay-safe control-sequence filter.
//! Live output is unchanged; only persisted/replayed output uses this filter.
use std::collections::VecDeque;
const CHUNK_UNITS: usize = 16 * 1024;
#[derive(Clone)]
struct Chunk {
    data: String,
    bytes: usize,
    newlines: usize,
    units: usize,
}
#[derive(Clone)]
pub struct BoundedTerminalHistory {
    max_lines: usize,
    max_bytes: usize,
    chunks: VecDeque<Chunk>,
    bytes: usize,
    newlines: usize,
    cached: Option<String>,
}
impl BoundedTerminalHistory {
    pub fn new(max_lines: usize, initial: &str, max_bytes: usize) -> Self {
        let mut value = Self {
            max_lines,
            max_bytes,
            chunks: VecDeque::new(),
            bytes: 0,
            newlines: 0,
            cached: Some(String::new()),
        };
        value.append(initial);
        value
    }
    pub fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cached = None;
        if self.max_bytes == 0 || self.max_lines == 0 {
            self.clear();
            if self.max_bytes > 0 && text.ends_with('\n') {
                self.push("\n");
            }
            return;
        }
        let mut start = 0;
        let mut units = 0;
        for (index, character) in text.char_indices() {
            if units + character.len_utf16() > CHUNK_UNITS {
                self.push(&text[start..index]);
                self.trim();
                start = index;
                units = 0;
            }
            units += character.len_utf16();
        }
        self.push(&text[start..]);
        self.trim();
    }
    fn push(&mut self, data: &str) {
        if data.is_empty() {
            return;
        }
        let bytes = data.len();
        let units = data.encode_utf16().count();
        let newlines = data.bytes().filter(|byte| *byte == b'\n').count();
        if let Some(last) = self
            .chunks
            .back_mut()
            .filter(|last| last.units + units <= CHUNK_UNITS)
        {
            last.data.push_str(data);
            last.bytes += bytes;
            last.units += units;
            last.newlines += newlines;
        } else {
            self.chunks.push_back(Chunk {
                data: data.into(),
                bytes,
                newlines,
                units,
            });
        }
        self.bytes += bytes;
        self.newlines += newlines;
        self.cached = None;
    }
    fn discard(&mut self) {
        if let Some(first) = self.chunks.pop_front() {
            self.bytes -= first.bytes;
            self.newlines -= first.newlines;
        }
    }
    fn trim_prefix(&mut self, offset: usize) {
        let first = self.chunks.front_mut().unwrap();
        if offset == first.bytes {
            self.discard();
            return;
        }
        let prefix = &first.data[..offset];
        let lines = prefix.bytes().filter(|b| *b == b'\n').count();
        let units = prefix.encode_utf16().count();
        first.data = first.data[offset..].into();
        first.bytes -= offset;
        first.newlines -= lines;
        first.units -= units;
        self.bytes -= offset;
        self.newlines -= lines;
    }
    fn trim(&mut self) {
        let trailing_newline = self
            .chunks
            .back()
            .is_some_and(|chunk| chunk.data.ends_with('\n'));
        let mut drop_lines =
            (self.newlines + usize::from(!trailing_newline)).saturating_sub(self.max_lines);
        while drop_lines > 0 {
            let first = self.chunks.front().unwrap();
            if first.newlines < drop_lines {
                drop_lines -= first.newlines;
                self.discard();
                continue;
            }
            let offset = first
                .data
                .match_indices('\n')
                .nth(drop_lines - 1)
                .unwrap()
                .0
                + 1;
            self.trim_prefix(offset);
            drop_lines = 0;
        }
        while self.bytes > self.max_bytes {
            let first = self.chunks.front().unwrap();
            let remove = self.bytes - self.max_bytes;
            if first.bytes <= remove {
                self.discard();
                continue;
            }
            let offset = first
                .data
                .char_indices()
                .map(|(index, character)| index + character.len_utf8())
                .find(|end| *end >= remove)
                .unwrap();
            self.trim_prefix(offset);
        }
    }
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.bytes = 0;
        self.newlines = 0;
        self.cached = Some(String::new());
    }
    pub fn value(&mut self) -> &str {
        self.cached.get_or_insert_with(|| {
            let mut result = String::with_capacity(self.bytes);
            for chunk in &self.chunks {
                result.push_str(&chunk.data)
            }
            result
        })
    }
}
fn csi_strip(body: &str, final_byte: char) -> bool {
    let digits = |body: &str| {
        body.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ';' | '?'))
    };
    match final_byte {
        'n' => true,
        'R' => digits(body),
        'c' => body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ';' | '?' | '>')),
        'p' | 'y' => body.strip_suffix('$').is_some_and(digits),
        'q' => body
            .strip_prefix('>')
            .is_some_and(|body| body.chars().all(|c| c.is_ascii_digit() || c == ';')),
        'u' => body.starts_with('?'),
        _ => false,
    }
}
fn string_strip(kind: char, content: &str) -> bool {
    if matches!(kind, ']' | '\u{9d}') {
        return ["10;", "11;", "12;"].iter().any(|prefix| {
            content
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.starts_with('?') || tail.starts_with("rgb:"))
        });
    }
    if matches!(kind, 'P' | '\u{90}') {
        let content = content.strip_prefix(['0', '1']).unwrap_or(content);
        return ["$q", "$r", "+q", "+r"]
            .iter()
            .any(|prefix| content.starts_with(prefix));
    }
    false
}
fn string_end(input: &str, start: usize) -> Option<(usize, usize)> {
    for (offset, c) in input[start..].char_indices() {
        let index = start + offset;
        if matches!(c, '\u{7}' | '\u{9c}') {
            return Some((index, index + c.len_utf8()));
        }
        if c == '\u{1b}' && input[index + 1..].starts_with('\\') {
            return Some((index, index + 2));
        }
    }
    None
}
#[derive(Default)]
pub struct TerminalHistoryFilter {
    pending: String,
}
impl TerminalHistoryFilter {
    pub fn clear(&mut self) {
        self.pending.clear();
    }
    pub fn feed(&mut self, data: &str) -> String {
        let mut input = std::mem::take(&mut self.pending);
        input.push_str(data);
        let mut visible = String::new();
        let mut index = 0;
        while index < input.len() {
            let first = input[index..].chars().next().unwrap();
            let (kind, start) = if first == '\u{1b}' {
                let next = index + 1;
                let Some(kind) = input[next..].chars().next() else {
                    break;
                };
                (kind, next + kind.len_utf8())
            } else {
                (first, index + first.len_utf8())
            };
            if (first == '\u{1b}' && kind == '[') || first == '\u{9b}' {
                let Some((offset, final_byte)) = input[start..]
                    .char_indices()
                    .find(|(_, c)| matches!(*c, '@'..='~'))
                else {
                    break;
                };
                let end = start + offset + final_byte.len_utf8();
                if !csi_strip(&input[start..start + offset], final_byte) {
                    visible.push_str(&input[index..end]);
                }
                index = end;
                continue;
            }
            if (first == '\u{1b}' && matches!(kind, ']' | 'P' | '^' | '_'))
                || matches!(first, '\u{9d}' | '\u{90}' | '\u{9e}' | '\u{9f}')
            {
                let Some((content_end, end)) = string_end(&input, start) else {
                    break;
                };
                if !string_strip(kind, &input[start..content_end]) {
                    visible.push_str(&input[index..end]);
                }
                index = end;
                continue;
            }
            if first == '\u{1b}' {
                let escape_start = index + 1;
                let mut cursor = escape_start;
                while let Some(c) = input[cursor..].chars().next() {
                    if !matches!(c, ' '..='/') {
                        break;
                    }
                    cursor += c.len_utf8();
                }
                let Some(c) = input[cursor..].chars().next() else {
                    break;
                };
                let end = if matches!(c, '0'..='~') {
                    cursor + c.len_utf8()
                } else {
                    escape_start + kind.len_utf8()
                };
                visible.push_str(&input[index..end]);
                index = end;
                continue;
            }
            visible.push(first);
            index += first.len_utf8();
        }
        self.pending.push_str(&input[index..]);
        visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_and_every_control_split_match_original_pure_manager_functions() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/terminal-history.json")).unwrap();
        for (index, case) in fixture["filters"].as_array().unwrap().iter().enumerate() {
            let mut filter = TerminalHistoryFilter::default();
            let mut outputs = Vec::new();
            for chunk in case["chunks"].as_array().unwrap() {
                outputs.push(filter.feed(chunk.as_str().unwrap()));
            }
            assert_eq!(
                serde_json::json!(outputs),
                case["outputs"],
                "filter fixture{index}"
            );
            assert_eq!(
                filter.pending,
                case["pending"].as_str().unwrap(),
                "filter pending{index}"
            );
        }
        for (index, case) in fixture["histories"].as_array().unwrap().iter().enumerate() {
            let mut history = BoundedTerminalHistory::new(
                case["lines"].as_u64().unwrap() as usize,
                "",
                case["bytes"].as_u64().unwrap() as usize,
            );
            let mut values = Vec::new();
            for chunk in case["chunks"].as_array().unwrap() {
                history.append(chunk.as_str().unwrap());
                values.push(history.value().to_owned());
            }
            assert_eq!(
                serde_json::json!(values),
                case["values"],
                "history fixture{index}"
            );
            history.clear();
            assert_eq!(history.value(), "");
        }
    }
    #[test]
    fn retained_chunks_remain_bounded_after_large_ascii_unicode_and_line_appends() {
        let mut history = BoundedTerminalHistory::new(5000, "", 4096);
        for _ in 0..100 {
            history.append(&format!("{}\n", "😀".repeat(2000)));
        }
        assert!(history.value().len() <= 4096);
        assert!(history.chunks.len() <= 2);
        history.append(&"x\n".repeat(10000));
        assert!(history.value().len() <= 4096);
        assert!(history.value().ends_with("x\n"));
    }
}
