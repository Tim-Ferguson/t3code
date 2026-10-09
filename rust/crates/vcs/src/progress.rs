//! Progress reporting for long git commands (the `progress` option of the original's
//! `execute`): output lines as git prints them, and hook start/finish events read from git's
//! trace2 event stream. Callbacks run on the task that reads the process, so they must not
//! block.
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::DateTime;
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::process::StdoutCallback;

pub type LineCallback = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookFinished {
    pub hook_name: String,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<u64>,
}

/// `ExecuteGitProgress`. Git redraws progress with a bare `\r` and only ends the line once
/// the step is done, so `\r` ends a line here as `\n` does; empty lines are not reported.
#[derive(Clone, Default)]
pub struct GitProgress {
    pub on_stdout_line: Option<LineCallback>,
    pub on_stderr_line: Option<LineCallback>,
    pub on_hook_started: Option<LineCallback>,
    pub on_hook_finished: Option<Arc<dyn Fn(&HookFinished) + Send + Sync>>,
    /// Keep reporting lines after the buffered output is full, for commands whose output is
    /// only consumed through `progress`.
    pub keep_line_callbacks_after_truncation: bool,
}

impl GitProgress {
    pub(crate) fn wants_hooks(&self) -> bool {
        self.on_hook_started.is_some() || self.on_hook_finished.is_some()
    }
}

/// A line longer than this is not one the callbacks want, and a separator-free stream must
/// not grow the buffer without bound.
const MAX_PENDING_LINE_BYTES: usize = 64 * 1024;

struct Splitter {
    callback: LineCallback,
    pending: Vec<u8>,
    seen: usize,
    cap: usize,
    keep_after_cap: bool,
}

impl Splitter {
    fn push(&mut self, chunk: &[u8]) {
        let chunk = if self.keep_after_cap {
            chunk
        } else {
            &chunk[..chunk.len().min(self.cap.saturating_sub(self.seen))]
        };
        self.seen += chunk.len();
        self.pending.extend_from_slice(chunk);
        // Lines end at the last separator; whatever follows waits for more bytes.
        let Some(end) = self
            .pending
            .iter()
            .rposition(|byte| matches!(byte, b'\n' | b'\r'))
        else {
            if self.seen > self.cap && self.pending.len() > MAX_PENDING_LINE_BYTES {
                self.pending.clear();
            }
            return;
        };
        let rest = self.pending.split_off(end + 1);
        let complete = std::mem::replace(&mut self.pending, rest);
        self.emit(&complete);
        if self.seen > self.cap && self.pending.len() > MAX_PENDING_LINE_BYTES {
            self.pending.clear();
        }
    }

    fn emit(&self, bytes: &[u8]) {
        for line in bytes.split(|byte| matches!(byte, b'\n' | b'\r')) {
            if !line.is_empty() {
                (self.callback)(&String::from_utf8_lossy(line));
            }
        }
    }

    fn finish(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        self.emit(&pending);
    }
}

/// Feeds one stream's chunks to a line callback; `finish` reports a trailing partial line.
pub(crate) struct LineStream {
    splitter: Arc<Mutex<Splitter>>,
}

impl LineStream {
    pub(crate) fn new(callback: LineCallback, cap: usize, keep_after_cap: bool) -> Self {
        Self {
            splitter: Arc::new(Mutex::new(Splitter {
                callback,
                pending: Vec::new(),
                seen: 0,
                cap,
                keep_after_cap,
            })),
        }
    }

    pub(crate) fn chunk_callback(&self) -> StdoutCallback {
        let splitter = self.splitter.clone();
        Arc::new(move |chunk| splitter.lock().unwrap().push(chunk))
    }

    pub(crate) fn finish(&self) {
        self.splitter.lock().unwrap().finish();
    }
}

// ---------------------------------------------------------------------------------------------
// trace2
// ---------------------------------------------------------------------------------------------

/// How often the trace file is read while the command runs. The final read after the command
/// exits is exact, so this only bounds how late a hook is reported.
const TRACE_POLL_INTERVAL: Duration = Duration::from_millis(50);

struct HookStart {
    hook_name: String,
    started_ms: Option<i64>,
}

struct TraceState {
    processed_bytes: usize,
    remainder: String,
    hooks: std::collections::HashMap<String, HookStart>,
    progress: GitProgress,
}

impl TraceState {
    fn read_new(&mut self, path: &PathBuf) {
        let Ok(contents) = std::fs::read(path) else {
            return;
        };
        if contents.len() <= self.processed_bytes {
            return;
        }
        let appended = String::from_utf8_lossy(&contents[self.processed_bytes..]).into_owned();
        self.processed_bytes = contents.len();
        let combined = std::mem::take(&mut self.remainder) + &appended;
        let mut lines: Vec<&str> = combined.split('\n').collect();
        self.remainder = lines.pop().unwrap_or("").to_string();
        for line in lines {
            self.handle_line(line.trim_end_matches('\r'));
        }
    }

    fn finish(&mut self, path: &PathBuf) {
        self.read_new(path);
        let last = std::mem::take(&mut self.remainder);
        self.handle_line(&last);
    }

    fn handle_line(&mut self, line: &str) {
        let line = line.trim();
        let Ok(Value::Object(record)) = serde_json::from_str::<Value>(line) else {
            return;
        };
        // git names the child's class on `child_start` only; its `child_exit` is matched to
        // the hook by `child_id`.
        let is_hook = record.get("child_class").and_then(Value::as_str) == Some("hook");
        let child_key = match record.get("child_id") {
            Some(Value::Number(number)) => Some(number.to_string()),
            Some(Value::String(text)) => Some(text.clone()),
            _ => None,
        }
        .or_else(|| {
            record
                .get("hook_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(String::from)
        });
        let Some(child_key) = child_key else {
            return;
        };
        if !is_hook && !self.hooks.contains_key(&child_key) {
            return;
        }
        let event_name = record.get("event").and_then(Value::as_str);
        let named = record
            .get("hook_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let hook_name = if named.is_empty() {
            self.hooks
                .get(&child_key)
                .map(|started| started.hook_name.clone())
                .unwrap_or_default()
        } else {
            named.to_string()
        };
        if hook_name.is_empty() {
            return;
        }
        // Git's own timestamps, so a late read does not stretch a hook's duration.
        let time_ms = record
            .get("time")
            .and_then(Value::as_str)
            .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
            .map(|time| time.timestamp_millis());
        match event_name {
            Some("child_start") => {
                self.hooks.insert(
                    child_key,
                    HookStart {
                        hook_name: hook_name.clone(),
                        started_ms: time_ms,
                    },
                );
                if let Some(callback) = &self.progress.on_hook_started {
                    callback(&hook_name);
                }
            }
            Some("child_exit") => {
                let started = self.hooks.remove(&child_key);
                // git writes the exit status as `code`.
                let exit_code = record
                    .get("code")
                    .or_else(|| record.get("exitCode"))
                    .and_then(Value::as_i64);
                let duration_ms = match (started.as_ref().and_then(|s| s.started_ms), time_ms) {
                    (Some(started), Some(finished)) => Some((finished - started).max(0) as u64),
                    _ => None,
                };
                if let Some(callback) = &self.progress.on_hook_finished {
                    callback(&HookFinished {
                        hook_name: started.map(|s| s.hook_name).unwrap_or(hook_name),
                        exit_code,
                        duration_ms,
                    });
                }
            }
            _ => {}
        }
    }
}

/// Reads git's `GIT_TRACE2_EVENT` file while a command runs and reports its hooks. Dropping
/// it stops the reader and deletes the file; `finish` must run first to report what the file
/// still holds.
pub(crate) struct Trace2Monitor {
    path: PathBuf,
    state: Arc<Mutex<TraceState>>,
    poller: JoinHandle<()>,
}

impl Trace2Monitor {
    pub(crate) fn start(progress: GitProgress) -> Self {
        let path = std::env::temp_dir().join(format!(
            "t3code-git-trace2-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let state = Arc::new(Mutex::new(TraceState {
            processed_bytes: 0,
            remainder: String::new(),
            hooks: Default::default(),
            progress,
        }));
        let poller = tokio::spawn({
            let (path, state) = (path.clone(), state.clone());
            async move {
                loop {
                    tokio::time::sleep(TRACE_POLL_INTERVAL).await;
                    state.lock().unwrap().read_new(&path);
                }
            }
        });
        Self {
            path,
            state,
            poller,
        }
    }

    pub(crate) fn path(&self) -> &PathBuf {
        &self.path
    }

    pub(crate) fn finish(&self) {
        self.state.lock().unwrap().finish(&self.path);
    }
}

impl Drop for Trace2Monitor {
    fn drop(&mut self) {
        self.poller.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}
