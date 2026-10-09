//! Temporary indexes for commit and review previews: a private copy of the repository's index
//! that git can stage into without touching the real one.
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

/// A file or directory in the system temp directory, removed on drop.
pub(crate) struct TempPath(PathBuf);

impl TempPath {
    fn unique(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}{}", uuid::Uuid::new_v4()))
    }

    pub(crate) fn directory(prefix: &str) -> std::io::Result<Self> {
        let path = Self::unique(prefix);
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    /// A path for a file git (or a copy) will create.
    pub(crate) fn file(prefix: &str) -> Self {
        Self(Self::unique(prefix))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        if self.0.is_dir() {
            let _ = fs::remove_dir_all(&self.0);
        } else {
            let _ = fs::remove_file(&self.0);
        }
    }
}

/// Copies an index and gives the copy the original's modification time truncated to whole
/// seconds. Git treats entries newer than the index file as racily clean and re-reads them;
/// truncating keeps files modified in the preceding second from looking racy.
pub(crate) fn copy_index_with_whole_second_mtime(from: &Path, to: &Path) -> std::io::Result<()> {
    let seconds = fs::metadata(from)?
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_secs());
    fs::copy(from, to)?;
    let time = UNIX_EPOCH + Duration::from_secs(seconds);
    fs::OpenOptions::new()
        .write(true)
        .open(to)?
        .set_times(fs::FileTimes::new().set_accessed(time).set_modified(time))
}
