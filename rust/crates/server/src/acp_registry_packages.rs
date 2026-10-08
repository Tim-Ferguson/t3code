//! Managed npm/uv installations stay below this server's tools directory.
use crate::{
    acp_registry_commands::{CommandOptions, RunningCommand},
    acp_registry_support::{Agent, RegistryError},
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

type Resolver = dyn Fn(&str, &IndexMap<String, String>) -> Option<PathBuf> + Send + Sync;
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum Distribution {
    #[serde(rename = "npx")]
    Npm,
    #[serde(rename = "uvx")]
    Uv,
}
impl Distribution {
    fn tag(self) -> &'static str {
        match self {
            Self::Npm => "npx",
            Self::Uv => "uvx",
        }
    }
    fn manager(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Uv => "uv",
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Receipt {
    pub agent_id: String,
    pub agent_version: String,
    pub distribution: Distribution,
    pub package_spec: String,
    pub manager_path: String,
    pub bin_directory: String,
    pub executable_path: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "t3_contracts::deserialize_optional"
    )]
    pub package_root: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "t3_contracts::deserialize_optional"
    )]
    pub package_version: Option<String>,
}
fn error(detail: impl Into<String>) -> RegistryError {
    RegistryError {
        reason: "install_failed",
        detail: detail.into(),
    }
}
fn command_name(name: &str) -> bool {
    name.encode_utf16().count() <= 128
        && regex::Regex::new(r"(?i)\A[a-z0-9][a-z0-9._-]*\z")
            .unwrap()
            .is_match(name)
}
fn command_candidates(agent_id: &str, package_name: &str) -> Vec<String> {
    let base = package_name.rsplit('/').next().unwrap_or(package_name);
    let mut result = Vec::new();
    for candidate in [agent_id, base, base.strip_suffix("-acp").unwrap_or(base)] {
        if command_name(candidate) && !result.iter().any(|existing| existing == candidate) {
            result.push(candidate.into());
        }
    }
    result
}
fn package_identity(spec: &str, distribution: Distribution) -> (&str, &str) {
    let (separator, width) = if distribution == Distribution::Uv {
        spec.rfind("==")
            .map(|index| (index, 2))
            .or_else(|| spec.rfind('@').map(|index| (index, 1)))
    } else {
        spec.rfind('@').map(|index| (index, 1))
    }
    .expect("catalog admitted an exactly pinned package");
    let version = &spec[separator + width..];
    (
        &spec[..separator],
        version.strip_prefix(['v', 'V']).unwrap_or(version),
    )
}
fn decode_bin(bin: &Value) -> Option<Value> {
    if let Some(path) = bin.as_str() {
        return (path.encode_utf16().count() <= 1024).then(|| bin.clone());
    }
    let object = bin.as_object()?;
    let mut result = serde_json::Map::new();
    for (name, path) in object {
        // Effect Record decodes only properties admitted by its key schema.
        if !command_name(name) {
            continue;
        }
        if path
            .as_str()
            .is_none_or(|path| path.encode_utf16().count() > 1024)
        {
            return None;
        }
        result.insert(name.clone(), path.clone());
    }
    Some(Value::Object(result))
}
fn npm_command(agent_id: &str, package: &str, bin: &Value) -> Option<String> {
    let bin = decode_bin(bin)?;
    let base = package.rsplit('/').next().unwrap_or(package);
    if bin.is_string() {
        return Some(base.into());
    }
    let object = bin.as_object()?;
    let mut entries = object.iter().collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
    if entries.len() == 1 {
        return Some(entries[0].0.clone());
    }
    if object.contains_key(base) {
        return Some(base.into());
    }
    if object.contains_key(agent_id) {
        return Some(agent_id.into());
    }
    let first = entries.first()?;
    entries
        .iter()
        .all(|(_, path)| path == &first.1)
        .then(|| first.0.clone())
}
fn parse_manager_path(manager: &Path, output: &str) -> Result<PathBuf, RegistryError> {
    let lines = output
        .split('\n')
        .map(t3_contracts::trim_wire_string)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1 || !Path::new(lines[0]).is_absolute() {
        return Err(error(format!(
            "ACP Registry package manager '{}' returned an invalid global path.",
            manager.display()
        )));
    }
    Ok(lines[0].into())
}
pub(crate) fn preferred_path(
    environment: &IndexMap<String, String>,
    directory: &Path,
) -> IndexMap<String, String> {
    crate::acp_registry_spawn::preferred_path(
        environment,
        &directory.to_string_lossy(),
        cfg!(windows),
    )
}
async fn run(
    manager: &Path,
    arguments: Vec<String>,
    environment: &IndexMap<String, String>,
    timeout: Duration,
    installation: Arc<dyn Send + Sync>,
) -> Result<String, RegistryError> {
    let mut job = RunningCommand::start(
        CommandOptions {
            command: manager.to_string_lossy().into_owned(),
            arguments,
            cwd: None,
            environment: Some(environment.clone()),
            timeout: Some(timeout),
            truncated_output_reason: "install_failed",
        },
        installation,
    )?;
    job.wait().await
}

pub(crate) struct PackageInstaller<'a> {
    pub agent: &'a Agent,
    pub distribution: Distribution,
    pub spec: &'a str,
    pub agent_root: &'a Path,
    pub receipts: &'a Path,
    pub environment: IndexMap<String, String>,
    pub resolve: Arc<Resolver>,
    /// Retains installation admission until every canceled manager is reaped.
    pub installation: Arc<dyn Send + Sync>,
}
impl PackageInstaller<'_> {
    fn root(&self) -> PathBuf {
        self.agent_root
            .join(if self.distribution == Distribution::Npm {
                "npm"
            } else {
                "python"
            })
    }
    fn bin(&self) -> PathBuf {
        if cfg!(windows) && self.distribution == Distribution::Npm {
            self.root()
        } else {
            self.root().join("bin")
        }
    }
    fn receipt_path(&self, manager: &Path) -> PathBuf {
        let digest = Sha256::digest(format!(
            "{}\0{}",
            self.distribution.tag(),
            manager.display()
        ));
        let prefix = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        self.receipts
            .join(format!("{}-{prefix}.json", self.agent.id))
    }
    async fn cached(&self, manager: &Path) -> Option<Receipt> {
        let bytes = tokio::fs::read(self.receipt_path(manager)).await.ok()?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        if !value.is_object() {
            return None;
        }
        let receipt: Receipt = serde_json::from_value(value).ok()?;
        if receipt.agent_id != self.agent.id
            || receipt.agent_version != self.agent.version
            || receipt.distribution != self.distribution
            || receipt.package_spec != self.spec
            || Path::new(&receipt.manager_path) != manager
            || Path::new(&receipt.bin_directory) != self.bin()
            || Path::new(&receipt.executable_path).parent()
                != Some(Path::new(&receipt.bin_directory))
            || !tokio::fs::try_exists(&receipt.executable_path).await.ok()?
        {
            return None;
        }
        if self.distribution == Distribution::Npm {
            let root = receipt.package_root.as_ref()?;
            let version = receipt.package_version.as_ref()?;
            let manifest = self.manifest(&Path::new(root).join("package.json")).await?;
            let (name, expected) = package_identity(self.spec, self.distribution);
            if manifest["name"] != name || manifest["version"] != expected || version != expected {
                return None;
            }
        }
        Some(receipt)
    }
    async fn manifest(&self, path: &Path) -> Option<Value> {
        if tokio::fs::metadata(path).await.ok()?.len() > 1024 * 1024 {
            return None;
        }
        let bytes = tokio::fs::read(path).await.ok()?;
        let mut value: Value = serde_json::from_slice(&bytes).ok()?;
        value["name"].as_str()?;
        value["version"].as_str()?;
        value["bin"] = decode_bin(&value["bin"])?;
        Some(value)
    }
    async fn discover(
        &self,
        manager: &Path,
        environment: &IndexMap<String, String>,
    ) -> Result<Option<Receipt>, RegistryError> {
        let (name, version) = package_identity(self.spec, self.distribution);
        let query = |args: Vec<String>| {
            run(
                manager,
                args,
                environment,
                Duration::from_secs(30),
                self.installation.clone(),
            )
        };
        let (bin, package_root) = if self.distribution == Distribution::Npm {
            let root = parse_manager_path(
                manager,
                &query(vec!["root".into(), "--global".into()]).await?,
            )?;
            let prefix = parse_manager_path(
                manager,
                &query(vec!["prefix".into(), "--global".into()]).await?,
            )?;
            let package_root = name.split('/').fold(root, |root, part| root.join(part));
            let Some(manifest) = self.manifest(&package_root.join("package.json")).await else {
                return Ok(None);
            };
            if manifest["name"] != name || manifest["version"] != version {
                return Ok(None);
            }
            let Some(command) = npm_command(&self.agent.id, name, &manifest["bin"]) else {
                return Ok(None);
            };
            let bin = if cfg!(windows) {
                prefix
            } else {
                prefix.join("bin")
            };
            let Some(executable) = (self.resolve)(&command, &preferred_path(environment, &bin))
            else {
                return Ok(None);
            };
            return Ok(Some(self.receipt(
                manager,
                bin,
                executable,
                Some(package_root),
                version,
            )));
        } else {
            let tools = query(vec!["tool".into(), "list".into()]).await?;
            let whitespace = r"\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";
            let pattern = regex::Regex::new(&format!(
                r"\A([^{whitespace}]+) v([^{whitespace}]+)(?:[{whitespace}]|\z)"
            ))
            .unwrap();
            let normalized = |name: &str| {
                regex::Regex::new("[._-]+")
                    .unwrap()
                    .replace_all(&name.to_lowercase(), "-")
                    .into_owned()
            };
            if !tools.split('\n').any(|line| {
                pattern.captures(line).is_some_and(|captures| {
                    normalized(&captures[1]) == normalized(name) && &captures[2] == version
                })
            }) {
                return Ok(None);
            }
            (
                parse_manager_path(
                    manager,
                    &query(vec!["tool".into(), "dir".into(), "--bin".into()]).await?,
                )?,
                None,
            )
        };
        let executable = command_candidates(&self.agent.id, name)
            .into_iter()
            .find_map(|candidate| (self.resolve)(&candidate, &preferred_path(environment, &bin)));
        Ok(executable
            .map(|executable| self.receipt(manager, bin, executable, package_root, version)))
    }
    fn receipt(
        &self,
        manager: &Path,
        bin: PathBuf,
        executable: PathBuf,
        root: Option<PathBuf>,
        version: &str,
    ) -> Receipt {
        Receipt {
            agent_id: self.agent.id.clone(),
            agent_version: self.agent.version.clone(),
            distribution: self.distribution,
            package_spec: self.spec.into(),
            manager_path: manager.to_string_lossy().into_owned(),
            bin_directory: bin.to_string_lossy().into_owned(),
            executable_path: executable.to_string_lossy().into_owned(),
            package_root: root.map(|root| root.to_string_lossy().into_owned()),
            package_version: Some(version.into()),
        }
    }
    async fn write_receipt(&self, manager: &Path, receipt: &Receipt) -> Result<(), RegistryError> {
        let path = self.receipt_path(manager);
        let bytes = serde_json::to_vec(receipt).map_err(|cause| error(cause.to_string()))?;
        let installation = self.installation.clone();
        tokio::task::spawn_blocking(move || {
            struct Temporary(PathBuf);
            impl Drop for Temporary {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let _installation = installation;
            std::fs::create_dir_all(path.parent().unwrap())?;
            let temporary = Temporary(PathBuf::from(format!(
                "{}.{}.tmp",
                path.display(),
                std::process::id()
            )));
            let _ = std::fs::remove_file(&temporary.0);
            let mut bytes = bytes;
            bytes.push(b'\n');
            std::fs::write(&temporary.0, bytes)?;
            match std::fs::remove_file(&path) {
                Ok(()) => (),
                Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => (),
                Err(cause) => return Err(cause),
            }
            std::fs::rename(&temporary.0, &path)
        })
        .await
        .map_err(|cause| error(cause.to_string()))?
        .map_err(|cause| error(cause.to_string()))
    }
    pub async fn ensure(&self) -> Result<Receipt, RegistryError> {
        let manager=(self.resolve)(self.distribution.manager(),&self.environment).ok_or_else(||RegistryError {reason:"runner_unavailable",detail:format!("ACP Registry agent {} requires '{}', but it is not available on this environment's PATH.",self.agent.id,self.distribution.manager())})?;
        let cached = self.cached(&manager).await;
        if self.distribution == Distribution::Npm && cached.is_some() {
            return Ok(cached.unwrap());
        }
        let root = self.root();
        let installation = self.installation.clone();
        tokio::task::spawn_blocking(move || {
            let _installation = installation;
            std::fs::create_dir_all(root)
        })
        .await
        .map_err(|cause| error(cause.to_string()))?
        .map_err(|cause| error(cause.to_string()))?;
        let mut environment = self.environment.clone();
        if self.distribution == Distribution::Npm {
            environment.insert(
                "npm_config_prefix".into(),
                self.root().to_string_lossy().into_owned(),
            );
        } else {
            environment.insert(
                "UV_TOOL_DIR".into(),
                self.root().to_string_lossy().into_owned(),
            );
            environment.insert(
                "UV_TOOL_BIN_DIR".into(),
                self.bin().to_string_lossy().into_owned(),
            );
        }
        if cached.is_some() {
            if let Some(receipt) = self.discover(&manager, &environment).await? {
                return Ok(receipt);
            }
        }
        if let Some(receipt) = self.discover(&manager, &environment).await? {
            self.write_receipt(&manager, &receipt).await?;
            return Ok(receipt);
        }
        let directory = match std::fs::read_link(&manager) {
            Ok(target) => {
                let target = if target.is_absolute() {
                    target
                } else {
                    manager.parent().unwrap().join(target)
                };
                if target.file_name() == manager.file_name() {
                    target.parent().unwrap().to_owned()
                } else {
                    manager.parent().unwrap().to_owned()
                }
            }
            Err(_) => manager.parent().unwrap().to_owned(),
        };
        let arguments = if self.distribution == Distribution::Npm {
            vec!["install", "--global", self.spec]
        } else {
            vec!["tool", "install", "--force", self.spec]
        };
        run(
            &manager,
            arguments.into_iter().map(str::to_owned).collect(),
            &preferred_path(&environment, &directory),
            Duration::from_secs(20 * 60),
            self.installation.clone(),
        )
        .await?;
        let receipt = self
            .discover(&manager, &environment)
            .await?
            .ok_or_else(|| {
                error(format!(
                    "ACP Registry installed {}, but could not resolve its global command.",
                    self.spec
                ))
            })?;
        self.write_receipt(&manager, &receipt).await?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_package_policy_fixtures() {
        for line in include_str!("../tests/fixtures/acp-registry-package-policy.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            match row["operation"].as_str().unwrap() {
                "identity" => {
                    let distribution = if row["distribution"] == "npx" {
                        Distribution::Npm
                    } else {
                        Distribution::Uv
                    };
                    let (name, version) =
                        package_identity(row["input"].as_str().unwrap(), distribution);
                    assert_eq!(
                        serde_json::json!({"name":name,"version":version}),
                        row["output"],
                        "{row}"
                    );
                }
                "candidates" => assert_eq!(
                    serde_json::json!(command_candidates(
                        row["id"].as_str().unwrap(),
                        row["name"].as_str().unwrap()
                    )),
                    row["output"],
                    "{row}"
                ),
                "npmCommand" => {
                    let input = &row["input"];
                    assert_eq!(
                        decode_bin(&input["bin"]).is_some(),
                        row["ok"].as_bool().unwrap(),
                        "{row}"
                    );
                    if row["ok"] == true {
                        assert_eq!(
                            serde_json::json!(npm_command(
                                row["id"].as_str().unwrap(),
                                input["name"].as_str().unwrap(),
                                &input["bin"]
                            )),
                            row["output"],
                            "{row}"
                        );
                    }
                }
                tag => panic!("Unknown package fixture {tag}"),
            }
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn actual_npm_and_uv_managers_use_owned_prefixes_and_validate_cached_identity() {
        tokio::time::timeout(Duration::from_secs(20), async {
            use std::os::unix::fs::PermissionsExt;
            for distribution in [Distribution::Npm,Distribution::Uv] {
                let directory=tempfile::tempdir().unwrap();
                let manager=directory.path().join(distribution.manager());
                let source=if distribution==Distribution::Npm {include_str!("../tests/fixtures/acp-registry-npm.fixture.sh")}else{include_str!("../tests/fixtures/acp-registry-uv.fixture.sh")};
                std::fs::write(&manager,source).unwrap();
                std::fs::set_permissions(&manager,std::fs::Permissions::from_mode(0o755)).unwrap();
                let log=directory.path().join("manager.log");
                let mut environment=std::env::vars().collect::<IndexMap<_,_>>();
                environment.insert(if distribution==Distribution::Npm{"FAKE_NPM_LOG"}else{"FAKE_UV_LOG"}.into(),log.to_string_lossy().into_owned());
                environment.insert("FAKE_NPM_MANIFEST".into(),serde_json::json!({"name":"@example/acp","version":"1.2.3","bin":{"example-agent":"index.js"}}).to_string());
                let captured=manager.clone();
                let resolve:Arc<Resolver>=Arc::new(move |command,environment| {
                    if command=="npm"||command=="uv"{return Some(captured.clone());}
                    environment.get("PATH")?.split(':').map(|directory|Path::new(directory).join(command)).find(|path|std::fs::metadata(path).is_ok_and(|info|info.is_file()&&info.permissions().mode()&0o111!=0))
                });
                let agent=Agent{id:if distribution==Distribution::Npm{"example-agent"}else{"fast-agent"}.into(),name:"Fixture".into(),version:"1.0.0".into(),description:"fixture".into(),authors:Vec::new(),license:None,website:None,repository:None,icon:None,binaries:IndexMap::new(),npx:None,uvx:None};
                let root=directory.path().join("tools/agent/1.0.0");
                let receipts=directory.path().join("caches/acp-registry/package-installs");
                let installer=PackageInstaller{agent:&agent,distribution,spec:if distribution==Distribution::Npm{"@example/acp@1.2.3"}else{"fast-agent-acp==0.10.1"},agent_root:&root,receipts:&receipts,environment,resolve,installation:Arc::new(())};
                let first=installer.ensure().await.unwrap();
                assert!(Path::new(&first.executable_path).starts_with(&root));
                assert!(Path::new(&first.executable_path).exists());
                assert_eq!(Path::new(&first.bin_directory),installer.bin());
                let before=std::fs::read_to_string(&log).unwrap();
                let second=installer.ensure().await.unwrap();
                assert_eq!(first.executable_path,second.executable_path);
                let after=std::fs::read_to_string(&log).unwrap();
                if distribution==Distribution::Npm {
                    assert_eq!(before,after,"validated npm receipt avoids manager calls");
                    std::fs::write(Path::new(first.package_root.as_ref().unwrap()).join("package.json"),serde_json::json!({"name":"@example/acp","version":"0.0.0","bin":{"example-agent":"index.js"}}).to_string()).unwrap();
                }else{
                    assert!(after.len()>before.len(),"uv receipt still requires exact tool-list identity");
                    std::fs::remove_file(&first.executable_path).unwrap();
                }
                let third=installer.ensure().await.unwrap();
                assert_eq!(third.executable_path,first.executable_path);
                let calls=std::fs::read_to_string(&log).unwrap();
                let install=if distribution==Distribution::Npm{"install --global @example/acp@1.2.3"}else{"tool install --force fast-agent-acp==0.10.1"};
                assert_eq!(calls.lines().filter(|line|*line==install).count(),2);
                assert_eq!(std::fs::read_dir(&receipts).unwrap().count(),1);
            }
        }).await.unwrap();
    }
}
