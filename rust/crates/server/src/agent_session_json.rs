//! Streaming field selection for transcript records (`project/AgentSessionJson.ts`).
//!
//! A Codex rollout can be several GiB because one record carries a screenshot or a tool result.
//! Importing reads each record as it streams past, keeps only the fields the import uses
//! (`TRANSCRIPT_RECORD`, the original's `TranscriptRecord` schema), and never allocates the values
//! it does not select. Every allocation is charged to a budget the caller shares across the whole
//! transcript: exhausting it rejects the transcript, never a message within it.
use serde_json::{Map, Value};
use std::io::{self, BufRead};

/// The 128-level nesting limit of the original's reader (`options.maxDepth`).
const MAX_DEPTH: usize = 128;
/// What the original charges for each token it assembles, before the characters of a string.
const TOKEN_COST: usize = 64;

/// `TranscriptJsonLimitError`: the budget or the nesting limit ran out.
#[derive(Debug, PartialEq, Eq)]
pub struct LimitError(pub &'static str);

/// Why a record could not be read.
#[derive(Debug)]
pub enum ReadError {
    Limit(LimitError),
    Io(io::Error),
}
impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl std::fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit(LimitError(message)) => formatter.write_str(message),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

/// One step of the path from a record's root to a value: an object key or an array index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// The shape a path is matched against (`createTranscriptJsonSelector` walks the schema AST).
pub enum Node {
    /// A string, number or boolean: nothing below it is selected.
    Leaf,
    /// Any value, kept whole.
    Unknown,
    Object(&'static [(&'static str, Node)]),
    Array(&'static Node),
    Union(&'static [Node]),
}

impl Node {
    /// `createTranscriptJsonSelector`'s `includes`: whether the value at `path` is part of this shape.
    pub fn includes(&self, path: &[Seg], index: usize) -> bool {
        if index == path.len() {
            return true;
        }
        match self {
            Self::Object(properties) => properties.iter().any(|(name, child)| matches!(&path[index], Seg::Key(key) if key == name) && child.includes(path, index + 1)),
            // An object where an array is expected is kept whole, for the decoder to reject.
            Self::Array(element) => match path[index] {
                Seg::Index(_) => element.includes(path, index + 1),
                Seg::Key(_) => true,
            },
            Self::Union(types) => types.iter().any(|node| node.includes(path, index)),
            Self::Unknown => true,
            Self::Leaf => false,
        }
    }
}

static CONTENT_BLOCK: Node = Node::Object(&[("type", Node::Leaf), ("text", Node::Leaf)]);
/// The original's `TranscriptRecord`: the only fields of a record the import reads.
pub static TRANSCRIPT_RECORD: Node = Node::Object(&[
    ("type", Node::Leaf),
    ("timestamp", Node::Leaf),
    ("cwd", Node::Leaf),
    ("sessionId", Node::Leaf),
    ("aiTitle", Node::Leaf),
    ("isSidechain", Node::Leaf),
    ("isMeta", Node::Leaf),
    ("isCompactSummary", Node::Leaf),
    ("message", Node::Object(&[("role", Node::Leaf), ("content", Node::Union(&[Node::Leaf, Node::Array(&CONTENT_BLOCK)])), ("model", Node::Leaf)])),
    (
        "payload",
        Node::Object(&[
            ("id", Node::Leaf),
            ("session_id", Node::Leaf),
            ("type", Node::Leaf),
            ("role", Node::Leaf),
            ("message", Node::Leaf),
            ("model", Node::Leaf),
            ("cwd", Node::Leaf),
            ("content", Node::Array(&CONTENT_BLOCK)),
            ("internal_chat_message_metadata_passthrough", Node::Unknown),
        ]),
    ),
]);

enum Failure {
    /// Not a complete JSON object or array: the record is dropped.
    Malformed,
    Limit(LimitError),
    Io(io::Error),
}
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
type Parsed<T> = Result<T, Failure>;

/// What the record's bytes cost in UTF-16 code units, as the original's `string.length` counts them.
fn utf16_units(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| (**byte & 0xC0) != 0x80).count() + bytes.iter().filter(|byte| **byte >= 0xF0).count()
}

/// JSON Lines records read one at a time from a stream.
pub struct RecordReader<R: BufRead> {
    reader: R,
}

impl<R: BufRead> RecordReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    /// The next record's selected fields. `Ok(None)` is the end of the stream; `Ok(Some(None))` is a
    /// record that is not one complete JSON object or array (it still counts as a record). `reserve` is
    /// charged for every allocation and may refuse it.
    pub fn next(&mut self, reserve: &mut dyn FnMut(usize) -> Result<(), LimitError>, select: &dyn Fn(&[Seg]) -> bool) -> Result<Option<Option<Value>>, ReadError> {
        if self.reader.fill_buf()?.is_empty() {
            return Ok(None);
        }
        let outcome = Parser { reader: &mut self.reader, reserve, select, depth: 0 }.record();
        // Whatever the outcome, the rest of the line is not this record's.
        let finished = Self::finish_line(&mut self.reader);
        match outcome {
            Ok(value) => {
                finished?;
                Ok(Some(value))
            }
            Err(Failure::Malformed) => {
                finished?;
                Ok(Some(None))
            }
            Err(Failure::Limit(limit)) => Err(ReadError::Limit(limit)),
            Err(Failure::Io(error)) => Err(ReadError::Io(error)),
        }
    }

    /// Discards the rest of the line, including its newline.
    fn finish_line(reader: &mut R) -> io::Result<()> {
        loop {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                return Ok(());
            }
            match buffer.iter().position(|byte| *byte == b'\n') {
                Some(newline) => {
                    reader.consume(newline + 1);
                    return Ok(());
                }
                None => {
                    let length = buffer.len();
                    reader.consume(length);
                }
            }
        }
    }
}

struct Parser<'a, R: BufRead> {
    reader: &'a mut R,
    reserve: &'a mut dyn FnMut(usize) -> Result<(), LimitError>,
    select: &'a dyn Fn(&[Seg]) -> bool,
    depth: usize,
}

impl<R: BufRead> Parser<'_, R> {
    fn charge(&mut self, bytes: usize) -> Parsed<()> {
        (self.reserve)(bytes).map_err(Failure::Limit)
    }

    /// The next byte of this line, without consuming it. A newline ends the record.
    fn peek(&mut self) -> Parsed<Option<u8>> {
        Ok(self.reader.fill_buf()?.first().copied().filter(|byte| *byte != b'\n'))
    }
    fn bump(&mut self) {
        self.reader.consume(1);
    }
    /// Skips JSON white space a buffer at a time: a record can be mostly padding.
    fn skip_whitespace(&mut self) -> Parsed<()> {
        loop {
            let buffer = self.reader.fill_buf()?;
            let blank = buffer.iter().position(|byte| !matches!(byte, b' ' | b'\t' | b'\r')).unwrap_or(buffer.len());
            let reached_other = blank < buffer.len();
            self.reader.consume(blank);
            if reached_other || blank == 0 {
                return Ok(());
            }
        }
    }
    fn expect(&mut self, wanted: u8) -> Parsed<()> {
        if self.peek()? == Some(wanted) {
            self.bump();
            Ok(())
        } else {
            Err(Failure::Malformed)
        }
    }

    /// One complete object or array, and nothing after it.
    fn record(mut self) -> Parsed<Option<Value>> {
        // A byte order mark opens a record as it opens the original's text decoder.
        if self.peek()? == Some(0xEF) {
            self.bump();
            self.expect(0xBB)?;
            self.expect(0xBF)?;
        }
        self.skip_whitespace()?;
        let container = matches!(self.peek()?, Some(b'{' | b'['));
        let value = self.value(&mut Vec::new(), true)?;
        self.skip_whitespace()?;
        if self.peek()?.is_some() {
            return Err(Failure::Malformed);
        }
        Ok(if container { value } else { None })
    }

    fn value(&mut self, path: &mut Vec<Seg>, selected: bool) -> Parsed<Option<Value>> {
        self.skip_whitespace()?;
        match self.peek()? {
            Some(b'{') => self.object(path, selected),
            Some(b'[') => self.array(path, selected),
            Some(b'"') => {
                self.bump();
                if selected {
                    self.charge(TOKEN_COST)?;
                }
                let text = self.string(selected)?;
                if selected {
                    self.charge(TOKEN_COST)?;
                }
                Ok(text.map(Value::String))
            }
            Some(b'-' | b'0'..=b'9') => self.number(selected),
            Some(b't') => self.literal(b"true", Value::Bool(true), selected),
            Some(b'f') => self.literal(b"false", Value::Bool(false), selected),
            Some(b'n') => self.literal(b"null", Value::Null, selected),
            _ => Err(Failure::Malformed),
        }
    }

    fn literal(&mut self, word: &[u8], value: Value, selected: bool) -> Parsed<Option<Value>> {
        for expected in word {
            self.expect(*expected)?;
        }
        if selected {
            self.charge(TOKEN_COST)?;
        }
        Ok(selected.then_some(value))
    }

    /// Digits at the cursor, appended to `text`; how many there were.
    fn digits(&mut self, text: &mut String) -> Parsed<usize> {
        let mut count = 0;
        while let Some(byte @ b'0'..=b'9') = self.peek()? {
            text.push(byte as char);
            self.bump();
            count += 1;
        }
        Ok(count)
    }

    fn number(&mut self, selected: bool) -> Parsed<Option<Value>> {
        let mut text = String::new();
        if self.peek()? == Some(b'-') {
            text.push('-');
            self.bump();
        }
        // An integer part is `0` or digits not starting with `0`.
        match self.peek()? {
            Some(b'0') => {
                text.push('0');
                self.bump();
            }
            Some(b'1'..=b'9') => {
                self.digits(&mut text)?;
            }
            _ => return Err(Failure::Malformed),
        }
        if self.peek()? == Some(b'.') {
            text.push('.');
            self.bump();
            if self.digits(&mut text)? == 0 {
                return Err(Failure::Malformed);
            }
        }
        if matches!(self.peek()?, Some(b'e' | b'E')) {
            text.push('e');
            self.bump();
            if let Some(sign @ (b'+' | b'-')) = self.peek()? {
                text.push(sign as char);
                self.bump();
            }
            if self.digits(&mut text)? == 0 {
                return Err(Failure::Malformed);
            }
        }
        if !selected {
            return Ok(None);
        }
        self.charge(3 * TOKEN_COST + 2 * text.len())?;
        Ok(Some(serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    /// The text of a string whose opening quote is consumed. Unselected text is validated and dropped;
    /// selected text is charged as it grows, so an oversized string is refused before it is held.
    fn string(&mut self, keep: bool) -> Parsed<Option<String>> {
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let buffer = self.reader.fill_buf()?;
            if buffer.is_empty() {
                return Err(Failure::Malformed);
            }
            let stop = buffer.iter().position(|byte| *byte == b'"' || *byte == b'\\' || *byte < 0x20);
            let span = stop.unwrap_or(buffer.len());
            if keep && span > 0 {
                let units = utf16_units(&buffer[..span]);
                bytes.extend_from_slice(&buffer[..span]);
                self.reader.consume(span);
                self.charge(2 * units)?;
            } else {
                self.reader.consume(span);
            }
            let Some(_) = stop else { continue };
            match self.peek()? {
                Some(b'"') => {
                    self.bump();
                    return Ok(keep.then(|| String::from_utf8_lossy(&bytes).into_owned()));
                }
                Some(b'\\') => {
                    self.bump();
                    let (decoded, count) = self.escape()?;
                    for character in &decoded[..count] {
                        if keep {
                            let mut encoded = [0u8; 4];
                            bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                            self.charge(2 * character.len_utf16())?;
                        }
                    }
                }
                // A raw control character (a newline ends the record) is not valid in a string.
                _ => return Err(Failure::Malformed),
            }
        }
    }

    fn hex4(&mut self) -> Parsed<u32> {
        let mut value = 0u32;
        for _ in 0..4 {
            let Some(byte) = self.peek()? else { return Err(Failure::Malformed) };
            let digit = (byte as char).to_digit(16).ok_or(Failure::Malformed)?;
            value = value * 16 + digit;
            self.bump();
        }
        Ok(value)
    }
    fn simple_escape(byte: u8) -> Parsed<char> {
        Ok(match byte {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            _ => return Err(Failure::Malformed),
        })
    }
    fn next_byte(&mut self) -> Parsed<u8> {
        let byte = self.peek()?.ok_or(Failure::Malformed)?;
        self.bump();
        Ok(byte)
    }
    /// The characters an escape stands for (after its backslash). A surrogate pair is joined; a lone
    /// surrogate becomes U+FFFD, and what follows it is read as the escape it is.
    fn escape(&mut self) -> Parsed<([char; 2], usize)> {
        const LOST: char = '\u{FFFD}';
        let byte = self.next_byte()?;
        if byte != b'u' {
            return Ok(([Self::simple_escape(byte)?, LOST], 1));
        }
        let unit = self.hex4()?;
        if !(0xD800..0xDC00).contains(&unit) {
            return Ok(([char::from_u32(unit).unwrap_or(LOST), LOST], 1));
        }
        if self.peek()? != Some(b'\\') {
            return Ok(([LOST, LOST], 1));
        }
        self.bump();
        let next = self.next_byte()?;
        if next != b'u' {
            return Ok(([LOST, Self::simple_escape(next)?], 2));
        }
        let low = self.hex4()?;
        if (0xDC00..0xE000).contains(&low) {
            return Ok(([char::from_u32(0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)).unwrap_or(LOST), LOST], 1));
        }
        Ok(([LOST, char::from_u32(low).unwrap_or(LOST)], 2))
    }

    fn enter(&mut self) -> Parsed<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Failure::Limit(LimitError("Transcript JSON nesting exceeds the depth limit")));
        }
        Ok(())
    }

    fn object(&mut self, path: &mut Vec<Seg>, selected: bool) -> Parsed<Option<Value>> {
        self.bump();
        self.enter()?;
        if selected {
            self.charge(TOKEN_COST)?;
        }
        let mut members = Map::new();
        self.skip_whitespace()?;
        if self.peek()? == Some(b'}') {
            self.bump();
        } else {
            loop {
                self.skip_whitespace()?;
                self.expect(b'"')?;
                // Keys are charged whether or not their value is selected.
                let key = self.key(selected)?;
                self.skip_whitespace()?;
                self.expect(b':')?;
                let mut child_selected = false;
                if selected {
                    path.push(Seg::Key(key.clone().unwrap_or_default()));
                    child_selected = (self.select)(path);
                    if child_selected {
                        self.charge(TOKEN_COST + 2 * key.as_deref().map_or(0, |key| key.encode_utf16().count()))?;
                    }
                }
                let value = self.value(path, child_selected)?;
                if selected {
                    path.pop();
                }
                if let (Some(key), Some(value)) = (key, value) {
                    // A repeated key replaces the earlier value whole, as `JSON.parse` does.
                    members.insert(key, value);
                }
                self.skip_whitespace()?;
                match self.peek()? {
                    Some(b',') => self.bump(),
                    Some(b'}') => {
                        self.bump();
                        break;
                    }
                    _ => return Err(Failure::Malformed),
                }
            }
        }
        self.depth -= 1;
        if selected {
            self.charge(TOKEN_COST)?;
        }
        Ok(selected.then_some(Value::Object(members)))
    }

    /// An object key, whose characters are charged (twice their length) as they are read.
    fn key(&mut self, keep: bool) -> Parsed<Option<String>> {
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let buffer = self.reader.fill_buf()?;
            if buffer.is_empty() {
                return Err(Failure::Malformed);
            }
            let stop = buffer.iter().position(|byte| *byte == b'"' || *byte == b'\\' || *byte < 0x20);
            let span = stop.unwrap_or(buffer.len());
            let units = utf16_units(&buffer[..span]);
            if keep {
                bytes.extend_from_slice(&buffer[..span]);
            }
            self.reader.consume(span);
            self.charge(2 * units)?;
            let Some(_) = stop else { continue };
            match self.peek()? {
                Some(b'"') => {
                    self.bump();
                    return Ok(keep.then(|| String::from_utf8_lossy(&bytes).into_owned()));
                }
                Some(b'\\') => {
                    self.bump();
                    let (decoded, count) = self.escape()?;
                    for character in &decoded[..count] {
                        if keep {
                            let mut encoded = [0u8; 4];
                            bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                        }
                        self.charge(2 * character.len_utf16())?;
                    }
                }
                _ => return Err(Failure::Malformed),
            }
        }
    }

    fn array(&mut self, path: &mut Vec<Seg>, selected: bool) -> Parsed<Option<Value>> {
        self.bump();
        self.enter()?;
        if selected {
            self.charge(TOKEN_COST)?;
        }
        let mut elements = Vec::new();
        self.skip_whitespace()?;
        if self.peek()? == Some(b']') {
            self.bump();
        } else {
            let mut index = 0usize;
            loop {
                let mut child_selected = false;
                if selected {
                    path.push(Seg::Index(index));
                    child_selected = (self.select)(path);
                }
                let value = self.value(path, child_selected)?;
                if selected {
                    path.pop();
                }
                if let Some(value) = value {
                    elements.push(value);
                }
                index += 1;
                self.skip_whitespace()?;
                match self.peek()? {
                    Some(b',') => self.bump(),
                    Some(b']') => {
                        self.bump();
                        break;
                    }
                    _ => return Err(Failure::Malformed),
                }
            }
        }
        self.depth -= 1;
        if selected {
            self.charge(TOKEN_COST)?;
        }
        Ok(selected.then_some(Value::Array(elements)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    /// `read` in `AgentSessionJson.test.ts`: the text is fed through a reader that hands out `size` bytes at a time.
    fn read(json: &str, size: usize, select: &dyn Fn(&[Seg]) -> bool) -> Option<Value> {
        let mut records = RecordReader::new(BufReader::with_capacity(size, Cursor::new(json.as_bytes().to_vec())));
        records.next(&mut |_| Ok(()), select).ok().flatten().flatten()
    }
    fn everything(_: &[Seg]) -> bool {
        true
    }

    #[test]
    fn matches_json_parse_across_buffer_boundaries() {
        for size in [1, 2, 7, 64, 1024] {
            for text in [
                r#"{"a":1,"a":2,"b":"before","b":"after"}"#,
                r#"{"a":{"x":1},"a":{"y":2},"b":[],"c":{}}"#,
                r#"{"a":[null,true,false,1,-2.3e4,"😀a\\\"",{},[],[1,2]]}"#,
                r#"{"a":"s","a":null,"b":null,"b":"s","c":0,"c":false}"#,
                r#"{"__proto__":{"polluted":true},"constructor":1,"__proto__":2}"#,
            ] {
                assert_eq!(read(text, size, &everything), Some(serde_json::from_str::<Value>(text).unwrap()), "size {size}: {text}");
            }
        }
    }

    #[test]
    fn projects_siblings_and_array_elements_without_merging_repeated_parent_objects() {
        let text = r#"{"message":{"usage":{"input":100},"content":"large"},"message":{"usage":{"output":5},"content":[1,2]},"rows":[{"keep":1,"drop":2},{"keep":3}],"drop":{"keep":4}}"#;
        let select = |path: &[Seg]| {
            if path.first() == Some(&Seg::Key("drop".into())) {
                return false;
            }
            !path.contains(&Seg::Key("content".into())) && !path.contains(&Seg::Key("drop".into()))
        };
        assert_eq!(read(text, 1, &select), Some(serde_json::json!({"message":{"usage":{"output":5}},"rows":[{"keep":1},{"keep":3}]})));
    }

    #[test]
    fn rejects_malformed_input() {
        for text in ["{\"a\":", "{\"a\":1} trailing", "{\"a\":1}{\"a\":2}", "{\"a\":\"bad\\x\"}", "{\"a\":[1,]}"] {
            assert_eq!(read(text, 1, &everything), None, "{text}");
        }
    }

    #[test]
    fn retains_the_allocation_and_depth_limits() {
        let mut records = RecordReader::new(Cursor::new(br#"{"a":1}"#.to_vec()));
        let refused = records.next(&mut |_| Err(LimitError("budget")), &everything);
        assert!(matches!(refused, Err(ReadError::Limit(LimitError("budget")))));
        let deep = format!("{}0{}", "[".repeat(129), "]".repeat(129));
        let mut records = RecordReader::new(Cursor::new(deep.into_bytes()));
        assert!(matches!(records.next(&mut |_| Ok(()), &everything), Err(ReadError::Limit(_))));
        // 128 levels are within the limit.
        let allowed = format!("{}0{}", "[".repeat(128), "]".repeat(128));
        assert!(read(&allowed, 64, &everything).is_some());
    }

    #[test]
    fn a_newline_ends_a_record_and_the_next_one_starts_clean() {
        let mut records = RecordReader::new(Cursor::new(b"{\"a\":1}\nnot json\n\n{\"b\":[2]}".to_vec()));
        let mut seen = vec![];
        while let Some(record) = records.next(&mut |_| Ok(()), &everything).unwrap() {
            seen.push(record);
        }
        assert_eq!(seen, [Some(serde_json::json!({"a":1})), None, None, Some(serde_json::json!({"b":[2]}))]);
    }

    #[test]
    fn unselected_text_is_never_charged_but_keys_are() {
        let big = "x".repeat(1024 * 1024);
        let text = format!(r#"{{"type":"tool_result","output":"{big}"}}"#);
        let mut charged = 0usize;
        let mut records = RecordReader::new(BufReader::with_capacity(8 * 1024, Cursor::new(text.into_bytes())));
        let record = records
            .next(
                &mut |bytes| {
                    charged += bytes;
                    Ok(())
                },
                &|path: &[Seg]| TRANSCRIPT_RECORD.includes(path, 0),
            )
            .unwrap()
            .unwrap();
        assert_eq!(record, Some(serde_json::json!({"type":"tool_result"})));
        // The megabyte value was skipped; what was charged is the tokens and the key names.
        assert!(charged < 1024, "{charged}");
    }

    #[test]
    fn the_transcript_schema_selects_only_what_the_import_reads() {
        let text = r#"{"type":"response_item","timestamp":"t","extra":{"big":1},"payload":{"type":"message","role":"user","encrypted":"zzz","content":[{"type":"input_image","image_url":"data:...","text":"hi"}],"internal_chat_message_metadata_passthrough":{"turn_id":"a","other":[1]}}}"#;
        let projected = read(text, 16, &|path: &[Seg]| TRANSCRIPT_RECORD.includes(path, 0)).unwrap();
        assert_eq!(
            projected,
            serde_json::json!({"type":"response_item","timestamp":"t","payload":{"type":"message","role":"user","content":[{"type":"input_image","text":"hi"}],"internal_chat_message_metadata_passthrough":{"turn_id":"a","other":[1]}}})
        );
    }

    #[test]
    fn a_byte_order_mark_opens_a_record_and_nothing_else_does() {
        assert_eq!(read("\u{feff}{\"a\":1}", 1, &everything), Some(serde_json::json!({"a":1})));
        assert_eq!(read("\u{fffe}{\"a\":1}", 1, &everything), None);
        assert_eq!(read("{\"a\":1}\u{feff}", 1, &everything), None);
    }

    #[test]
    fn escapes_decode_as_json_parse_does() {
        let text = r#"{"a":"😀 é \ud800 x","bA":"\/"}"#;
        assert_eq!(read(text, 3, &everything), Some(serde_json::json!({"a":"😀 é \u{FFFD} x","bA":"/"})));
    }
}
