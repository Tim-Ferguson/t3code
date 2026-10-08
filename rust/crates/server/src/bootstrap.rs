//! Owned desktop bootstrap acquisition, before Tokio or service descriptors exist.
//! Keep the source FD-path reopening/fallback policy, first-line parsing, UTF-8
//! replacement decoding, EOF/timeout absence, and structured I/O causes.
use std::{io, time::Duration};
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("Failed to stat bootstrap file descriptor {fd}.")]
    Stat {
        fd: i32,
        #[source]
        cause: io::Error,
    },
    #[error("Failed to open bootstrap input stream for file descriptor {fd}{path} on '{platform}'.",path=fd_path.as_ref().map(|path|format!(" via '{path}'")).unwrap_or_default())]
    Open {
        fd: i32,
        platform: &'static str,
        fd_path: Option<String>,
        #[source]
        cause: io::Error,
    },
    #[error("Failed to read bootstrap envelope from file descriptor {fd}.")]
    Read {
        fd: i32,
        #[source]
        cause: io::Error,
    },
    #[error("Failed to decode bootstrap envelope from file descriptor {fd}.")]
    Decode {
        fd: i32,
        #[source]
        cause: serde_json::Error,
    },
}
pub fn fd_path(fd: i32, platform: &str) -> Option<String> {
    match platform {
        "linux" => Some(format!("/proc/self/fd/{fd}")),
        "win32" => None,
        _ => Some(format!("/dev/fd/{fd}")),
    }
}
#[cfg(unix)]
pub struct BootstrapInput {
    fd: i32,
    owned: std::os::fd::OwnedFd,
    reopened: Option<std::fs::File>,
}
#[cfg(unix)]
impl BootstrapInput {
    /// The caller has already transferred exclusive ownership of this descriptor.
    pub fn open(owned: std::os::fd::OwnedFd) -> Result<Self, BootstrapError> {
        use std::os::fd::AsRawFd;
        let fd = owned.as_raw_fd();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } < 0 {
            return Err(BootstrapError::Stat {
                fd,
                cause: io::Error::last_os_error(),
            });
        }
        let platform = if cfg!(target_os = "linux") {
            "linux"
        } else {
            "darwin"
        };
        let path = fd_path(fd, platform);
        let reopened = match path.as_ref().map(std::fs::File::open).transpose() {
            Ok(file) => file,
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENXIO | libc::EINVAL | libc::EPERM | libc::EACCES)
                ) =>
            {
                None
            }
            Err(cause) => {
                return Err(BootstrapError::Open {
                    fd,
                    platform,
                    fd_path: path,
                    cause,
                });
            }
        };
        Ok(Self {
            fd,
            owned,
            reopened,
        })
    }
    /// Availability is checked before ownership is constructed. Call only at
    /// process entry for an explicitly transferred raw descriptor, while no
    /// concurrent thread or service can close/reuse the supplied FD.
    ///
    /// # Safety
    /// A valid `fd` must be exclusively transferred by the caller. No other
    /// owner may close it after this call; unavailable numbers return None.
    pub unsafe fn acquire_transferred(fd: i32) -> Result<Option<Self>, BootstrapError> {
        use std::os::fd::FromRawFd;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } < 0 {
            let cause = io::Error::last_os_error();
            return if matches!(cause.raw_os_error(), Some(libc::EBADF | libc::ENOENT)) {
                Ok(None)
            } else {
                Err(BootstrapError::Stat { fd, cause })
            };
        }
        Self::open(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }).map(Some)
    }
    pub fn read<T: serde::de::DeserializeOwned>(
        mut self,
        timeout: Duration,
    ) -> Result<Option<T>, BootstrapError> {
        self.read_retaining(timeout)
    }
    /// Retain the explicitly owned original until the supervisor's other channel
    /// descriptors have been resolved. The temporary reopened descriptor always
    /// closes before returning, so malformed channel numbers cannot capture it.
    pub fn read_retaining<T: serde::de::DeserializeOwned>(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<T>, BootstrapError> {
        use std::os::fd::AsRawFd;
        let fd = self
            .reopened
            .as_ref()
            .map(AsRawFd::as_raw_fd)
            .unwrap_or(self.owned.as_raw_fd());
        let result = read_outcome(self.fd, read_line(fd, timeout));
        self.reopened.take();
        match result? {
            None => Ok(None),
            Some(line) => serde_json::from_str(&String::from_utf8_lossy(&line))
                .map(Some)
                .map_err(|cause| BootstrapError::Decode { fd: self.fd, cause }),
        }
    }
    pub fn into_descriptor(self) -> std::os::fd::OwnedFd {
        self.owned
    }
}
#[cfg(unix)]
fn read_line(fd: i32, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
    let deadline = std::time::Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let wait = remaining
            .as_millis()
            .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
            .min(i32::MAX as u128) as i32;
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, wait) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Ok(None);
        }
        let count = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                if std::time::Instant::now() >= deadline {
                    return Ok(None);
                }
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            return Ok((!bytes.is_empty()).then_some(bytes));
        }
        let chunk = &chunk[..count as usize];
        if let Some(end) = chunk
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
        {
            bytes.extend_from_slice(&chunk[..end]);
            return Ok(Some(bytes));
        }
        bytes.extend_from_slice(chunk);
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
    }
}

#[cfg(unix)]
fn read_outcome(
    fd: i32,
    result: io::Result<Option<Vec<u8>>>,
) -> Result<Option<Vec<u8>>, BootstrapError> {
    match result {
        Err(cause) if matches!(cause.raw_os_error(), Some(libc::EBADF | libc::ENOENT)) => Ok(None),
        Err(cause) => Err(BootstrapError::Read { fd, cause }),
        Ok(line) => Ok(line),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde::{Deserialize, Deserializer};
    use serde_json::{Value, json};
    use std::{
        io::{Read, Write},
        os::fd::{AsRawFd, OwnedFd},
        os::unix::net::UnixStream,
    };
    #[derive(Debug, PartialEq)]
    struct Envelope {
        mode: String,
    }
    impl<'de> Deserialize<'de> for Envelope {
        fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
            let value = Value::deserialize(decoder)?;
            let mode = value
                .as_object()
                .and_then(|value| value.get("mode"))
                .and_then(Value::as_str)
                .ok_or_else(|| serde::de::Error::custom("expected object with string mode"))?;
            Ok(Self {
                mode: mode.to_owned(),
            })
        }
    }
    #[test]
    fn source_reader_witnesses_cover_lines_utf8_eof_and_read_errors() {
        let directory = tempfile::tempdir().unwrap();
        for row in include_str!("../tests/fixtures/bootstrap-reader.jsonl").lines() {
            let row: Value = serde_json::from_str(row).unwrap();
            if row["op"] == "line" {
                let hex = row["input"].as_str().unwrap();
                let bytes = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                    .collect::<Vec<_>>();
                let path = directory.path().join("input");
                std::fs::write(&path, bytes).unwrap();
                let descriptor: OwnedFd = std::fs::File::open(path).unwrap().into();
                let outcome = BootstrapInput::open(descriptor)
                    .unwrap()
                    .read::<Envelope>(Duration::from_secs(1));
                if row.get("error").is_some() {
                    assert!(
                        matches!(outcome, Err(BootstrapError::Decode { .. })),
                        "{row}: {outcome:?}"
                    );
                } else {
                    assert_eq!(
                        outcome.unwrap().map(|value| json!({"mode":value.mode})),
                        row.get("result").filter(|value| !value.is_null()).cloned(),
                        "{row}"
                    );
                }
            } else {
                let code = match row["code"].as_str().unwrap() {
                    "EBADF" => libc::EBADF,
                    "ENOENT" => libc::ENOENT,
                    "EACCES" => libc::EACCES,
                    "EIO" => libc::EIO,
                    _ => unreachable!(),
                };
                let outcome = read_outcome(7, Err(io::Error::from_raw_os_error(code)));
                if row.get("error").is_some() {
                    let error = outcome.unwrap_err();
                    assert_eq!(error.to_string(), row["message"].as_str().unwrap());
                    assert!(
                        matches!(error,BootstrapError::Read{fd:7,cause} if cause.raw_os_error()==Some(code))
                    );
                } else {
                    assert!(outcome.unwrap().is_none());
                }
            }
        }
    }
    #[test]
    fn split_crlf_first_line_finishes_before_lf_arrives() {
        let (input, mut writer) = UnixStream::pair().unwrap();
        let (release, receive) = std::sync::mpsc::sync_channel(0);
        let worker = std::thread::spawn(move || {
            writer.write_all(b"{\"mode\":\"desktop\"}\r").unwrap();
            receive.recv().unwrap();
            writer.write_all(b"\nignored").ok();
        });
        let mut input = BootstrapInput::open(input.into()).unwrap();
        let value = input
            .read_retaining::<Envelope>(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(value.mode, "desktop");
        assert!(input.reopened.is_none());
        release.send(()).unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn timeout_and_decode_failure_release_owned_streams() {
        for bytes in [None, Some(b"[]\n".as_slice())] {
            let (input, mut writer) = UnixStream::pair().unwrap();
            if let Some(bytes) = bytes {
                writer.write_all(bytes).unwrap();
            }
            let outcome = BootstrapInput::open(input.into())
                .unwrap()
                .read::<Envelope>(Duration::ZERO);
            if bytes.is_some() {
                assert!(matches!(outcome, Err(BootstrapError::Decode { .. })));
            } else {
                assert!(outcome.unwrap().is_none());
            }
            let mut byte = [0];
            assert_eq!(writer.read(&mut byte).unwrap(), 0);
        }
    }
    #[test]
    fn unavailable_descriptor_does_not_construct_owner_and_errors_retain_context() {
        assert!(
            unsafe { BootstrapInput::acquire_transferred(i32::MAX) }
                .unwrap()
                .is_none()
        );
        let error = BootstrapError::Open {
            fd: 7,
            platform: "linux",
            fd_path: fd_path(7, "linux"),
            cause: io::Error::from_raw_os_error(libc::EIO),
        };
        assert_eq!(
            error.to_string(),
            "Failed to open bootstrap input stream for file descriptor 7 via '/proc/self/fd/7' on 'linux'."
        );
        assert!(std::error::Error::source(&error).unwrap().is::<io::Error>());
        assert_eq!(fd_path(0, "win32"), None);
        let (input, _writer) = UnixStream::pair().unwrap();
        let input = BootstrapInput::open(input.into()).unwrap();
        let fd = input.into_descriptor();
        assert!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } >= 0);
    }
}
