//! Isolated terminal histories, including the original default-session migration.
use crate::terminal_history::BoundedTerminalHistory;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Debug, Clone, thiserror::Error)]
#[error("Terminal operation failed: {0:?}")]
pub struct TerminalFailure(pub t3_contracts::TerminalError);
impl TerminalFailure {
    pub(crate) fn from_value(value: Value) -> Self {
        Self(serde_json::from_value(value).expect("source-defined native terminal error"))
    }
    pub fn wire(&self) -> Value {
        serde_json::to_value(&self.0).expect("typed terminal error serializes")
    }
}
pub(crate) fn io_cause(error: io::Error) -> Value {
    json!({"name":"Error","message":error.to_string(),"errno":error.raw_os_error(),"code":match error.kind() {io::ErrorKind::NotFound=>"ENOENT",io::ErrorKind::PermissionDenied=>"EACCES",io::ErrorKind::NotADirectory=>"ENOTDIR",io::ErrorKind::IsADirectory=>"EISDIR",_=>"EIO"}})
}
#[derive(Clone)]
pub struct TerminalHistoryStore {
    directory: PathBuf,
    lines: usize,
    bytes: usize,
    #[cfg(test)]
    gate: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<PersistGate>>>>,
}
#[cfg(test)]
pub(crate) struct PersistGate {
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
    history: Option<String>,
}
impl TerminalHistoryStore {
    pub async fn new(directory: PathBuf, lines: usize, bytes: usize) -> io::Result<Self> {
        tokio::fs::create_dir_all(&directory).await?;
        Ok(Self {
            directory,
            lines,
            bytes,
            #[cfg(test)]
            gate: Default::default(),
        })
    }
    fn thread_name(thread: &str) -> String {
        format!("terminal_{}", URL_SAFE_NO_PAD.encode(thread))
    }
    pub fn path(&self, thread: &str, terminal: &str) -> PathBuf {
        let name = Self::thread_name(thread);
        self.directory
            .join(if terminal == t3_contracts::DEFAULT_TERMINAL_ID {
                format!("{name}.log")
            } else {
                format!("{name}_{}.log", URL_SAFE_NO_PAD.encode(terminal))
            })
    }
    fn legacy(&self, thread: &str) -> PathBuf {
        let name: String = thread
            .encode_utf16()
            .map(|unit| {
                if (unit <= 127 && (unit as u8).is_ascii_alphanumeric())
                    || matches!(unit, 45 | 46 | 95)
                {
                    char::from_u32(unit.into()).unwrap()
                } else {
                    '_'
                }
            })
            .collect();
        self.directory.join(format!("{name}.log"))
    }
    fn failure(thread: &str, terminal: &str, operation: &str, error: io::Error) -> TerminalFailure {
        TerminalFailure::from_value(
            json!({"_tag":"TerminalHistoryError","operation":operation,"threadId":thread,"terminalId":terminal,"cause":io_cause(error)}),
        )
    }
    async fn tail(&self, path: &Path) -> io::Result<(String, bool)> {
        let mut file = tokio::fs::File::open(path).await?;
        let size = file.metadata().await?.len();
        let offset = size.saturating_sub(self.bytes as u64);
        file.seek(io::SeekFrom::Start(offset)).await?;
        let mut bytes = Vec::with_capacity((size - offset) as usize);
        file.take(size - offset).read_to_end(&mut bytes).await?;
        let mut start = 0;
        if offset > 0 {
            while bytes.get(start).is_some_and(|byte| byte & 0xc0 == 0x80) {
                start += 1;
            }
        }
        // TextDecoder(ignoreBOM:true) preserves a literal BOM in stored history.
        Ok((
            String::from_utf8_lossy(&bytes[start..]).into_owned(),
            offset > 0,
        ))
    }
    pub async fn read(
        &self,
        thread: &str,
        terminal: &str,
    ) -> Result<BoundedTerminalHistory, TerminalFailure> {
        let path = self.path(thread, terminal);
        if tokio::fs::try_exists(&path)
            .await
            .map_err(|error| Self::failure(thread, terminal, "read", error))?
        {
            let (raw, truncated) = self
                .tail(&path)
                .await
                .map_err(|error| Self::failure(thread, terminal, "read", error))?;
            let mut history = BoundedTerminalHistory::new(self.lines, &raw, self.bytes);
            let capped = history.value();
            if truncated || capped != raw {
                tokio::fs::write(&path, capped)
                    .await
                    .map_err(|error| Self::failure(thread, terminal, "truncate", error))?;
            }
            return Ok(history);
        }
        if terminal == t3_contracts::DEFAULT_TERMINAL_ID {
            let legacy = self.legacy(thread);
            if tokio::fs::try_exists(&legacy)
                .await
                .map_err(|error| Self::failure(thread, terminal, "migrate", error))?
            {
                let (raw, _) = self
                    .tail(&legacy)
                    .await
                    .map_err(|error| Self::failure(thread, terminal, "migrate", error))?;
                let mut history = BoundedTerminalHistory::new(self.lines, &raw, self.bytes);
                tokio::fs::write(&path, history.value())
                    .await
                    .map_err(|error| Self::failure(thread, terminal, "migrate", error))?;
                if let Err(error) = tokio::fs::remove_file(&legacy).await {
                    tracing::warn!(%thread,%terminal,%error,"failed to remove legacy terminal history");
                }
                return Ok(history);
            }
        }
        Ok(BoundedTerminalHistory::new(self.lines, "", self.bytes))
    }
    /// Original live persistence warns on write failures; read/truncate/migrate
    /// are typed failures because returning incomplete restored history is unsafe.
    pub async fn persist(&self, thread: &str, terminal: &str, history: String) {
        #[cfg(test)]
        {
            let gate = {
                let mut slot = self.gate.lock().unwrap();
                if slot.as_ref().is_some_and(|gate| {
                    gate.history
                        .as_ref()
                        .is_none_or(|expected| expected == &history)
                }) {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
        }
        if let Err(error) = tokio::fs::write(self.path(thread, terminal), history).await {
            tracing::warn!(%thread,%terminal,%error,"failed to persist terminal history");
        }
    }
    #[cfg(test)]
    pub(crate) fn block_next_persist(&self, history: Option<&str>) -> std::sync::Arc<PersistGate> {
        let gate = std::sync::Arc::new(PersistGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            history: history.map(str::to_owned),
        });
        *self.gate.lock().unwrap() = Some(gate.clone());
        gate
    }
    pub async fn delete(&self, thread: &str, terminal: &str) {
        self.remove(self.path(thread, terminal)).await;
        if terminal == t3_contracts::DEFAULT_TERMINAL_ID {
            self.remove(self.legacy(thread)).await;
        }
    }
    async fn remove(&self, path: PathBuf) {
        if let Err(error) = tokio::fs::remove_file(&path).await {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(?path,%error,"failed to delete terminal history");
            }
        }
    }
    pub async fn delete_thread(&self, thread: &str) {
        let name = Self::thread_name(thread);
        let prefix = format!("{name}_");
        let legacy = self.legacy(thread);
        if let Ok(mut directory) = tokio::fs::read_dir(&self.directory).await {
            while let Ok(Some(entry)) = directory.next_entry().await {
                let file = entry.file_name().to_string_lossy().into_owned();
                if file == format!("{name}.log")
                    || entry.path() == legacy
                    || file.starts_with(&prefix)
                {
                    self.remove(entry.path()).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn legacy_default_history_migrates_once_without_colliding_encoded_thread_ids() {
        let directory = tempfile::tempdir().unwrap();
        let store = TerminalHistoryStore::new(directory.path().into(), 5000, 1024)
            .await
            .unwrap();
        let thread = "thread/前😀";
        tokio::fs::write(store.legacy(thread), "old\n\u{feff}前😀\n")
            .await
            .unwrap();
        assert!(
            store
                .legacy(thread)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("___")
        );
        let mut other = store.read(thread, "term-2").await.unwrap();
        assert_eq!(other.value(), "");
        let mut migrated = store.read(thread, "term-1").await.unwrap();
        assert_eq!(migrated.value(), "old\n\u{feff}前😀\n");
        assert!(!store.legacy(thread).exists());
        assert_ne!(
            store.path("thread/a", "term-1"),
            store.path("thread_a", "term-1")
        );
        let mut reopened = store.read(thread, "term-1").await.unwrap();
        assert_eq!(reopened.value(), migrated.value());
    }
    #[tokio::test]
    async fn tail_read_skips_partial_utf8_caps_lines_and_rewrites_truncated_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = TerminalHistoryStore::new(directory.path().into(), 1, 8)
            .await
            .unwrap();
        tokio::fs::write(store.path("thread", "term-1"), "old\n前😀\ntail")
            .await
            .unwrap();
        let mut history = store.read("thread", "term-1").await.unwrap();
        assert_eq!(history.value(), "tail");
        assert_eq!(
            tokio::fs::read_to_string(store.path("thread", "term-1"))
                .await
                .unwrap(),
            history.value()
        );
    }
}
