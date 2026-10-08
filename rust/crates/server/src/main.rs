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
}
fn options() -> Result<Options, Box<dyn std::error::Error>> {
    let mut options = Options {
        command: "serve".into(),
        state_dir: std::env::current_dir()?.join(".t3-rust"),
        host: "127.0.0.1".into(),
        port: 3774,
        config: None,
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
            "--help" | "-h" => {
                println!(
                    "t3-server [serve|pair] [--state-dir PATH] [--host ADDRESS] [--port PORT] [--config JSON]\nNative port in progress. State defaults to .t3-rust under the current directory.\npair prints a scoped, one-use browser pairing credential valid for five minutes."
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
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(&bytes)?;
            file.sync_all()?;
            Ok(bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => secret(path),
        Err(error) => Err(error.into()),
    }
}

fn identity(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    if path.exists() {
        return Ok(fs::read_to_string(path)?.trim().into());
    }
    let identity = uuid::Uuid::new_v4().to_string();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(identity.as_bytes())?;
            file.sync_all()?;
            Ok(identity)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => self::identity(path),
        Err(error) => Err(error.into()),
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
