//! The desktop shell's SSH service (`t3_desktop::ssh`) as the Connections page's [`Gateway`]: the conversions between the
//! two crates' types, and the file that remembers the hosts added.
use crate::ssh_environments::{Ensured, Gateway, Local, Primary};
use std::path::PathBuf;
use t3_client::ssh::{DiscoveredHost, PromptRequest, Target};
use t3_desktop::ssh::{Ensured as Prepared, HostSource, Ssh, SshTarget};

pub(crate) struct DesktopGateway {
    pub ssh: Ssh,
    /// Where the hosts added are kept.
    pub saved: PathBuf,
    /// How to reach the server this app started, now.
    pub primary: Box<dyn Fn() -> Option<Primary>>,
}

fn to_target(target: SshTarget) -> Target {
    Target { alias: target.alias, hostname: target.hostname, username: target.username, port: target.port }
}
fn from_target(target: Target) -> SshTarget {
    SshTarget { alias: target.alias, hostname: target.hostname, username: target.username, port: target.port }
}

impl Gateway for DesktopGateway {
    fn discover_hosts(&self) -> Local<Vec<DiscoveredHost>> {
        let ssh = self.ssh.clone();
        Box::pin(async move {
            ssh.discover_hosts()
                .await
                .into_iter()
                .map(|host| DiscoveredHost {
                    source: match host.source {
                        HostSource::SshConfig => "ssh-config",
                        HostSource::KnownHosts => "known-hosts",
                    }
                    .into(),
                    target: Target { alias: host.alias, hostname: host.hostname, username: host.username, port: host.port },
                })
                .collect()
        })
    }

    fn resolve_host(&self, alias: String) -> Local<Result<Target, String>> {
        let ssh = self.ssh.clone();
        Box::pin(async move { ssh.resolve_host(&alias).await.map(to_target) })
    }

    fn ensure(&self, target: Target) -> Local<Result<Ensured, String>> {
        let ssh = self.ssh.clone();
        Box::pin(async move {
            Ok(match ssh.ensure(from_target(target), true).await? {
                Prepared::Ready(bootstrap) => Ensured::Ready { http_base_url: bootstrap.http_base_url, pairing_token: bootstrap.pairing_token },
                Prepared::Cancelled(message) => Ensured::Cancelled(message),
            })
        })
    }

    fn disconnect(&self, target: Target) -> Local<Result<(), String>> {
        let ssh = self.ssh.clone();
        Box::pin(async move { ssh.disconnect(from_target(target)).await })
    }

    fn prompts(&self) -> tokio::sync::watch::Receiver<Vec<PromptRequest>> {
        self.ssh.prompts().watch()
    }

    fn answer_prompt(&self, request_id: String, password: Option<String>) -> Result<(), String> {
        self.ssh.prompts().resolve(&request_id, password).map_err(|error| error.to_string())
    }

    fn primary(&self) -> Option<Primary> {
        (self.primary)()
    }

    fn saved(&self) -> Vec<Target> {
        t3_desktop::ssh::load_saved(&self.saved).into_iter().map(to_target).collect()
    }

    fn save(&self, targets: &[Target]) {
        let targets: Vec<SshTarget> = targets.iter().cloned().map(from_target).collect();
        if let Err(error) = t3_desktop::ssh::store_saved(&self.saved, &targets) {
            eprintln!("Cannot save the SSH environments to {}: {error}", self.saved.display());
        }
    }
}
