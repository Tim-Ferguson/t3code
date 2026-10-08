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
    bootstrap_fd: Option<i32>,
    bootstrap: Option<t3_contracts::DesktopBackendBootstrap>,
    overrides: std::collections::HashSet<String>,
}
fn options() -> Result<Options, Box<dyn std::error::Error>> {
    parse_options(std::env::args().skip(1), |key| std::env::var(key).ok())
}
fn parse_options(
    args: impl IntoIterator<Item = String>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Options, Box<dyn std::error::Error>> {
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
        bootstrap_fd: env("T3CODE_BOOTSTRAP_FD")
            .map(|value| value.parse())
            .transpose()?,
        bootstrap: None,
        overrides: Default::default(),
    };
    for (key, name) in [
        ("T3CODE_HOST", "--host"),
        ("T3CODE_PORT", "--port"),
        ("T3CODE_MODE", "--mode"),
        ("T3CODE_HOME", "--state-dir"),
    ] {
        if let Some(value) = env(key) {
            match name {
                "--host" => options.host = value,
                "--port" => {
                    options.port = value.parse()?;
                    if options.port == 0 {
                        return Err("T3CODE_PORT must be between 1 and 65535".into());
                    }
                }
                "--mode" => {
                    if !matches!(value.as_str(), "web" | "desktop") {
                        return Err("T3CODE_MODE requires web or desktop".into());
                    }
                    options.mode = value;
                }
                "--state-dir" => {
                    if value.trim().is_empty() {
                        continue;
                    }
                    options.state_dir = PathBuf::from(value.trim()).join("userdata");
                }
                _ => unreachable!(),
            }
            options.overrides.insert(name.to_owned());
        }
    }
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if matches!(
            arg.as_str(),
            "--host"
                | "--port"
                | "--mode"
                | "--state-dir"
                | "--desktop-telemetry-fd"
                | "--desktop-telemetry-control-fd"
        ) {
            options.overrides.insert(arg.clone());
        }
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
            "--bootstrap-fd" => {
                options.bootstrap_fd = Some(
                    args.next()
                        .ok_or("--bootstrap-fd requires an inherited descriptor")?
                        .parse()?,
                )
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
                    "t3-server [serve|pair] [--state-dir PATH] [--host ADDRESS] [--port PORT] [--settings JSON] [--config JSON] [--assets DIRECTORY] [--mode web|desktop] [--bootstrap-fd FD] [--desktop-telemetry-fd FD] [--desktop-telemetry-control-fd FD]\nNative port in progress. State defaults to .t3-rust under the current directory.\npair prints a scoped, one-use browser pairing credential valid for five minutes."
                );
                std::process::exit(0)
            }
            _ => return Err(format!("Unknown argument: {arg}").into()),
        }
    }
    Ok(options)
}

fn apply_bootstrap(
    options: &mut Options,
    envelope: t3_contracts::DesktopBackendBootstrap,
) -> Result<(), Box<dyn std::error::Error>> {
    // These services are still explicit parity gaps. Refuse requested external
    // side effects until their implementations exist; don't silently claim them.
    if envelope.tailscale_serve_enabled {
        return Err(
            "Desktop bootstrap requests Tailscale Serve; this service is not ported yet".into(),
        );
    }
    if [
        envelope.otlp_traces_url.as_ref(),
        envelope.otlp_metrics_url.as_ref(),
        envelope.otlp_logs_url.as_ref(),
    ]
    .into_iter()
    .flatten()
    .flatten()
    .any(|url| !url.is_empty())
    {
        return Err(
            "Desktop bootstrap requests OTLP export; this service is not ported yet".into(),
        );
    }
    if envelope.desktop_browser_fd.is_some() || envelope.desktop_browser_control_fd.is_some() {
        return Err("Desktop bootstrap browser IPC is not ported yet".into());
    }
    if !options.overrides.contains("--host") {
        options.host = envelope.host.clone();
    }
    if !options.overrides.contains("--port") {
        options.port = envelope.port.0 as u16;
    }
    if !options.overrides.contains("--mode") {
        options.mode = "desktop".into();
    }
    if !options.overrides.contains("--state-dir") {
        if let Some(home) = envelope
            .t3_home
            .as_ref()
            .and_then(|home| home.as_ref())
            .filter(|home| !home.trim().is_empty())
        {
            options.state_dir = PathBuf::from(home.trim()).join("userdata");
        }
    }
    if !options.overrides.contains("--desktop-telemetry-fd") {
        options.desktop_telemetry_fd = envelope
            .desktop_telemetry_fd
            .map(|fd| i32::try_from(fd.0))
            .transpose()?;
    }
    if !options.overrides.contains("--desktop-telemetry-control-fd") {
        options.desktop_telemetry_control_fd = envelope
            .desktop_telemetry_control_fd
            .map(|fd| i32::try_from(fd.0))
            .transpose()?;
    }
    options.bootstrap = Some(envelope);
    Ok(())
}
#[cfg(unix)]
fn acquire_bootstrap(
    options: &mut Options,
) -> Result<Option<std::os::fd::OwnedFd>, Box<dyn std::error::Error>> {
    let Some(fd) = options.bootstrap_fd else {
        return Ok(None);
    };
    // This is called before Tokio, SQLite, logging, or any other service opens
    // descriptors. The supervisor explicitly transfers ownership to this process.
    let Some(mut input) =
        (unsafe { t3_server::bootstrap::BootstrapInput::acquire_transferred(fd) })?
    else {
        return Ok(None);
    };
    if let Some(envelope) = input.read_retaining(std::time::Duration::from_millis(1000))? {
        apply_bootstrap(options, envelope)?;
    }
    Ok(Some(input.into_descriptor()))
}
#[cfg(not(unix))]
fn acquire_bootstrap(options: &mut Options) -> Result<Option<()>, Box<dyn std::error::Error>> {
    if options.bootstrap_fd.is_some() {
        return Err(
            "Inherited desktop bootstrap descriptors are not supported on this platform yet".into(),
        );
    }
    Ok(None)
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
    fn envelope() -> t3_contracts::DesktopBackendBootstrap {
        serde_json::from_value(json!({"mode":"desktop","noBrowser":true,"port":3773,"host":"127.0.0.1","t3Home":" /tmp/bootstrap-home ","desktopBootstrapToken":"fixture-IPC","tailscaleServeEnabled":false,"tailscaleServePort":443})).unwrap()
    }
    #[test]
    fn bootstrap_startup_precedence_keeps_explicit_flags_then_environment_then_envelope() {
        let mut defaults = parse_options(Vec::new(), |_| None).unwrap();
        apply_bootstrap(&mut defaults, envelope()).unwrap();
        assert_eq!(
            (
                defaults.mode.as_str(),
                defaults.host.as_str(),
                defaults.port
            ),
            ("desktop", "127.0.0.1", 3773)
        );
        assert_eq!(
            defaults.state_dir,
            PathBuf::from("/tmp/bootstrap-home/userdata")
        );
        let mut environment = parse_options(Vec::new(), |key| match key {
            "T3CODE_HOME" => Some(" /tmp/env-home ".into()),
            "T3CODE_PORT" => Some("3775".into()),
            "T3CODE_HOST" => Some("localhost".into()),
            "T3CODE_MODE" => Some("web".into()),
            "T3CODE_BOOTSTRAP_FD" => Some("7".into()),
            _ => None,
        })
        .unwrap();
        apply_bootstrap(&mut environment, envelope()).unwrap();
        assert_eq!(
            (
                environment.mode.as_str(),
                environment.host.as_str(),
                environment.port,
                environment.bootstrap_fd
            ),
            ("web", "localhost", 3775, Some(7))
        );
        assert_eq!(
            environment.state_dir,
            PathBuf::from("/tmp/env-home/userdata")
        );
        let mut explicit = parse_options(
            [
                "--state-dir",
                "/tmp/explicit-state",
                "--port",
                "0",
                "--bootstrap-fd",
                "9",
            ]
            .map(String::from),
            |key| match key {
                "T3CODE_HOME" => Some("/tmp/env-home".into()),
                "T3CODE_BOOTSTRAP_FD" => Some("7".into()),
                _ => None,
            },
        )
        .unwrap();
        apply_bootstrap(&mut explicit, envelope()).unwrap();
        assert_eq!(explicit.state_dir, PathBuf::from("/tmp/explicit-state"));
        assert_eq!(explicit.port, 0);
        assert_eq!(explicit.bootstrap_fd, Some(9));
    }
    #[cfg(unix)]
    #[test]
    fn bootstrap_shared_channel_is_cloned_after_all_caller_numbers_are_validated() {
        use std::os::fd::AsRawFd;
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let descriptor: std::os::fd::OwnedFd = socket.into();
        let mut options = parse_options(Vec::new(), |_| None).unwrap();
        options.desktop_telemetry_fd = Some(descriptor.as_raw_fd());
        options.desktop_telemetry_control_fd = options.desktop_telemetry_fd;
        let descriptors = adopt_desktop_descriptors(&options, Some(descriptor)).unwrap();
        assert_ne!(
            descriptors.input.unwrap().as_raw_fd(),
            descriptors.control.unwrap().as_raw_fd()
        );
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let descriptor: std::os::fd::OwnedFd = socket.into();
        options.desktop_telemetry_fd = Some(descriptor.as_raw_fd());
        options.desktop_telemetry_control_fd = Some(i32::MAX);
        assert!(adopt_desktop_descriptors(&options, Some(descriptor)).is_err());
    }

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
            bootstrap_fd: None,
            bootstrap: None,
            overrides: Default::default(),
        };
        let descriptors = adopt_desktop_descriptors(&options, None).unwrap();
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
fn adopt_desktop_descriptors(
    options: &Options,
    #[cfg(unix)] bootstrap_owner: Option<std::os::fd::OwnedFd>,
    #[cfg(not(unix))] _bootstrap_owner: Option<()>,
) -> std::io::Result<DesktopDescriptors> {
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        // Called at process entry, before Tokio or any service can open/reuse
        // descriptors. These handles are explicitly transferred by the caller.
        let validate = |fd: i32| -> std::io::Result<()> {
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
            Ok(())
        };
        // Validate every caller-provided number before cloning can allocate an
        // FD that accidentally makes a previously unavailable channel look real.
        for fd in [
            options.desktop_telemetry_fd,
            options.desktop_telemetry_control_fd,
        ]
        .into_iter()
        .flatten()
        {
            validate(fd)?;
        }
        let adopt = |fd: i32| -> std::io::Result<OwnedFd> {
            if let Some(owner) = bootstrap_owner
                .as_ref()
                .filter(|owner| owner.as_raw_fd() == fd)
            {
                owner.try_clone()
            } else {
                Ok(unsafe { OwnedFd::from_raw_fd(fd) })
            }
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
    if let Some(result) = t3_server::agent_device_launcher::dispatch(std::env::args_os().skip(1)) {
        match result {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
    if let Some(result) = t3_server::acp_mcp_bridge::dispatch(std::env::args_os().skip(1)) {
        match result {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
    let mut options = options()?;
    let bootstrap_owner = acquire_bootstrap(&mut options)?;
    let descriptors = adopt_desktop_descriptors(&options, bootstrap_owner)?;
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
    let loopback = !t3_server::auth::is_remote_reachable_host(&options.host);
    let policy = if loopback {
        if options.mode == "desktop" {
            "desktop-managed-local"
        } else {
            "loopback-browser"
        }
    } else {
        "remote-reachable"
    };
    let cookie_name = if loopback {
        format!("t3_session_{}_{}", options.port, &instance_hash[..12])
    } else {
        format!("t3_session_{}", &instance_hash[..12])
    };
    let mut auth = AuthService::new(
        store.clone(),
        secret(&state_dir.join("rust-signing-secret"))?,
        cookie_name,
        policy.into(),
    )?;
    if options.mode == "desktop" {
        auth = auth.with_desktop_mode();
    }
    if let Some(bootstrap) = &options.bootstrap {
        auth = auth.with_desktop_bootstrap(
            bootstrap.desktop_bootstrap_token.clone(),
            bootstrap.desktop_bootstrap_secret.clone(),
            chrono::Utc::now(),
        );
    }
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
        store.clone(),
    )
    .await?;
    if config.is_none() {
        config = Some(native_config.snapshot.clone());
    }
    let listener = tokio::net::TcpListener::bind((options.host.as_str(), options.port)).await?;
    let mcp_registry = t3_server::mcp_sessions::McpSessionRegistry::new(
        environment_id.parse()?,
        Some(listener.local_addr()?),
        std::sync::Arc::new(|| chrono::Utc::now().timestamp_millis()),
        t3_server::mcp_sessions::DEFAULT_LIVENESS_WINDOW_MS,
    );
    let native_executable = std::env::current_exe()?;
    native_config
        .providers
        .set_mcp_sessions(t3_server::provider_mcp::ProviderMcpSessions::new(
            mcp_registry.clone(),
            native_executable.clone(),
        ));
    let execution = t3_server::execution::ExecutionService::try_start_with_lease(
        store.clone(),
        native_config.providers.clone(),
        runtime_lease,
    )?;
    let telemetry = t3_server::native_telemetry::NativeTelemetryClient::new(
        t3_server::native_telemetry::NativeTelemetryOptions::host(
            std::env::current_dir()?,
            options
                .bootstrap
                .as_ref()
                .and_then(|bootstrap| bootstrap.resource_monitor_path.as_ref())
                .map(|path| PathBuf::from(path.as_str())),
        ),
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
    let host_resources = t3_server::host_resources::HostResources::new(
        t3_server::host_resources::HostResourcesOptions::host(std::sync::Arc::new(|| {
            chrono::Utc::now().timestamp_millis()
        })),
    );
    let registry = t3_server::resource_ports::TerminalRegistry::default();
    let discovery = t3_server::resource_discovery::PortDiscovery::new(
        t3_server::resource_discovery::PortDiscoveryOptions::host(os, registry.clone())?,
    );
    let mut terminal_options = t3_server::terminal_manager::TerminalManagerOptions::host(
        state_dir.join("logs").join("terminals"),
        &native_config.settings,
    );
    let managed_paths = t3_server::acp_registry_path::ManagedBinaryDirectories {
        cache_dir: state_dir.join("caches"),
        tools_dir: state_dir.join("tools"),
        platform: os.into(),
        architecture: arch.into(),
    };
    terminal_options.managed_directories = Some(std::sync::Arc::new(move || {
        let managed_paths = managed_paths.clone();
        Box::pin(async move { managed_paths.directories().await })
    }));
    t3_server::resource_discovery::configure_terminal_tracking(
        &mut terminal_options,
        telemetry.clone(),
        registry,
    );
    let terminals = t3_server::terminal_manager::TerminalManager::new(terminal_options).await?;
    let settings_service = native_config.settings_service.clone();
    let settings_runtime = if let Some(service) = &settings_service {
        Some(
            t3_server::server_settings_runtime::SettingsRuntime::start(
                service,
                native_config.providers.clone(),
                std::env::current_dir()?,
                terminals.clone(),
                desktop.clone(),
                options.mode.clone(),
            )
            .await?,
        )
    } else {
        None
    };
    let background = if let Some(service) = &settings_service {
        Some(
            t3_server::background_policy::BackgroundPolicy::start(
                service.clone(),
                desktop.clone(),
                std::sync::Arc::new(|| chrono::Utc::now().timestamp_millis()),
            )
            .await?,
        )
    } else {
        None
    };
    let device_hosts = t3_server::device_host_resolver::DeviceHostResolver::new(Default::default());
    let provider_auth = t3_server::provider_auth_service::ProviderAuthService::new(
        native_config.providers.clone(),
        std::env::current_dir()?,
        state_dir.join("caches"),
        execution.authentication_stop(),
    );
    let devices = if let Some(settings) = &settings_service {
        let tools = t3_server::device_toolchain::DeviceToolchain::new(
            t3_server::device_toolchain::ToolchainOptions::new(state_dir.clone()),
        );
        let host = t3_server::local_device_host::LocalDeviceHost::new(
            t3_server::local_device_host::LocalDeviceHostOptions::host(state_dir.clone()),
            tools,
        );
        Some(
            t3_server::device_service::DeviceService::new(settings.clone(), host)
                .await
                .map_err(|error| {
                    std::io::Error::other(serde_json::to_value(error).unwrap().to_string())
                })?,
        )
    } else {
        None
    };
    let mcp = t3_server::mcp_http::McpHttpService::new(
        mcp_registry.clone(),
        devices
            .as_ref()
            .map(|devices| t3_server::mcp_device::McpDeviceTools {
                devices: devices.clone(),
                store: store.clone(),
                state_dir: state_dir.clone(),
                executable: native_executable.clone(),
            }),
    )
    .with_controls(t3_server::mcp_control::McpControlTools {
        store: store.clone(),
        execution: Some(execution.clone()),
        clock: std::sync::Arc::new(chrono::Utc::now),
    });
    let state = ApiState {
        settings: settings_service.clone(),
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
        host_resources: Some(host_resources.clone()),
        background: background.clone(),
        device_hosts: Some(device_hosts.clone()),
        devices: devices.clone(),
        provider_auth: Some(provider_auth.clone()),
    };
    tracing::info!(address=%listener.local_addr()?,state_dir=%state_dir.display(),"native server listening");
    let stopping_mcp = mcp.clone();
    axum::serve(listener, router(state).merge(mcp.router()))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            stopping_mcp.shutdown().await;
        })
        .await?;
    mcp.shutdown().await;
    if let Some(background) = background {
        background.shutdown().await;
    }
    device_hosts.shutdown().await;
    if let Some(devices) = devices {
        devices.shutdown().await;
    }
    provider_auth.shutdown().await;
    host_resources.shutdown().await;
    if let Some(runtime) = settings_runtime {
        runtime.shutdown().await;
    }
    execution.shutdown().await;
    terminals.shutdown().await;
    discovery.shutdown().await;
    tokio::join!(resources.shutdown(), desktop.shutdown());
    telemetry.shutdown().await;
    if let Some(settings) = settings_service {
        settings.shutdown().await;
    }
    Ok(())
}
