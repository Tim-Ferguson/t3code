//! Source terminalOutput.ts: bounded UTF-8 chunks with UTF-16 renderer cursors.
//! Shared chunk text keeps live updates from copying the retained transcript.
use serde::{Deserialize, Serialize};
use std::sync::Arc;
pub const DEFAULT_MAX_BUFFER_BYTES: i64 = 512 * 1024;
pub const DEFAULT_CHUNK_BYTES: usize = 16 * 1024;
pub const MAX_OUTPUT_CHUNKS: usize = 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputChunk {
    pub start_offset: i64,
    #[serde(serialize_with = "write_text", deserialize_with = "read_text")]
    pub data: Arc<str>,
    pub byte_length: usize,
}
fn write_text<S: serde::Serializer>(data: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(data)
}
fn read_text<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Arc<str>, D::Error> {
    String::deserialize(deserializer).map(Arc::from)
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputState {
    pub generation: i64,
    pub chunks: Vec<OutputChunk>,
    pub retained_bytes: usize,
    pub reset_version: i64,
    pub next_offset: i64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputCursor {
    pub generation: i64,
    pub reset_version: i64,
    pub offset: i64,
}
pub const INITIAL_CURSOR: OutputCursor = OutputCursor {
    generation: -1,
    reset_version: -1,
    offset: 0,
};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OutputUpdate {
    None { cursor: OutputCursor },
    Reset { data: String, cursor: OutputCursor },
    Append { data: String, cursor: OutputCursor },
}
impl OutputUpdate {
    pub fn cursor(&self) -> OutputCursor {
        match self {
            Self::None { cursor } | Self::Reset { cursor, .. } | Self::Append { cursor, .. } => {
                *cursor
            }
        }
    }
}
fn utf16_len(data: &str) -> i64 {
    data.encode_utf16().count() as i64
}
fn chunks(data: &str, first_offset: i64, max_bytes: usize) -> (Vec<OutputChunk>, i64) {
    let mut result = vec![];
    let mut start = 0;
    let mut offset = first_offset;
    while start < data.len() {
        let mut end = (start + max_bytes).min(data.len());
        while !data.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = (start + max_bytes).min(data.len());
            while !data.is_char_boundary(end) {
                end += 1;
            }
        }
        let text = &data[start..end];
        result.push(OutputChunk {
            start_offset: offset,
            data: Arc::from(text),
            byte_length: text.len(),
        });
        offset += utf16_len(text);
        start = end;
    }
    (result, offset)
}
fn chunk_budget(max_bytes: i64) -> usize {
    max_bytes.clamp(1, DEFAULT_CHUNK_BYTES as i64) as usize
}
pub fn text(output: &OutputState) -> String {
    output
        .chunks
        .iter()
        .map(|chunk| chunk.data.as_ref())
        .collect()
}
pub fn reset(current: &OutputState, data: &str, max_bytes: i64) -> OutputState {
    let retained = if max_bytes <= 0 {
        ""
    } else {
        let mut start = data.len().saturating_sub(max_bytes as usize);
        while !data.is_char_boundary(start) {
            start += 1;
        }
        &data[start..]
    };
    let (chunks, next_offset) = chunks(retained, 0, chunk_budget(max_bytes));
    OutputState {
        generation: current.generation,
        chunks,
        retained_bytes: retained.len(),
        reset_version: current.reset_version + 1,
        next_offset,
    }
}
pub fn append(current: &OutputState, data: &str, max_bytes: i64) -> OutputState {
    if data.is_empty() {
        return current.clone();
    }
    if max_bytes <= 0 {
        return OutputState {
            generation: current.generation,
            chunks: vec![],
            retained_bytes: 0,
            reset_version: current.reset_version + 1,
            next_offset: current.next_offset + utf16_len(data),
        };
    }
    let (mut appended, next_offset) = chunks(data, current.next_offset, chunk_budget(max_bytes));
    let mut result = current.chunks.clone();
    result.append(&mut appended);
    let mut retained_bytes = current.retained_bytes + data.len();
    let mut first = 0;
    while retained_bytes > max_bytes as usize && first < result.len() {
        let chunk = &mut result[first];
        let drop_bytes = retained_bytes - max_bytes as usize;
        if drop_bytes < chunk.byte_length {
            let mut cut = drop_bytes;
            while !chunk.data.is_char_boundary(cut) {
                cut += 1;
            }
            chunk.start_offset += utf16_len(&chunk.data[..cut]);
            chunk.data = Arc::from(&chunk.data[cut..]);
            chunk.byte_length -= cut;
            retained_bytes -= cut;
            if chunk.byte_length == 0 {
                first += 1;
            }
            break;
        }
        retained_bytes -= chunk.byte_length;
        first += 1;
    }
    if first > 0 {
        result.drain(..first);
    }
    if result.len() > MAX_OUTPUT_CHUNKS {
        let mut compacted: Vec<OutputChunk> = vec![];
        for chunk in result {
            if let Some(previous) = compacted.last_mut().filter(|previous| {
                previous.start_offset + utf16_len(&previous.data) == chunk.start_offset
                    && previous.byte_length + chunk.byte_length <= DEFAULT_CHUNK_BYTES
            }) {
                previous.data = Arc::from(format!("{}{}", previous.data, chunk.data));
                previous.byte_length += chunk.byte_length;
            } else {
                compacted.push(chunk);
            }
        }
        let excess = compacted.len().saturating_sub(MAX_OUTPUT_CHUNKS);
        for chunk in compacted.drain(..excess) {
            retained_bytes -= chunk.byte_length;
        }
        result = compacted;
    }
    OutputState {
        generation: current.generation,
        chunks: result,
        retained_bytes,
        reset_version: current.reset_version,
        next_offset,
    }
}
/// Cursors returned by this module always fall on code-point boundaries.
/// UTF-16 offsets are deliberately distinct from byte offsets for astral text.
fn suffix(data: &str, units: i64) -> &str {
    if units <= 0 {
        return data;
    }
    let mut offset = 0;
    for (index, c) in data.char_indices() {
        if offset >= units {
            return &data[index..];
        }
        offset += c.len_utf16() as i64;
    }
    ""
}
pub fn read(output: &OutputState, cursor: OutputCursor) -> OutputUpdate {
    let next = OutputCursor {
        generation: output.generation,
        reset_version: output.reset_version,
        offset: output.next_offset,
    };
    let first = output
        .chunks
        .first()
        .map(|chunk| chunk.start_offset)
        .unwrap_or(output.next_offset);
    if cursor.generation != output.generation
        || cursor.reset_version != output.reset_version
        || cursor.offset < first
    {
        return OutputUpdate::Reset {
            data: text(output),
            cursor: next,
        };
    }
    let mut data = String::new();
    let mut appended = false;
    for chunk in &output.chunks {
        if chunk.start_offset + utf16_len(&chunk.data) > cursor.offset {
            data.push_str(suffix(&chunk.data, cursor.offset - chunk.start_offset));
            appended = true;
        }
    }
    if appended {
        OutputUpdate::Append { data, cursor: next }
    } else {
        OutputUpdate::None { cursor: next }
    }
}
