//! Source uninstall removes only platform binary directories, retaining managed
//! npm/python installations and refusing provider-referenced agents.
use std::path::Path;
use t3_contracts::ServerSettings;
pub(crate) fn referenced(settings: &ServerSettings, agent_id: &str) -> bool {
    settings.provider_instances.values().any(|instance| {
        instance.driver.as_str() == "acpRegistry"
            && instance
                .config
                .as_ref()
                .and_then(|value| value.as_object())
                .is_some_and(|config| {
                    config.get("source").and_then(|value| value.as_str()) != Some("local")
                        && config
                            .get("agentId")
                            .and_then(|value| value.as_str())
                            .is_some_and(|id| t3_contracts::trim_wire_string(id) == agent_id)
                })
    })
}
pub(crate) fn remove_binary_directories(agent_root: &Path) -> std::io::Result<bool> {
    let metadata = match std::fs::symlink_metadata(agent_root) {
        Ok(metadata) => metadata,
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(cause) => return Err(cause),
    };
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ACP binary uninstall refuses symlink agent roots.",
        ));
    }
    // Narrow safety deviation: original readDirectory follows version symlinks.
    // Validate all immediate ancestors before removing any binary directories.
    let versions = std::fs::read_dir(agent_root)?.collect::<Result<Vec<_>, _>>()?;
    for version in &versions {
        if version.file_type()?.is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ACP binary uninstall refuses symlink version roots.",
            ));
        }
    }
    let mut removed = false;
    for version in versions {
        let version = version.path();
        for entry in std::fs::read_dir(&version)? {
            let entry = entry?;
            if matches!(
                entry.file_name().to_str(),
                Some(
                    "darwin-aarch64"
                        | "darwin-x86_64"
                        | "linux-aarch64"
                        | "linux-x86_64"
                        | "windows-aarch64"
                        | "windows-x86_64"
                )
            ) {
                // rm recursive removes a symlink itself, never its target.
                if entry.file_type()?.is_symlink() || entry.file_type()?.is_file() {
                    std::fs::remove_file(entry.path())?;
                } else {
                    std::fs::remove_dir_all(entry.path())?;
                }
                removed = true;
            }
        }
        if std::fs::read_dir(&version)?.next().transpose()?.is_none() {
            std::fs::remove_dir(&version)?;
        }
    }
    if std::fs::read_dir(agent_root)?.next().transpose()?.is_none() {
        std::fs::remove_dir(agent_root)?;
    }
    Ok(removed)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_platform_binaries_are_removed_and_package_roots_remain() {
        let directory = tempfile::tempdir().unwrap();
        let agent = directory.path().join("agent");
        for name in [
            "darwin-aarch64",
            "windows-x86_64",
            "npm",
            "python",
            "linux-x86_64.lock",
            "unrecognized",
        ] {
            std::fs::create_dir_all(agent.join("1.2.3").join(name)).unwrap();
            std::fs::write(agent.join("1.2.3").join(name).join("owned"), name).unwrap();
        }
        std::fs::create_dir_all(agent.join("empty-version")).unwrap();
        assert!(remove_binary_directories(&agent).unwrap());
        assert!(!agent.join("1.2.3/darwin-aarch64").exists());
        assert!(!agent.join("1.2.3/windows-x86_64").exists());
        assert!(!agent.join("empty-version").exists());
        for name in ["npm", "python", "linux-x86_64.lock", "unrecognized"] {
            assert!(agent.join("1.2.3").join(name).join("owned").is_file());
        }
        assert!(!remove_binary_directories(&agent).unwrap());
        assert!(!remove_binary_directories(&directory.path().join("missing")).unwrap());
    }
    #[cfg(unix)]
    #[test]
    fn removing_platform_symlink_never_removes_external_target() {
        let directory = tempfile::tempdir().unwrap();
        let external = directory.path().join("external");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("keep"), "owned elsewhere").unwrap();
        let agent = directory.path().join("agent");
        std::fs::create_dir_all(agent.join("1")).unwrap();
        std::os::unix::fs::symlink(&external, agent.join("1/linux-x86_64")).unwrap();
        assert!(remove_binary_directories(&agent).unwrap());
        assert!(external.join("keep").is_file());
        assert!(!agent.exists());
    }
    #[cfg(unix)]
    #[test]
    fn symlink_agent_and_version_ancestors_never_remove_external_binaries() {
        let directory = tempfile::tempdir().unwrap();
        let external = directory.path().join("external");
        std::fs::create_dir_all(external.join("1/linux-x86_64")).unwrap();
        std::fs::write(external.join("1/linux-x86_64/keep"), "external").unwrap();
        let agent = directory.path().join("agent");
        std::os::unix::fs::symlink(&external, &agent).unwrap();
        assert!(remove_binary_directories(&agent).is_err());
        assert!(external.join("1/linux-x86_64/keep").is_file());
        std::fs::remove_file(&agent).unwrap();
        std::fs::create_dir(&agent).unwrap();
        std::os::unix::fs::symlink(external.join("1"), agent.join("1")).unwrap();
        assert!(remove_binary_directories(&agent).is_err());
        assert!(external.join("1/linux-x86_64/keep").is_file());
        assert!(
            agent
                .join("1")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
