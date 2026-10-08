use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use t3_server::{
    auth::AuthService,
    config::NativeConfig,
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
    settings: Option<PathBuf>,
    mode: String,
    desktop_telemetry_fd: Option<i32>,
    desktop_telemetry_control_fd: Option<i32>,
}
fn options() -> Result<Options, Box<dyn std::error::Error>> {
    let mut options = Options {
        command: "serve".into(),
        state_dir: std::env::current_dir()?.join(".t3-rust"),
        host: "127.0.0.1".into(),
        port: 3774,
        config: None,
        assets: None,
        settings: None,
        mode: "web".into(),
        desktop_telemetry_fd: None,
        desktop_telemetry_control_fd: None,
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
            "--settings" => {
                options.settings = Some(PathBuf::from(
                    args.next().ok_or("--settings requires a JSON path")?,
                ))
            }
            "--mode" => {
                options.mode = args.next().ok_or("--mode requires web or desktop")?;
                if !matches!(options.mode.as_str(), "web" | "desktop") {
                    return Err("--mode requires web or desktop".into());
                }
            }
            "--desktop-telemetry-fd" => {
                options.desktop_telemetry_fd = Some(
                    args.next()
                        .ok_or("--desktop-telemetry-fd requires an inherited descriptor")?
                        .parse()?,
                )
            }
            "--desktop-telemetry-control-fd" => {
                options.desktop_telemetry_control_fd = Some(
                    args.next()
                        .ok_or("--desktop-telemetry-control-fd requires an inherited descriptor")?
                        .parse()?,
                )
            }
            "--help" | "-h" => {
                println!(
                    "t3-server [serve|pair] [--state-dir PATH] [--host ADDRESS] [--port PORT] [--settings JSON] [--config JSON] [--assets DIRECTORY] [--mode web|desktop] [--desktop-telemetry-fd FD] [--desktop-telemetry-control-fd FD]\nNative port in progress. State defaults to .t3-rust under the current directory.\npair prints a scoped, one-use browser pairing credential valid for five minutes."
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
    #[cfg(unix)]
    #[test]
    fn shared_inherited_socket_is_cloned_before_any_runtime_owns_descriptors() {
        use std::os::fd::{AsRawFd, IntoRawFd};
        let (server, mut host) = std::os::unix::net::UnixStream::pair().unwrap();
        let raw = server.into_raw_fd();
        let options = Options {
            command: "serve".into(),
            state_dir: PathBuf::new(),
            host: String::new(),
            port: 0,
            config: None,
            assets: None,
            settings: None,
            mode: "desktop".into(),
            desktop_telemetry_fd: Some(raw),
            desktop_telemetry_control_fd: Some(raw),
        };
        let descriptors = adopt_desktop_descriptors(&options).unwrap();
        assert_ne!(
            descriptors.input.as_ref().unwrap().as_raw_fd(),
            descriptors.control.as_ref().unwrap().as_raw_fd()
        );
        drop(descriptors.input);
        host.write_all(b"owned").unwrap();
        let mut stream = std::os::unix::net::UnixStream::from(descriptors.control.unwrap());
        let mut bytes = [0; 5];
        std::io::Read::read_exact(&mut stream, &mut bytes).unwrap();
        assert_eq!(&bytes, b"owned");
        drop(stream);
        let mut byte = [0];
        assert_eq!(std::io::Read::read(&mut host, &mut byte).unwrap(), 0);
    }
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

#[cfg(unix)]
struct DesktopDescriptors {
    input: Option<std::os::fd::OwnedFd>,
    control: Option<std::os::fd::OwnedFd>,
}
#[cfg(not(unix))]
struct DesktopDescriptors;
fn adopt_desktop_descriptors(options: &Options) -> std::io::Result<DesktopDescriptors> {
    #[cfg(unix)]
    {
        use std::os::fd::{FromRawFd, OwnedFd};
        // Called at process entry, before Tokio or any service can open/reuse
        // descriptors. These handles are explicitly transferred by the caller.
        let adopt = |fd: i32| -> std::io::Result<OwnedFd> {
            if fd < 3 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "desktop descriptors must be above stderr",
                ));
            }
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let kind = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
            if kind != libc::S_IFIFO && kind != libc::S_IFSOCK {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "desktop descriptors must be inherited pipes or sockets",
                ));
            }
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        };
        let input = options.desktop_telemetry_fd.map(adopt).transpose()?;
        let control = if options.desktop_telemetry_fd.is_some()
            && options.desktop_telemetry_fd == options.desktop_telemetry_control_fd
        {
            input.as_ref().map(OwnedFd::try_clone).transpose()?
        } else {
            options
                .desktop_telemetry_control_fd
                .map(adopt)
                .transpose()?
        };
        Ok(DesktopDescriptors { input, control })
    }
    #[cfg(not(unix))]
    {
        if options.desktop_telemetry_fd.is_some() || options.desktop_telemetry_control_fd.is_some()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Inherited desktop telemetry descriptors are not supported on this platform yet",
            ));
        }
        Ok(DesktopDescriptors)
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = options()?;
    let descriptors = adopt_desktop_descriptors(&options)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(options, descriptors))
}
async fn run(
    options: Options,
    descriptors: DesktopDescriptors,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
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
    let runtime_lease = store.acquire_runtime_lease()?;
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
    let native_config = NativeConfig::load(
        &state_dir,
        &std::env::current_dir()?,
        options.settings.as_deref(),
        &environment,
        &auth.descriptor(),
    )
    .await?;
    if config.is_none() {
        config = Some(native_config.snapshot.clone());
    }
    let execution = t3_server::execution::ExecutionService::try_start_with_lease(
        store.clone(),
        native_config.providers.clone(),
        runtime_lease,
    )?;
    let telemetry = t3_server::native_telemetry::NativeTelemetryClient::new(
        t3_server::native_telemetry::NativeTelemetryOptions::host(std::env::current_dir()?, None),
    );
    let mut desktop_options = t3_server::desktop_telemetry_bootstrap::options_for_settings(
        &options.mode,
        &native_config.settings,
    );
    #[cfg(unix)]
    {
        desktop_options = t3_server::desktop_telemetry_bootstrap::from_owned_descriptors(
            desktop_options,
            descriptors.input,
            descriptors.control,
        )?;
    }
    #[cfg(not(unix))]
    let _ = descriptors;
    let desktop =
        t3_server::desktop_telemetry::DesktopTelemetryReceiver::new(desktop_options).await;
    let resources = t3_server::resource_telemetry_service::ResourceTelemetry::new(
        telemetry.clone(),
        desktop.clone(),
        t3_server::resource_attribution::ResourceAttribution::default(),
        std::process::id() as u64,
        std::sync::Arc::new(|| chrono::Utc::now().timestamp_millis()),
    )
    .await;
    let registry = t3_server::resource_ports::TerminalRegistry::default();
    let discovery = t3_server::resource_discovery::PortDiscovery::new(
        t3_server::resource_discovery::PortDiscoveryOptions::host(os, registry.clone())?,
    );
    let mut terminal_options = t3_server::terminal_manager::TerminalManagerOptions::host(
        state_dir.join("logs").join("terminals"),
        &native_config.settings,
    );
    t3_server::resource_discovery::configure_terminal_tracking(
        &mut terminal_options,
        telemetry.clone(),
        registry,
    );
    let terminals = t3_server::terminal_manager::TerminalManager::new(terminal_options).await?;
    let state = ApiState {
        store,
        auth,
        environment,
        config,
        cors_origins: None,
        assets: options.assets.map(fs::canonicalize).transpose()?,
        providers: Some(native_config.providers),
        execution: Some(execution.clone()),
        workspace: Some(t3_server::workspace_entries::WorkspaceEntries::from_host()?),
        terminals: Some(terminals.clone()),
        discovery: Some(discovery.clone()),
        resource_telemetry: Some(resources.clone()),
    };
    let listener = tokio::net::TcpListener::bind((options.host.as_str(), options.port)).await?;
    tracing::info!(address=%listener.local_addr()?,state_dir=%state_dir.display(),"native server listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    execution.shutdown().await;
    terminals.shutdown().await;
    discovery.shutdown().await;
    tokio::join!(resources.shutdown(), desktop.shutdown());
    telemetry.shutdown().await;
    Ok(())
}
