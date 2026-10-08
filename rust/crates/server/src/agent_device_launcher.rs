//! Native guard for the externally installed agent-device CLI.
use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const MISSING_TARGET: &str =
    "Call device_open first and include its --config and --session flags.";

pub fn permits(args: &[OsString]) -> bool {
    if args.len() == 1
        && ["help", "--help", "-h", "--version", "version"]
            .iter()
            .any(|value| args[0] == *value)
    {
        return true;
    }
    ["--config", "--session"].iter().all(|flag| {
        args.iter()
            .position(|value| value == *flag)
            .and_then(|index| args.get(index + 1))
            .is_some_and(|value| !value.is_empty() && !value.to_string_lossy().starts_with("--"))
    })
}

pub fn run(node: &Path, entry: &Path, args: &[OsString]) -> io::Result<i32> {
    if !permits(args) {
        eprintln!("{MISSING_TARGET}");
        return Ok(1);
    }
    let status = Command::new(node)
        .arg(entry)
        .args(args)
        .env_remove("AGENT_DEVICE_DAEMON_BASE_URL")
        .env_remove("AGENT_DEVICE_DAEMON_AUTH_TOKEN")
        .env_remove("AGENT_DEVICE_CONFIG")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    Ok(status.code().unwrap_or(1))
}

/// Invoked before normal server options, inherited descriptors, or runtime startup.
pub fn dispatch(args: impl IntoIterator<Item = OsString>) -> Option<io::Result<i32>> {
    let mut args = args.into_iter();
    if args.next()?.to_str() != Some("__agent-device") {
        return None;
    }
    Some((|| {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid native agent-device launcher arguments",
            )
        };
        if args.next().as_deref() != Some(std::ffi::OsStr::new("--node")) {
            return Err(invalid());
        }
        let node = PathBuf::from(args.next().ok_or_else(invalid)?);
        if args.next().as_deref() != Some(std::ffi::OsStr::new("--entry")) {
            return Err(invalid());
        }
        let entry = PathBuf::from(args.next().ok_or_else(invalid)?);
        if args.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
            return Err(invalid());
        }
        run(&node, &entry, &args.collect::<Vec<_>>())
    })())
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn ensure(
    state_dir: &Path,
    executable: &Path,
    node: &Path,
    entry: &Path,
) -> io::Result<PathBuf> {
    let directory = state_dir.join("device/bin");
    fs::create_dir_all(&directory)?;
    #[cfg(windows)]
    let (name, script) = {
        // cmd.exe expands percent signs even inside quotes.
        let quote = |path: &Path| -> io::Result<String> {
            let value = path.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Launcher path is not Unicode")
            })?;
            if value.contains(['"', '\r', '\n']) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Invalid launcher path",
                ));
            }
            Ok(format!("\"{}\"", value.replace('%', "%%")))
        };
        (
            "agent-device.cmd",
            format!(
                "@echo off\r\n{} __agent-device --node {} --entry {} -- %*\r\n",
                quote(executable)?,
                quote(node)?,
                quote(entry)?
            ),
        )
    };
    #[cfg(not(windows))]
    let (name, script) = {
        let quote = |path: &Path| -> io::Result<String> {
            path.to_str().map(shell_quote).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Launcher path is not Unicode")
            })
        };
        (
            "agent-device",
            format!(
                "#!/bin/sh\nexec {} __agent-device --node {} --entry {} -- \"$@\"\n",
                quote(executable)?,
                quote(node)?,
                quote(entry)?
            ),
        )
    };
    let file = directory.join(name);
    if fs::read(&file).ok().as_deref() == Some(script.as_bytes()) {
        return Ok(file);
    }
    // Publish only a complete executable. Simultaneous device_open calls can
    // regenerate this shared path while another caller executes it.
    let temporary = directory.join(format!(".launcher-{}", uuid::Uuid::new_v4()));
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let remove = Remove(temporary.clone());
    fs::write(&temporary, &script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))?;
    }
    if let Err(error) = fs::rename(&temporary, &file) {
        // A concurrent publisher may already have installed the same wrapper.
        if fs::read(&file).ok().as_deref() != Some(script.as_bytes()) {
            return Err(error);
        }
    }
    drop(remove);
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_first_flag_and_informational_admission() {
        let cases = [
            (vec!["help"], true),
            (vec!["version"], true),
            (vec!["help", "--foo"], false),
            (vec![], false),
            (vec!["open", "--config", "", "--session", "x"], false),
            (vec!["open", "--config", "--bad", "--session", "x"], false),
            (vec!["open", "--config", "-x", "--session", "y"], true),
            (
                vec!["--config", "", "--config", "valid", "--session", "y"],
                false,
            ),
        ];
        for (args, expected) in cases {
            assert_eq!(
                permits(&args.into_iter().map(OsString::from).collect::<Vec<_>>()),
                expected
            );
        }
    }
}
