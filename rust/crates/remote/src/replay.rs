//! Replay markers of the relay's signed requests are secrets named
//! `cloud-{mint,health}-{jti,nonce}-<id>` (`secrets.rs`). They only matter while the proof can pass
//! its time check, so the server sweeps old ones (`auth/replayMarkers.ts`).

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::secrets::CLOUD_REPLAY_MARKER_PREFIXES;

/// How long a marker stays on disk. A marker matters for about five minutes (DPoP) or seven (cloud
/// proofs); after that the time check rejects a replay by itself. The sweep and the time check both
/// use the wall clock, so a pruned marker lets a replay through only if the clock moves back by
/// almost a day.
pub const REPLAY_MARKER_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The sweep runs after activation, then at this interval.
pub const REPLAY_MARKER_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// The DPoP verifier's own marker prefix, swept by the same job (`auth/dpop.ts`).
pub const DPOP_REPLAY_MARKER_PREFIX: &str = "dpop-proof-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneReport {
    pub matched: usize,
    pub removed: usize,
    pub failed: usize,
}

/// Deletes replay markers whose mtime is older than [`REPLAY_MARKER_MAX_AGE`] from the secrets
/// directory. The secret store saves each secret as `<name>.bin`, so only
/// `<marker prefix>*.bin` names match; other secrets and `set`'s temp files are never touched.
/// One locked file does not stop the sweep.
pub fn prune_expired_replay_markers(
    secrets_dir: &Path,
    now: SystemTime,
) -> std::io::Result<PruneReport> {
    let cutoff = now
        .checked_sub(REPLAY_MARKER_MAX_AGE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let prefixes = || {
        std::iter::once(DPOP_REPLAY_MARKER_PREFIX)
            .chain(CLOUD_REPLAY_MARKER_PREFIXES.iter().copied())
    };
    let mut report = PruneReport::default();
    for entry in std::fs::read_dir(secrets_dir)? {
        let Ok(entry) = entry else {
            report.failed += 1;
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".bin") || !prefixes().any(|prefix| name.starts_with(prefix)) {
            continue;
        }
        report.matched += 1;
        match entry.metadata().and_then(|meta| meta.modified()) {
            Ok(modified) if modified < cutoff => match std::fs::remove_file(entry.path()) {
                Ok(()) => report.removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => report.failed += 1,
            },
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => report.failed += 1,
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_old_markers() {
        let dir = tempfile::Builder::new()
            .prefix("t3code-rust-port-test-replay-")
            .tempdir()
            .unwrap();
        let write = |name: &str| std::fs::write(dir.path().join(name), b"x").unwrap();
        for name in [
            "cloud-mint-jti-old.bin",
            "cloud-health-nonce-old.bin",
            "dpop-proof-old.bin",
            "cloud-relay-url.bin",
            "cloud-mint-jti-old.bin.1234.tmp",
        ] {
            write(name);
        }
        // Two days later every marker is old; unrelated secrets and temp files are untouched.
        let later = SystemTime::now() + Duration::from_secs(2 * 24 * 60 * 60);
        let report = prune_expired_replay_markers(dir.path(), later).unwrap();
        assert_eq!(
            report,
            PruneReport {
                matched: 3,
                removed: 3,
                failed: 0
            }
        );
        assert!(dir.path().join("cloud-relay-url.bin").exists());
        assert!(dir.path().join("cloud-mint-jti-old.bin.1234.tmp").exists());
        // Fresh markers stay.
        write("cloud-mint-jti-new.bin");
        let report = prune_expired_replay_markers(dir.path(), SystemTime::now()).unwrap();
        assert_eq!(
            report,
            PruneReport {
                matched: 1,
                removed: 0,
                failed: 0
            }
        );
    }
}
