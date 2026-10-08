//! Original file-backed secret store. Call its synchronous operations from an
//! owned blocking worker when used by asynchronous settings transactions.
use rand::TryRngCore;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Secure,
    Read,
    TemporaryPath,
    Persist,
    RandomGeneration,
    ConcurrentRead,
    Remove,
    Decode,
    Encode,
}
impl Operation {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Secure => "SecretStoreSecureError",
            Self::Read => "SecretStoreReadError",
            Self::TemporaryPath => "SecretStoreTemporaryPathError",
            Self::Persist => "SecretStorePersistError",
            Self::RandomGeneration => "SecretStoreRandomGenerationError",
            Self::ConcurrentRead => "SecretStoreConcurrentReadError",
            Self::Remove => "SecretStoreRemoveError",
            Self::Decode => "SecretStoreDecodeError",
            Self::Encode => "SecretStoreEncodeError",
        }
    }
    fn verb(self) -> &'static str {
        match self {
            Self::Secure => "secure",
            Self::Read | Self::ConcurrentRead => "read",
            Self::TemporaryPath => "create temporary path for",
            Self::Persist => "persist",
            Self::RandomGeneration => "generate random bytes for",
            Self::Remove => "remove",
            Self::Decode => "decode",
            Self::Encode => "encode",
        }
    }
}
#[derive(Debug, thiserror::Error)]
#[error("Failed to {verb} {resource}{suffix}.",verb=operation.verb(),suffix=if *operation==Operation::ConcurrentRead {" after concurrent creation"}else{""})]
pub struct SecretStoreError {
    pub operation: Operation,
    pub resource: String,
    #[source]
    pub cause: Option<io::Error>,
}
impl SecretStoreError {
    fn io(operation: Operation, name: &str, cause: io::Error) -> Self {
        Self {
            operation,
            resource: format!("secret {name}"),
            cause: Some(cause),
        }
    }
    pub fn already_exists(&self) -> bool {
        self.cause
            .as_ref()
            .is_some_and(|cause| cause.kind() == io::ErrorKind::AlreadyExists)
    }
}
#[derive(Clone)]
pub struct ServerSecretStore {
    directory: Arc<PathBuf>,
}
impl ServerSecretStore {
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self, SecretStoreError> {
        let directory = directory.into();
        fs::create_dir_all(&directory)
            .and_then(|_| restrict(&directory, 0o700))
            .map_err(|cause| SecretStoreError {
                operation: Operation::Secure,
                resource: format!("secrets directory {}", directory.display()),
                cause: Some(cause),
            })?;
        Ok(Self {
            directory: Arc::new(directory),
        })
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    fn path(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{name}.bin"))
    }
    pub fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
        match fs::read(self.path(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(cause) => Err(SecretStoreError::io(Operation::Read, name, cause)),
        }
    }
    pub fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretStoreError> {
        let path = self.path(name);
        let temporary = path.with_extension(format!("bin.{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = secure_options()
                .create(true)
                .truncate(true)
                .open(&temporary)?;
            file.write_all(value)?;
            restrict(&temporary, 0o600)?;
            fs::rename(&temporary, &path)?;
            restrict(&path, 0o600)
        })();
        if let Err(cause) = result {
            let _ = fs::remove_file(&temporary);
            return Err(SecretStoreError::io(Operation::Persist, name, cause));
        }
        Ok(())
    }
    pub fn create(&self, name: &str, value: &[u8]) -> Result<(), SecretStoreError> {
        let path = self.path(name);
        (|| {
            let mut file = secure_options().create_new(true).open(&path)?;
            file.write_all(value)?;
            file.sync_all()?;
            restrict(&path, 0o600)
        })()
        .map_err(|cause| SecretStoreError::io(Operation::Persist, name, cause))
    }
    pub fn get_or_create_random(
        &self,
        name: &str,
        length: usize,
    ) -> Result<Vec<u8>, SecretStoreError> {
        if let Some(value) = self.get(name)? {
            return Ok(value);
        }
        let mut value = vec![0; length];
        rand::rngs::OsRng
            .try_fill_bytes(&mut value)
            .map_err(|cause| {
                SecretStoreError::io(Operation::RandomGeneration, name, io::Error::other(cause))
            })?;
        match self.create(name, &value) {
            Ok(()) => Ok(value),
            Err(cause) if cause.already_exists() => {
                self.get(name)?.ok_or_else(|| SecretStoreError {
                    operation: Operation::ConcurrentRead,
                    resource: format!("secret {name}"),
                    cause: None,
                })
            }
            Err(cause) => Err(cause),
        }
    }
    pub fn remove(&self, name: &str) -> Result<(), SecretStoreError> {
        match fs::remove_file(self.path(name)) {
            Ok(()) => Ok(()),
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(cause) => Err(SecretStoreError::io(Operation::Remove, name, cause)),
        }
    }
}
fn secure_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Same symlink-following policy as shared/symlink.ts, including dangling final
/// targets and a relative link inside a symlinked parent directory.
pub fn resolve_symlink_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = lexical_absolute(path)?;
    for _ in 0..40 {
        match fs::read_link(&current) {
            Ok(link) => {
                let parent = current.parent().unwrap_or(Path::new("."));
                let parent = match fs::canonicalize(parent) {
                    Ok(parent) => parent,
                    Err(cause) if cause.kind() == io::ErrorKind::NotFound => parent.to_owned(),
                    Err(cause) => return Err(cause),
                };
                current = lexical_absolute(&if link.is_absolute() {
                    link
                } else {
                    parent.join(link)
                })?;
            }
            Err(cause)
                if cause.kind() == io::ErrorKind::NotFound
                    || cause.kind() == io::ErrorKind::InvalidInput =>
            {
                return Ok(current);
            }
            Err(cause) => return Err(cause),
        }
    }
    Err(io::Error::other("Too many levels of symbolic links"))
}
fn lexical_absolute(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut output = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    Ok(output)
}
/// A sibling temporary directory preserves the symlink and admits atomic rename
/// on the destination filesystem. Cleanup failure cannot turn a landed write
/// into failure; settings owns the rollback policy for write errors.
pub fn write_string_atomically(path: &Path, contents: &str) -> io::Result<()> {
    let target = resolve_symlink_target(path)?;
    let parent = target.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let temporary = parent.join(format!("{name}.{}", uuid::Uuid::new_v4()));
    fs::create_dir(&temporary)?;
    let result = (|| {
        let content = temporary.join("contents.tmp");
        let mut file = File::create(&content)?;
        file.write_all(contents.as_bytes())?;
        drop(file);
        fs::rename(content, target)
    })();
    let _ = fs::remove_dir_all(temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn create_reuse_set_remove_and_persist_failure_keep_original_causes() {
        let root = tempfile::tempdir().unwrap();
        let store = ServerSecretStore::open(root.path().join("secrets")).unwrap();
        assert_eq!(store.get("missing").unwrap(), None);
        store.remove("missing").unwrap();
        let value = store.get_or_create_random("signing", 32).unwrap();
        assert_eq!(value.len(), 32);
        assert_eq!(store.get_or_create_random("signing", 64).unwrap(), value);
        let independent = ServerSecretStore::open(store.directory()).unwrap();
        assert_eq!(
            independent.get_or_create_random("signing", 32).unwrap(),
            value
        );
        let error = independent.create("signing", b"replacement").unwrap_err();
        assert!(error.already_exists());
        assert_eq!(error.operation, Operation::Persist);
        assert_eq!(store.get("signing").unwrap(), Some(value));
        store.set("signing", b"updated").unwrap();
        assert_eq!(store.get("signing").unwrap(), Some(b"updated".to_vec()));
        store.remove("signing").unwrap();
        assert_eq!(store.get("signing").unwrap(), None);
        fs::create_dir(store.path("blocked")).unwrap();
        let error = store.set("blocked", b"never published").unwrap_err();
        assert_eq!(error.to_string(), "Failed to persist secret blocked.");
        assert_eq!(error.operation.tag(), "SecretStorePersistError");
        assert!(error.cause.is_some());
        assert_eq!(fs::read_dir(store.directory()).unwrap().count(), 1);
        assert_eq!(store.get("blocked").unwrap_err().operation, Operation::Read);
        assert_eq!(
            store.remove("blocked").unwrap_err().operation,
            Operation::Remove
        );
    }
    #[cfg(unix)]
    #[test]
    fn directories_and_every_published_secret_keep_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("secrets");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o777)).unwrap();
        let store = ServerSecretStore::open(directory).unwrap();
        assert_eq!(
            fs::metadata(store.directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        store.create("created", b"first").unwrap();
        assert_eq!(
            fs::metadata(store.path("created"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::set_permissions(store.path("created"), fs::Permissions::from_mode(0o777)).unwrap();
        store.set("created", b"changed").unwrap();
        assert_eq!(
            fs::metadata(store.path("created"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[cfg(unix)]
    #[test]
    fn atomic_writer_preserves_symlink_chains_dangling_targets_and_rejects_cycles() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("real")).unwrap();
        symlink("real", root.path().join("parent")).unwrap();
        symlink("../target/missing.json", root.path().join("real/link")).unwrap();
        symlink("parent/link", root.path().join("settings.json")).unwrap();
        assert_eq!(
            resolve_symlink_target(&root.path().join("parent/../unlinked.json")).unwrap(),
            root.path().join("unlinked.json")
        );
        let settings = root.path().join("settings.json");
        write_string_atomically(&settings, "first\n").unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("target/missing.json")).unwrap(),
            "first\n"
        );
        assert_eq!(
            fs::read_link(&settings).unwrap(),
            PathBuf::from("parent/link")
        );
        write_string_atomically(&settings, "second\n").unwrap();
        assert_eq!(fs::read_to_string(settings).unwrap(), "second\n");
        assert_eq!(fs::read_dir(root.path().join("target")).unwrap().count(), 1);
        symlink("cycle-b", root.path().join("cycle-a")).unwrap();
        symlink("cycle-a", root.path().join("cycle-b")).unwrap();
        let error =
            write_string_atomically(&root.path().join("cycle-a"), "unreachable").unwrap_err();
        assert_eq!(error.to_string(), "Too many levels of symbolic links");
        assert!(
            fs::symlink_metadata(root.path().join("cycle-a"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
