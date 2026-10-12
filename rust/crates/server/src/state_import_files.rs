//! The files the original keeps beside its database, for an import into a state directory other than the original's.
//!
//! Where the destination *is* the original's `userdata` directory nothing is copied: this server already reads the same
//! `settings.json`, `keybindings.json`, `attachments/` and `secrets/` there, in the same formats (its settings, keybinding,
//! attachment and secret stores are ports of the original's, and `server_settings_secrets` names secrets the same way). A copy
//! never replaces a file the destination has, and never touches the source.
use crate::state_import::{FileReport, ImportError, Report};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Secrets the settings and source-control services read by name (`server_settings_secrets`): provider environment variables,
/// usage-limit source keys, GitHub tokens, the two Bitbucket tokens, and the ChatGPT sign-in of managed Codex (`provider-auth-<hash>`
/// holds both a connection's tokens and the client registration, `codex_chatgpt_auth`).
const SETTINGS_SECRET_PREFIXES: [&str; 6] = ["provider-env-", "usage-limit-source-", "github-token-", "bitbucket-access-token", "bitbucket-api-token", "provider-auth-"];

/// The usage scan cache (`usage_scan_cache`) and the legacy one it falls back to. Both are the original's own files and documents.
const USAGE_SCAN_CACHES: [&str; 2] = ["usage-scan-cache-v5.json", "usage-scan-cache.json"];

fn note(report: &mut Report, name: &str, action: &str, count: u64) {
    report.files.push(FileReport { name: name.to_owned(), action: action.to_owned(), count });
}

/// `link`: attachments never change once written, so a hard link (same device) costs no space; everything else is copied, because
/// this server may rewrite it in place and the original's file must not change with it.
fn copy_file(from: &Path, to: &Path, secret: bool, link: bool) -> std::io::Result<bool> {
    if to.exists() {
        return Ok(false);
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temporary = to.as_os_str().to_os_string();
    temporary.push(".importing");
    let temporary = PathBuf::from(temporary);
    if !link || fs::hard_link(from, &temporary).is_err() {
        fs::copy(from, &temporary)?;
    }
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&temporary, to)?;
    Ok(true)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<u64> {
    let mut copied = 0;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copied += copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() && copy_file(&entry.path(), &target, false, true)? {
            copied += 1;
        }
    }
    Ok(copied)
}

/// The environment's identity (`<userdata>/environment-id`, a UUID the original made once). Saved connections, drafts and the client
/// state keyed by an environment, the pairing and relay links that name it, and the check a client makes that it reached the
/// server it meant (`expectedEnvironmentId`) all hold it, so an import that kept this server's own would make every one of them
/// a different environment. This server reads its identity from `rust-environment-id`; the import writes the original's there.
///
/// A store of this server may already have clients paired to the identity it has; then it is kept (a store is only imported into
/// before it exists, so this is a guard, not a path an import takes).
pub fn adopt_environment_identity(userdata: &Path, state_dir: &Path, report: &mut Report) -> Result<(), ImportError> {
    let Ok(text) = fs::read_to_string(userdata.join("environment-id")) else { return Ok(()) };
    let original = text.trim().to_owned();
    if uuid::Uuid::parse_str(&original).is_err() {
        report.warnings.push("environment-id is not a UUID, which this server requires of an identity; the server makes its own, and clients saved against the original's must be added again".into());
        note(report, "environment-id", "not adopted (not a UUID)", 0);
        return Ok(());
    }
    let target = state_dir.join("rust-environment-id");
    let existing = fs::read_to_string(&target).ok().map(|text| text.trim().to_owned()).filter(|text| !text.is_empty());
    if existing.as_deref() == Some(original.as_str()) {
        note(report, "environment-id", "already this server's identity", 0);
    } else if existing.is_some() && state_dir.join("rust-state.sqlite").exists() {
        report.warnings.push(format!("this server already has the identity {} with a store that devices may be paired to, so it was kept; clients saved against the original's identity ({original}) must be added again", existing.unwrap_or_default()));
        note(report, "environment-id", "not adopted (this server's identity has clients)", 0);
    } else {
        let mut temporary = target.as_os_str().to_os_string();
        temporary.push(".importing");
        let temporary = PathBuf::from(temporary);
        fs::write(&temporary, format!("{original}\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&temporary, &target)?;
        note(report, "environment-id", if existing.is_some() { "adopted, replacing an identity no client was paired to" } else { "adopted" }, 1);
    }
    report.environment_id = Some(original);
    Ok(())
}

/// Cleanup rules in the settings that arrive are held (`cleanup_hold`); the report says so.
fn hold_cleanup(state_dir: &Path, report: &mut Report) {
    if let Some(line) = crate::cleanup_hold::on_import(state_dir) {
        report.credentials_to_reenter.push(line);
    }
}

pub fn copy(userdata: &Path, state_dir: &Path, report: &mut Report) -> Result<(), ImportError> {
    if fs::canonicalize(userdata).ok().as_deref() == Some(state_dir) {
        note(report, "settings.json, keybindings.json, attachments/, secrets/", "used in place (same directory)", 0);
        hold_cleanup(state_dir, report);
        return Ok(());
    }
    for name in ["settings.json", "keybindings.json"] {
        let from = userdata.join(name);
        if from.is_file() {
            let copied = copy_file(&from, &state_dir.join(name), false, false)?;
            let action = if copied { "copied" } else { "kept the destination's own" };
            note(report, name, action, 1);
            if copied && name == "settings.json" {
                hold_cleanup(state_dir, report);
            }
        }
    }
    let attachments = userdata.join("attachments");
    if attachments.is_dir() {
        let copied = copy_tree(&attachments, &state_dir.join("attachments"))?;
        note(report, "attachments/", "copied (hard-linked where possible)", copied);
    }
    let secrets = userdata.join("secrets");
    if secrets.is_dir() {
        let (mut carried, mut left) = (0, 0);
        for entry in fs::read_dir(&secrets)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_file() && SETTINGS_SECRET_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
                if copy_file(&entry.path(), &state_dir.join("secrets").join(&name), true, false)? {
                    carried += 1;
                }
            } else {
                left += 1;
            }
        }
        note(report, "secrets/ (settings, source-control and ChatGPT sign-in)", "copied with owner-only permissions", carried);
        if left > 0 {
            note(report, "secrets/ (session signing keys, T3 Connect link key, other)", "not carried", left);
            if secrets.join("cloud-link-ed25519-key-pair.bin").exists() {
                report.credentials_to_reenter.push("T3 Connect: the link to the hosted relay is not carried; link this server again when it supports T3 Connect".into());
            }
        }
    }
    // What the cache kept of transcripts since deleted would otherwise be lost, so it comes along; a transcript the cache no longer
    // matches is read again as usual.
    for name in USAGE_SCAN_CACHES {
        let from = userdata.join(name);
        if from.is_file() {
            let copied = copy_file(&from, &state_dir.join(name), false, false)?;
            note(report, name, if copied { "copied" } else { "kept the destination's own" }, 1);
        }
    }
    if userdata.join("usage-model-rates.json").exists() {
        note(report, "usage-model-rates.json", "not carried (the rates table is fetched again when needed)", 0);
    }
    for name in ["themes", "browser-artifacts", "logs", "model-manifest.json", "anonymous-id", "server-runtime.json"] {
        if userdata.join(name).exists() {
            note(report, name, "not carried (caches and identifiers this server makes for itself)", 0);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store this server already made may have devices paired to its identity, so the original's does not replace it.
    #[test]
    fn an_identity_a_store_may_have_clients_for_is_kept() {
        let directory = tempfile::tempdir().unwrap();
        let (userdata, state) = (directory.path().join("userdata"), directory.path().join("state"));
        fs::create_dir_all(&userdata).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(userdata.join("environment-id"), "0d555e0a-6745-47b5-a5aa-bb51118f46ae\n").unwrap();
        fs::write(state.join("rust-environment-id"), "5f1f2fbe-8d2c-4a43-a7c5-0cc52c7a3f11\n").unwrap();
        fs::write(state.join("rust-state.sqlite"), b"").unwrap();
        let mut report = Report::default();
        adopt_environment_identity(&userdata, &state, &mut report).unwrap();
        assert_eq!(fs::read_to_string(state.join("rust-environment-id")).unwrap().trim(), "5f1f2fbe-8d2c-4a43-a7c5-0cc52c7a3f11");
        assert!(report.warnings.iter().any(|line| line.contains("kept") && line.contains("0d555e0a-6745-47b5-a5aa-bb51118f46ae")), "{:?}", report.warnings);
        assert!(report.files.iter().any(|file| file.action.contains("not adopted")));
    }
}
