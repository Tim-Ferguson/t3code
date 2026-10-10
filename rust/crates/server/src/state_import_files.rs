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
/// usage-limit source keys, GitHub tokens and the two Bitbucket tokens.
const SETTINGS_SECRET_PREFIXES: [&str; 5] = ["provider-env-", "usage-limit-source-", "github-token-", "bitbucket-access-token", "bitbucket-api-token"];

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

pub fn copy(userdata: &Path, state_dir: &Path, report: &mut Report) -> Result<(), ImportError> {
    if fs::canonicalize(userdata).ok().as_deref() == Some(state_dir) {
        note(report, "settings.json, keybindings.json, attachments/, secrets/", "used in place (same directory)", 0);
        return Ok(());
    }
    for name in ["settings.json", "keybindings.json"] {
        let from = userdata.join(name);
        if from.is_file() {
            let action = if copy_file(&from, &state_dir.join(name), false, false)? { "copied" } else { "kept the destination's own" };
            note(report, name, action, 1);
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
        note(report, "secrets/ (settings and source-control tokens)", "copied with owner-only permissions", carried);
        if left > 0 {
            note(report, "secrets/ (session signing keys, T3 Connect link key, other)", "not carried", left);
            if secrets.join("cloud-link-ed25519-key-pair.bin").exists() {
                report.credentials_to_reenter.push("T3 Connect: the link to the hosted relay is not carried; link this server again when it supports T3 Connect".into());
            }
        }
    }
    for name in ["usage-scan-cache.json", "usage-model-rates.json"] {
        if userdata.join(name).exists() {
            note(report, name, "not carried (this server keeps no usage scan cache; usage is rescanned from provider transcripts, so usage from transcripts that were deleted is not shown)", 0);
        }
    }
    for name in ["themes", "browser-artifacts", "logs", "model-manifest.json", "environment-id", "anonymous-id", "server-runtime.json"] {
        if userdata.join(name).exists() {
            note(report, name, "not carried (caches and identifiers this server makes for itself)", 0);
        }
    }
    Ok(())
}
