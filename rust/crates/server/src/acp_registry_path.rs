//! Disk-only managed ACP command directories for append-only terminal PATH exposure.
use crate::{
    acp_registry_archives::command_parts,
    acp_registry_packages::{Distribution, Receipt},
    acp_registry_support::{decode_agent, encode_version, platform_target, version},
};
use icu_collator::{Collator, CollatorPreferences, preferences::CollationNumericOrdering};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct ManagedBinaryDirectories {
    /// Server cache root; registry artifacts live in its `acp-registry` child.
    pub cache_dir: PathBuf,
    pub tools_dir: PathBuf,
    /// Node platform/architecture spellings, e.g. `darwin` / `arm64`.
    pub platform: String,
    pub architecture: String,
}

impl ManagedBinaryDirectories {
    /// Missing or damaged artifacts are omitted. This never refreshes or installs.
    pub async fn directories(&self) -> Vec<PathBuf> {
        let input = self.clone();
        tokio::task::spawn_blocking(move || input.read())
            .await
            .unwrap_or_default()
    }

    fn read(&self) -> Vec<PathBuf> {
        let registry = self.cache_dir.join("acp-registry");
        let receipts = registry.join("package-installs");
        let mut result = Vec::new();
        for filename in names(&receipts) {
            let Some(receipt) = std::fs::read(receipts.join(filename))
                .ok()
                .and_then(|bytes| decode_receipt(&bytes))
            else {
                continue;
            };
            let mut expected = self
                .tools_dir
                .join(&receipt.agent_id)
                .join(encode_version(&receipt.agent_version))
                .join(match receipt.distribution {
                    Distribution::Npm => "npm",
                    Distribution::Uv => "python",
                });
            if receipt.distribution != Distribution::Npm || self.platform != "win32" {
                expected.push("bin");
            }
            let executable = Path::new(&receipt.executable_path);
            if receipt.bin_directory == expected.to_string_lossy()
                && receipt_parent(&receipt.executable_path) == receipt.bin_directory
                && executable.exists()
            {
                result.push(expected);
            }
        }
        if let Some(target) = platform_target(&self.platform, &self.architecture) {
            let cached = cached_agents(&registry.join("registry.json"));
            let mut preferences: CollatorPreferences =
                crate::workspace_entries::locale_from_environment().into();
            preferences.numeric_ordering = Some(CollationNumericOrdering::True);
            let collator = Collator::try_new(preferences, Default::default())
                .expect("compiled ICU collation data");
            for agent in names(&self.tools_dir) {
                let agent_root = self.tools_dir.join(&agent);
                let mut versions = names(&agent_root);
                versions.sort_by(|left, right| collator.compare(right, left));
                for version in versions {
                    let root = agent_root.join(&version).join(&target);
                    if !root.exists() {
                        continue;
                    }
                    let parts = cached
                        .get(&(agent.clone(), version))
                        .and_then(|agent| agent.binaries.get(&target))
                        .and_then(|target| command_parts(&target.command));
                    let directory = parts
                        .map(|parts| {
                            parts[..parts.len() - 1]
                                .iter()
                                .fold(root.clone(), |path, part| path.join(part))
                        })
                        .unwrap_or(root);
                    if directory.exists() {
                        result.push(directory);
                    }
                }
            }
        }
        let mut seen = HashSet::new();
        result.retain(|directory| seen.insert(directory.clone()));
        result
    }
}

// Node dirname preserves embedded `.` and repeated separators. Path::parent
// normalizes them, which would admit receipts rejected by the source service.
fn receipt_parent(path: &str) -> &str {
    let bytes = path.as_bytes();
    let separator = |byte: u8| byte == b'/' || (cfg!(windows) && byte == b'\\');
    let mut end = bytes.len();
    while end > 0 && separator(bytes[end - 1]) {
        end -= 1;
    }
    let Some(index) = bytes[..end].iter().rposition(|byte| separator(*byte)) else {
        return ".";
    };
    if index == 0 {
        &path[..1]
    } else if index == 1 && separator(bytes[0]) {
        &path[..2]
    } else if cfg!(windows) && index == 2 && bytes[1] == b':' {
        &path[..3]
    } else {
        &path[..index]
    }
}

fn names(directory: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut names: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    names
}

fn decode_receipt(bytes: &[u8]) -> Option<Receipt> {
    let value: Value = serde_json::from_str(&String::from_utf8_lossy(bytes)).ok()?;
    if !value.is_object() {
        return None;
    }
    let mut receipt: Receipt = serde_json::from_value(value.clone()).ok()?;
    if receipt.agent_id.len() > 128
        || !receipt
            .agent_id
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        || !receipt
            .agent_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return None;
    }
    receipt.agent_version = version(&value["agentVersion"]).ok()?;
    Some(receipt)
}

fn cached_agents(path: &Path) -> HashMap<(String, String), crate::acp_registry_support::Agent> {
    let decoded = || -> Option<_> {
        let bytes = std::fs::read(path).ok()?;
        let value: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).ok()?;
        version(&value["version"]).ok()?;
        let agents = value["agents"]
            .as_array()
            .filter(|agents| agents.len() <= 512)?;
        Some(
            agents
                .iter()
                .filter_map(|value| decode_agent(value).ok())
                .map(|agent| ((agent.id.clone(), encode_version(&agent.version)), agent))
                .collect(),
        )
    };
    decoded().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unchanged_source_native_filesystem_witnesses() {
        for line in include_str!("../tests/fixtures/acp-registry-path.jsonl").lines() {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let root = tempfile::tempdir().unwrap();
            let root_string = root.path().to_string_lossy();
            for file in fixture["files"].as_array().unwrap() {
                let destination = root.path().join(file["path"].as_str().unwrap());
                if file["directory"] == true {
                    std::fs::create_dir_all(destination).unwrap();
                } else {
                    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
                    std::fs::write(
                        destination,
                        file["contents"]
                            .as_str()
                            .unwrap()
                            .replace("$ROOT", &root_string),
                    )
                    .unwrap();
                }
            }
            // Windows selection branches are tested on this native filesystem;
            // this does not establish Windows filesystem/OS execution parity.
            let directories = ManagedBinaryDirectories {
                cache_dir: root.path().join("cache"),
                tools_dir: root.path().join("tools"),
                platform: fixture["platform"].as_str().unwrap().into(),
                architecture: fixture["architecture"].as_str().unwrap().into(),
            }
            .directories()
            .await;
            let actual: Vec<_> = directories
                .iter()
                .map(|path| {
                    path.to_string_lossy()
                        .replace(root_string.as_ref(), "$ROOT")
                })
                .collect();
            assert_eq!(
                serde_json::json!(actual),
                fixture["output"],
                "{}",
                fixture["label"]
            );
        }
    }
}
