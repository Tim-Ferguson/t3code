use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use t3_server::{
    auth::AuthService,
    persistence::Store,
    transport::{ApiState, router},
};

struct Options {
    command: String,
    state_dir: PathBuf,
    host: String,
    port: u16,
    config: Option<PathBuf>,
    assets: Option<PathBuf>,
}
fn options() -> Result<Options, Box<dyn std::error::Error>> {
    let mut options = Options {
        command: "serve".into(),
        state_dir: std::env::current_dir()?.join(".t3-rust"),
        host: "127.0.0.1".into(),
        port: 3774,
        config: None,
        assets: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "serve" | "pair" => options.command = arg,
            "--state-dir" => {
                options.state_dir = PathBuf::from(args.next().ok_or("--state-dir requires a path")?)
            }
            "--host" => options.host = args.next().ok_or("--host requires an address")?,
            "--port" => options.port = args.next().ok_or("--port requires a number")?.parse()?,
            "--config" => {
                options.config = Some(PathBuf::from(
                    args.next().ok_or("--config requires a JSON path")?,
                ))
            }
            "--assets" => {
                options.assets = Some(PathBuf::from(
                    args.next()
                        .ok_or("--assets requires the built web asset directory")?,
                ))
            }
            "--help" | "-h" => {
                println!(
                    "t3-server [serve|pair] [--state-dir PATH] [--host ADDRESS] [--port PORT] [--config JSON] [--assets DIRECTORY]\nNative port in progress. State defaults to .t3-rust under the current directory.\npair prints a scoped, one-use browser pairing credential valid for five minutes."
                );
                std::process::exit(0)
            }
            _ => return Err(format!("Unknown argument: {arg}").into()),
        }
    }
    Ok(options)
}

fn secret(path: &Path) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    match fs::read(path) {
        Ok(bytes) => {
            return bytes
                .try_into()
                .map_err(|_| "Invalid native signing-secret length".into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let bytes = rand::random::<[u8; 32]>();
    publish_once(path, &bytes)?;
    fs::read(path)?
        .try_into()
        .map_err(|_| "Invalid native signing-secret length".into())
}

fn identity(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let identity = uuid::Uuid::new_v4().to_string();
    publish_once(path, identity.as_bytes())?;
    let identity = fs::read_to_string(path)?.trim().to_owned();
    uuid::Uuid::parse_str(&identity)?;
    Ok(identity)
}

/// Publish a fully synced inode using an atomic, exclusive hard link. Concurrent
/// serve/pair invocations observe the same complete identity and secret.
fn publish_once(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    if path.exists() {
        return Ok(());
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        match fs::hard_link(&temporary, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    })();
    let _ = fs::remove_file(temporary);
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_startup_observes_complete_stable_identity_and_secret() {
        let directory = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
        let workers = (0..12)
            .map(|_| {
                let root = directory.path().to_owned();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    (
                        identity(&root.join("id")).unwrap(),
                        secret(&root.join("secret")).unwrap(),
                    )
                })
            })
            .collect::<Vec<_>>();
        let values = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert!(values.iter().all(|value| value == &values[0]));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let options = options()?;
    fs::create_dir_all(&options.state_dir)?;
    let state_dir = fs::canonicalize(&options.state_dir)?;
    let store = Store::open(state_dir.join("rust-state.sqlite"))?;
    let environment_id = identity(&state_dir.join("rust-environment-id"))?;
    let instance_hash = format!("{:x}", Sha256::digest(environment_id.as_bytes()));
    let loopback = options
        .host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
        || options.host == "localhost";
    let policy = if loopback {
        "loopback-browser"
    } else {
        "remote-reachable"
    };
    let cookie_name = if loopback {
        format!("t3_session_{}_{}", options.port, &instance_hash[..12])
    } else {
        format!("t3_session_{}", &instance_hash[..12])
    };
    let auth = AuthService::new(
        store.clone(),
        secret(&state_dir.join("rust-signing-secret"))?,
        cookie_name,
        policy.into(),
    )?;
    if options.command == "pair" {
        let scopes = t3_contracts::RPC_REQUIRED_SCOPES
            .iter()
            .map(|(_, scope)| *scope)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .filter(|scope| scope.is_grantable())
            .collect::<Vec<_>>();
        println!(
            "{}",
            auth.create_pairing_credential(
                &scopes,
                chrono::Utc::now(),
                chrono::Duration::minutes(5)
            )?
        );
        return Ok(());
    }
    let mut config = options
        .config
        .map(|path| {
            fs::read_to_string(path).map(|contents| serde_json::from_str::<Value>(&contents))
        })
        .transpose()?
        .transpose()?;
    if let Some(config) = &mut config {
        if config["environment"]["environmentId"].as_str() != Some(environment_id.as_str()) {
            return Err("Configuration environmentId must match the native state directory".into());
        }
        config["auth"] = auth.descriptor();
    }
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        arch => arch,
    };
    let environment = json!({"environmentId":environment_id,"label":"T3 Code Rust","platform":{"os":os,"arch":arch},"serverVersion":env!("CARGO_PKG_VERSION"),"orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}});
    let state = ApiState {
        store,
        auth,
        environment,
        config,
        cors_origins: None,
        assets: options.assets.map(fs::canonicalize).transpose()?,
    };
    let listener = tokio::net::TcpListener::bind((options.host.as_str(), options.port)).await?;
    tracing::info!(address=%listener.local_addr()?,state_dir=%state_dir.display(),"native server listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
