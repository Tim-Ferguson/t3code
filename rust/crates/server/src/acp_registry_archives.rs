//! Original registry archive list/validate/extract policy.
use crate::{
    acp_registry_commands::{CommandOptions, RunningCommand},
    acp_registry_support::RegistryError,
};
use std::{path::Path, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ArchiveKind {
    Raw,
    TarBz2,
    TarGz,
    Zip,
}
impl ArchiveKind {
    pub fn from_url(url: &str) -> Result<Self, RegistryError> {
        let url = url::Url::parse(url).map_err(|cause| RegistryError {
            reason: "archive_invalid",
            detail: cause.to_string(),
        })?;
        let path = url.path().to_ascii_lowercase();
        Ok(if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
            Self::TarGz
        } else if path.ends_with(".tar.bz2") || path.ends_with(".tbz2") {
            Self::TarBz2
        } else if path.ends_with(".zip") {
            Self::Zip
        } else {
            Self::Raw
        })
    }
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Raw => "agent.bin",
            Self::TarBz2 => "agent.tar.bz2",
            Self::TarGz => "agent.tar.gz",
            Self::Zip => "agent.zip",
        }
    }
}
pub(crate) fn command_parts(command: &str) -> Option<Vec<String>> {
    let normalized = t3_contracts::trim_wire_string(command).replace('\\', "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
    let parts: Vec<_> = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    if parts.is_empty()
        || normalized.starts_with('/')
        || (normalized
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
            && normalized.as_bytes().get(1) == Some(&b':'))
        || parts.iter().any(|part| part == "." || part == "..")
    {
        None
    } else {
        Some(parts)
    }
}
pub(crate) fn validate_entries(listing: &str) -> bool {
    listing
        .split('\n')
        .map(t3_contracts::trim_wire_string)
        .filter(|entry| !entry.is_empty())
        .all(|entry| entry == "./" || command_parts(entry).is_some())
}

struct ArchiveRunner {
    environment: Option<indexmap::IndexMap<String, String>>,
}
impl ArchiveRunner {
    async fn run(
        &self,
        command: &str,
        arguments: Vec<String>,
        installation: Arc<dyn Send + Sync>,
    ) -> Result<String, RegistryError> {
        let mut job = RunningCommand::start(
            CommandOptions {
                command: command.into(),
                arguments,
                cwd: None,
                environment: self.environment.clone(),
                timeout: None,
                truncated_output_reason: "archive_invalid",
            },
            installation,
        )?;
        job.wait().await
    }
}

/// Publication and executable validation remain with the guarded installer.
/// A complete bounded listing is checked before any extractor receives the file.
pub(crate) async fn extract(
    kind: ArchiveKind,
    archive: &Path,
    destination: &Path,
    windows: bool,
    installation: Arc<dyn Send + Sync>,
) -> Result<(), RegistryError> {
    extract_with_environment(kind, archive, destination, windows, installation, None).await
}
pub(crate) async fn extract_with_environment(
    kind: ArchiveKind,
    archive: &Path,
    destination: &Path,
    windows: bool,
    installation: Arc<dyn Send + Sync>,
    environment: Option<indexmap::IndexMap<String, String>>,
) -> Result<(), RegistryError> {
    let runner = ArchiveRunner { environment };
    let archive = archive.to_string_lossy().into_owned();
    let destination = destination.to_string_lossy().into_owned();
    let listing = match kind {
        ArchiveKind::Raw => {
            return Err(RegistryError {
                reason: "archive_invalid",
                detail: "Raw registry artifacts do not use archive extraction.".into(),
            });
        }
        ArchiveKind::TarGz => {
            runner
                .run(
                    "tar",
                    vec!["-tzf".into(), archive.clone()],
                    installation.clone(),
                )
                .await?
        }
        ArchiveKind::TarBz2 => {
            runner
                .run(
                    "tar",
                    vec!["-tjf".into(), archive.clone()],
                    installation.clone(),
                )
                .await?
        }
        ArchiveKind::Zip if windows => {
            runner
                .run(
                    "tar",
                    vec!["-tf".into(), archive.clone()],
                    installation.clone(),
                )
                .await?
        }
        ArchiveKind::Zip => {
            runner
                .run(
                    "unzip",
                    vec!["-Z1".into(), archive.clone()],
                    installation.clone(),
                )
                .await?
        }
    };
    if !validate_entries(&listing) {
        return Err(RegistryError {
            reason: "archive_invalid",
            detail: "ACP Registry archive contains an unsafe path.".into(),
        });
    }
    if kind == ArchiveKind::Zip && !windows {
        runner
            .run(
                "unzip",
                vec!["-q".into(), archive, "-d".into(), destination],
                installation,
            )
            .await?;
    } else {
        let flag = match kind {
            ArchiveKind::TarGz => "-xzf",
            ArchiveKind::TarBz2 => "-xjf",
            _ => "-xf",
        };
        runner
            .run(
                "tar",
                vec![flag.into(), archive, "-C".into(), destination],
                installation,
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_archive_policy_fixtures() {
        for line in include_str!("../tests/fixtures/acp-registry-archives.jsonl").lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let input = row["input"].as_str().unwrap_or_default();
            match row["operation"].as_str().unwrap() {
                "path" => assert_eq!(
                    serde_json::to_value(command_parts(input)).unwrap(),
                    row["output"],
                    "{row}"
                ),
                "entries" => assert_eq!(
                    validate_entries(input),
                    row["output"].as_bool().unwrap(),
                    "{row}"
                ),
                "kind" => {
                    let kind = ArchiveKind::from_url(input).unwrap();
                    assert_eq!(kind.file_name(), row["fileName"].as_str().unwrap(), "{row}");
                    let tag = match kind {
                        ArchiveKind::Raw => "raw",
                        ArchiveKind::TarBz2 => "tar_bz2",
                        ArchiveKind::TarGz => "tar_gz",
                        ArchiveKind::Zip => "zip",
                    };
                    assert_eq!(tag, row["output"].as_str().unwrap(), "{row}");
                }
                "collect" => (),
                tag => panic!("Unknown archive fixture {tag}"),
            }
        }
    }
    async fn build_archive(path: &Path, kind: &str, unsafe_entry: bool) {
        let script = "import io,sys,tarfile,zipfile\npath,kind,unsafe=sys.argv[1:]\nname='../outside' if unsafe=='true' else 'bin/agent'\ndata=b'fixture executable'\nif kind=='zip':\n with zipfile.ZipFile(path,'w') as archive: archive.writestr(name,data)\nelse:\n with tarfile.open(path,'w:'+kind) as archive:\n  entry=tarfile.TarInfo(name);entry.size=len(data);archive.addfile(entry,io.BytesIO(data))";
        let mut job = RunningCommand::start(
            CommandOptions {
                command: "python3".into(),
                arguments: vec![
                    "-c".into(),
                    script.into(),
                    path.to_str().unwrap().into(),
                    kind.into(),
                    unsafe_entry.to_string(),
                ],
                cwd: None,
                environment: None,
                timeout: None,
                truncated_output_reason: "archive_invalid",
            },
            Arc::new(()),
        )
        .unwrap();
        job.wait().await.unwrap();
    }
    #[tokio::test]
    async fn actual_tar_and_zip_extract_only_after_complete_safe_listing() {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            for (kind, compression) in [
                (ArchiveKind::TarGz, "gz"),
                (ArchiveKind::TarBz2, "bz2"),
                (ArchiveKind::Zip, "zip"),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let archive = directory.path().join(kind.file_name());
                let destination = directory.path().join("extracted");
                std::fs::create_dir(&destination).unwrap();
                build_archive(&archive, compression, false).await;
                extract(kind, &archive, &destination, false, Arc::new(()))
                    .await
                    .unwrap();
                assert_eq!(
                    std::fs::read(destination.join("bin/agent")).unwrap(),
                    b"fixture executable"
                );
                std::fs::remove_dir_all(&destination).unwrap();
                std::fs::create_dir(&destination).unwrap();
                build_archive(&archive, compression, true).await;
                let failure = extract(kind, &archive, &destination, false, Arc::new(()))
                    .await
                    .unwrap_err();
                assert_eq!(failure.reason, "archive_invalid");
                assert!(!directory.path().join("outside").exists());
                assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
            }
        })
        .await
        .unwrap();
    }
}
