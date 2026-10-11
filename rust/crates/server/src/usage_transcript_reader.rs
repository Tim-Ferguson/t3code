//! Raw filesystem access for transcript scanning (`usageTranscriptReader.ts`).
//!
//! Transcripts are append-only, so a parse also reports the byte position it stopped at; a later scan of the same file resumes
//! from that position and parses only the appended bytes, which keeps a warm scan cheap while a session is writing a rollout.
use crate::usage_transcripts::{CodexScanState, UsageRecord, might_carry_usage, parse_claude_line, parse_codex_line};
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// 64 bytes of JSONL tail is ample to distinguish a replaced file.
pub const GUARD_LENGTH: u64 = 64;

/// A transcript found by the walk; `mtime_ms` is the file's modification time as a fraction of milliseconds, as Node's `mtimeMs`.
pub struct TranscriptFile {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ms: f64,
}

/// Where a parse stopped, with enough state to continue from there. The guard hash fingerprints the bytes immediately before
/// `resume_offset`: a resume only proceeds when they still match, so a rotated or rewritten file is never parsed from its middle.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsePosition {
    /// Byte offset just past the last newline-terminated line consumed.
    pub resume_offset: u64,
    pub guard_length: u64,
    /// FNV-1a hash of the guard window.
    pub guard_hash: u32,
    /// The Codex reducer as of `resume_offset`; `None` for stateless providers.
    pub codex_state: Option<CodexScanState>,
}

pub struct ParseResult {
    /// Records from newline-terminated lines at or after the parse start.
    pub records: Vec<UsageRecord>,
    /// Records from a trailing segment the writer has not newline-terminated yet, which `position` excludes: the next scan re-reads it.
    pub tail_records: Vec<UsageRecord>,
    pub position: ParsePosition,
    /// Whether the parse continued from the position rather than byte 0.
    pub resumed: bool,
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Node's `mtimeMs`: seconds times 1000 plus the nanoseconds as milliseconds, in doubles.
pub fn mtime_ms(metadata: &std::fs::Metadata) -> f64 {
    match metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()) {
        Some(elapsed) => elapsed.as_secs() as f64 * 1e3 + f64::from(elapsed.subsec_nanos()) / 1e6,
        None => 0.0,
    }
}

/// `listTranscriptFiles`: every `.jsonl` (or only `file_name`) under `root` last modified at or after `since_ms`, depth-first in name
/// order; the aggregator's first-seen dedupe relies on that order. Entries that vanish or cannot be read are skipped.
pub fn list_transcript_files(root: &Path, since_ms: f64, file_name: Option<&str>) -> Vec<TranscriptFile> {
    let mut found = vec![];
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else { continue };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|entry| entry.file_name());
        let mut batch = vec![];
        for entry in entries {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                batch.push((true, path));
            } else if match file_name {
                Some(name) => path.file_name().is_some_and(|candidate| candidate == name),
                None => path.extension().is_some_and(|extension| extension == "jsonl"),
            } {
                batch.push((false, path));
            }
        }
        // Subdirectories are visited where they sort.
        for (is_dir, path) in batch.into_iter().rev() {
            if is_dir {
                pending.push(path);
            } else if let Ok(metadata) = std::fs::metadata(&path) {
                let mtime = mtime_ms(&metadata);
                if mtime >= since_ms {
                    found.push(TranscriptFile { path, size: metadata.len(), mtime_ms: mtime });
                }
            }
        }
    }
    found
}

/// `readDirectoryVolumeId`: the directory's `device:inode`, which tells two servers reading one directory apart from two machines
/// whose hostname and home path happen to match. Empty when it cannot be read.
#[cfg(unix)]
pub fn directory_volume_id(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map(|metadata| format!("{}:{}", metadata.dev(), metadata.ino())).unwrap_or_default()
}
#[cfg(not(unix))]
pub fn directory_volume_id(_path: &Path) -> String {
    String::new()
}

fn guard_matches(file: &mut std::fs::File, position: &ParsePosition) -> bool {
    if position.guard_length == 0 || position.guard_length > GUARD_LENGTH || position.guard_length > position.resume_offset {
        return false;
    }
    let mut window = vec![0u8; position.guard_length as usize];
    file.seek(SeekFrom::Start(position.resume_offset - position.guard_length)).and_then(|_| file.read_exact(&mut window)).is_ok() && fnv1a(&window) == position.guard_hash
}

/// One line of a transcript, or nothing when it cannot carry usage. The Codex reducer also needs the lines that carry its session,
/// model and service tier.
fn parse_line(line: &[u8], provider: &str, state: &mut CodexScanState, out: &mut Vec<UsageRecord>) {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let text = String::from_utf8_lossy(line);
    let relevant = if provider == "codex" {
        might_carry_usage(&text, provider) || text.contains("\"turn_context\"") || text.contains("\"thread_settings_applied\"") || text.contains("\"session_meta\"")
    } else {
        might_carry_usage(&text, provider)
    };
    if !relevant {
        return;
    }
    out.extend(if provider == "codex" { parse_codex_line(&text, state) } else { parse_claude_line(&text) });
}

/// `readTranscriptRecords`: the records of one transcript, or `None` when the file could not be read. The difference matters to the
/// cache: an empty transcript is a stable fact worth keeping, a transient failure kept under the same `(size, mtime)` would drop the
/// file's usage until it changes. With `resume_from`, parsing continues from that position when its guard bytes still match.
pub fn read_transcript_records(path: &Path, provider: &str, resume_from: Option<&ParsePosition>) -> Option<ParseResult> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut state = CodexScanState::default();
    let (mut start, mut resumed) = (0u64, false);
    if let Some(from) = resume_from {
        if from.resume_offset > 0 && (provider != "codex" || from.codex_state.is_some()) && guard_matches(&mut file, from) {
            if let Some(saved) = &from.codex_state {
                state = saved.clone();
            }
            start = from.resume_offset;
            resumed = true;
        }
    }
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
    let mut records = vec![];
    let mut resume_offset = start;
    let mut line = Vec::new();
    let mut tail = None;
    loop {
        line.clear();
        let read = std::io::BufRead::read_until(&mut reader, b'\n', &mut line).ok()?;
        if read == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            parse_line(&line, provider, &mut state, &mut records);
            resume_offset += read as u64;
        } else {
            tail = Some(std::mem::take(&mut line));
        }
    }
    // The unterminated tail is parsed on a copy of the reducer, so it never moves the position the next scan resumes from.
    let mut tail_records = vec![];
    if let Some(tail) = tail {
        parse_line(&tail, provider, &mut state.clone(), &mut tail_records);
    }
    let mut file = reader.into_inner();
    let guard_length = GUARD_LENGTH.min(resume_offset);
    let mut guard_hash = 0;
    if guard_length > 0 {
        let mut window = vec![0u8; guard_length as usize];
        file.seek(SeekFrom::Start(resume_offset - guard_length)).and_then(|_| file.read_exact(&mut window)).ok()?;
        guard_hash = fnv1a(&window);
    }
    Some(ParseResult { records, tail_records, position: ParsePosition { resume_offset, guard_length, guard_hash, codex_state: (provider == "codex").then_some(state) }, resumed })
}
