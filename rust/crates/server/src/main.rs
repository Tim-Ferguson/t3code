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
    /// `--fixture`, for testing only: see `t3_server::fixture`.
    fixture: bool,
    /// The validated `T3CODE_FIXTURE_GITHUB_API_ORIGIN`; only ever set together with `fixture`.
    fixture_github_api_origin: Option<String>,
    /// `import --from`: the original's T3 home (or its `userdata` directory).
    from: Option<PathBuf>,
    /// `--no-import`: start with an empty store even though the original's database is beside it.
    no_import: bool,
    /// `--strict-import`: a failed import ends the process with an error, as it must when nothing watches the status page.
    strict_import: bool,
    /// `--tailscale-serve` / `T3CODE_TAILSCALE_SERVE`, else the desktop bootstrap's; off when none says.
    tailscale_serve: Option<bool>,
    /// `--tailscale-serve-port` / `T3CODE_TAILSCALE_SERVE_PORT`, else the bootstrap's; 443 when none says.
    tailscale_serve_port: Option<u16>,
    /// `connect <subcommand>`: the T3 Connect commands, which run against the state directory and do not start a server.
    connect: Option<(t3_server::connect_cli::Subcommand, t3_server::connect_cli::Flags)>,
}
/// What `Config.Boolean` reads.
fn parse_boolean(value: &str) -> Option<bool> {
    match value.trim().to_lowercase().as_str() {
        "true" | "yes" | "on" | "1" | "y" => Some(true),
        "false" | "no" | "off" | "0" | "n" => Some(false),
        _ => None,
    }
}
/// What `Config.Port` reads.
fn parse_port(value: &str) -> Option<u16> {
    value.trim().parse::<u16>().ok().filter(|port| *port >= 1)
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
        fixture: false,
        fixture_github_api_origin: None,
        from: None,
        no_import: false,
        strict_import: false,
        tailscale_serve: None,
        tailscale_serve_port: None,
        connect: None,
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
    if let Some(value) = env("T3CODE_TAILSCALE_SERVE") {
        options.tailscale_serve = Some(parse_boolean(&value).ok_or("T3CODE_TAILSCALE_SERVE requires a boolean")?);
    }
    if let Some(value) = env("T3CODE_TAILSCALE_SERVE_PORT") {
        options.tailscale_serve_port = Some(parse_port(&value).ok_or("T3CODE_TAILSCALE_SERVE_PORT must be between 1 and 65535")?);
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
            "serve" | "pair" | "import" => options.command = arg,
            // The original's `start` refuses to start over a running server; the runtime lock does that here.
            "start" => options.command = "serve".into(),
            // Nothing opens a browser, so there is nothing for the flag to switch off.
            "--no-browser" => {}
            _ if arg.starts_with("--no-browser=") => {
                parse_boolean(&arg["--no-browser=".len()..]).ok_or("--no-browser requires a boolean")?;
            }
            "connect" => {
                options.command = arg;
                options.connect = Some((t3_server::connect_cli::Subcommand::Connect, Default::default()));
            }
            "login" | "link" | "publish" | "status" | "unlink" | "logout" if options.connect.is_some() => {
                if let Some((subcommand, _)) = &mut options.connect {
                    *subcommand = t3_server::connect_cli::Subcommand::parse(&arg).expect("a connect subcommand");
                }
            }
            "--headless" | "--publish-only" | "--disable" | "--json" if options.connect.is_some() => {
                if let Some((_, flags)) = &mut options.connect {
                    match arg.as_str() {
                        "--headless" => flags.headless = true,
                        "--publish-only" => flags.publish_only = true,
                        "--disable" => flags.disable = true,
                        _ => flags.json = true,
                    }
                }
            }
            // The original's flag for the T3 home; the state directory is `<home>/userdata`.
            "--base-dir" => {
                options.state_dir = PathBuf::from(args.next().ok_or("--base-dir requires a path")?).join("userdata");
                options.overrides.insert("--state-dir".to_owned());
            }
            "--tailscale-serve" => options.tailscale_serve = Some(true),
            "--tailscale-serve-port" => options.tailscale_serve_port = Some(parse_port(&args.next().ok_or("--tailscale-serve-port requires a port")?).ok_or("--tailscale-serve-port must be between 1 and 65535")?),
            _ if arg.starts_with("--tailscale-serve=") => options.tailscale_serve = Some(parse_boolean(&arg["--tailscale-serve=".len()..]).ok_or("--tailscale-serve requires a boolean")?),
            _ if arg.starts_with("--tailscale-serve-port=") => options.tailscale_serve_port = Some(parse_port(&arg["--tailscale-serve-port=".len()..]).ok_or("--tailscale-serve-port must be between 1 and 65535")?),
            "--from" => options.from = Some(PathBuf::from(args.next().ok_or("--from requires the original's T3 home")?)),
            "--no-import" => options.no_import = true,
            "--strict-import" => options.strict_import = true,
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
            "--fixture" => options.fixture = true,
            "--help" | "-h" => {
                println!(
                    "t3-server [serve|pair|import|connect [login|link|publish|status|unlink|logout]] [--state-dir PATH] [--from T3_HOME] [--no-import] [--host ADDRESS] [--port PORT] [--settings JSON] [--config JSON] [--assets DIRECTORY] [--mode web|desktop] [--tailscale-serve] [--tailscale-serve-port PORT] [--bootstrap-fd FD] [--desktop-telemetry-fd FD] [--desktop-telemetry-control-fd FD] [--fixture]\nNative port in progress. State defaults to .t3-rust under the current directory.\npair prints a scoped, one-use browser pairing credential valid for five minutes.\nconnect sets up T3 Connect (--headless; link --publish-only; publish --disable; status --json; --base-dir PATH names the T3 home).\nimport --from T3_HOME --state-dir DIR imports the original server's database into DIR (see rust/README.md); serve does the same on first start when DIR holds the original's database and no store of this server, unless --no-import.\n--fixture is for testing only (rust/tools/browser_fixture): the server offers no editors, and T3CODE_FIXTURE_GITHUB_API_ORIGIN (a loopback origin such as http://127.0.0.1:PORT) sends GitHub requests to a fake GitHub API there. The variable is an error without this flag."
                );
                std::process::exit(0)
            }
            _ => return Err(format!("Unknown argument: {arg}").into()),
        }
    }
    options.fixture_github_api_origin = t3_server::fixture::github_api_origin(options.fixture, env(t3_server::fixture::GITHUB_API_ORIGIN_VARIABLE))?;
    Ok(options)
}

/// `resolveStaticDir`: without `--assets`, the web app bundled beside the executable (`client/index.html`) is served, so a server
/// started the way the remote SSH launch starts it (`serve --host --port --base-dir`) answers its readiness probe at `/`.
fn bundled_assets(executable: &Path) -> Option<PathBuf> {
    let directory = executable.parent()?.join("client");
    directory.join("index.html").is_file().then_some(directory)
}

fn apply_bootstrap(
    options: &mut Options,
    envelope: t3_contracts::DesktopBackendBootstrap,
) -> Result<(), Box<dyn std::error::Error>> {
    // These services are still explicit parity gaps. Refuse requested external
    // side effects until their implementations exist; don't silently claim them.
    // Tailscale Serve: a flag or the environment decides before the envelope does.
    if options.tailscale_serve.is_none() {
        options.tailscale_serve = Some(envelope.tailscale_serve_enabled);
    }
    if options.tailscale_serve_port.is_none() {
        options.tailscale_serve_port = u16::try_from(envelope.tailscale_serve_port.0).ok().filter(|port| *port >= 1);
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
/// Where the original reads the envelope on Windows is a pipe handle Node maps to a descriptor number (`new Net.Socket({fd})`); Rust
/// has no such mapping for an inherited handle, so only the delivery its desktop uses across the WSL boundary is understood here:
/// `--bootstrap-fd 0`, the envelope as the first line of standard input. Another descriptor is refused.
#[cfg(not(unix))]
fn acquire_bootstrap(options: &mut Options) -> Result<Option<()>, Box<dyn std::error::Error>> {
    match options.bootstrap_fd {
        None => Ok(None),
        Some(0) => {
            use std::io::BufRead;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            if !line.trim().is_empty() {
                apply_bootstrap(options, serde_json::from_str(line.trim())?)?;
            }
            Ok(None)
        }
        Some(_) => Err("On this platform the desktop bootstrap is read from standard input only (--bootstrap-fd 0)".into()),
    }
}

fn secret(path: &Path) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    t3_server::cli_commands::signing_secret(path)
}

fn identity(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let identity = uuid::Uuid::new_v4().to_string();
    publish_once(path, identity.as_bytes())?;
    let identity = fs::read_to_string(path)?.trim().to_owned();
    uuid::Uuid::parse_str(&identity)?;
    Ok(identity)
}

fn publish_once(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    t3_server::cli_commands::publish_once(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn envelope() -> t3_contracts::DesktopBackendBootstrap {
        serde_json::from_value(json!({"mode":"desktop","noBrowser":true,"port":3773,"host":"127.0.0.1","t3Home":" /tmp/bootstrap-home ","desktopBootstrapToken":"fixture-IPC","tailscaleServeEnabled":false,"tailscaleServePort":443})).unwrap()
    }
    #[test]
    fn the_originals_start_command_and_no_browser_flag_are_understood() {
        let started = parse_options(["start".to_owned(), "--no-browser".to_owned(), "--port".to_owned(), "0".to_owned()], |_| None).unwrap();
        assert_eq!((started.command.as_str(), started.port), ("serve", 0));
        assert!(parse_options(["serve".to_owned(), "--no-browser=false".to_owned()], |_| None).is_ok());
        assert!(parse_options(["serve".to_owned(), "--no-browser=maybe".to_owned()], |_| None).is_err());
    }
    #[test]
    fn the_web_app_bundled_beside_the_executable_is_found_only_when_it_has_an_index() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("t3-server");
        assert_eq!(bundled_assets(&executable), None);
        fs::create_dir(directory.path().join("client")).unwrap();
        assert_eq!(bundled_assets(&executable), None, "an empty directory is no web app");
        fs::write(directory.path().join("client").join("index.html"), "<html></html>").unwrap();
        assert_eq!(bundled_assets(&executable), Some(directory.path().join("client")));
    }
    #[test]
    fn the_fixture_origin_is_an_error_without_the_flag_and_is_checked_with_it() {
        let origin = |value: &'static str| move |key: &str| (key == "T3CODE_FIXTURE_GITHUB_API_ORIGIN").then(|| value.to_owned());
        // A normal start asks for nothing and changes nothing.
        let normal = parse_options(Vec::new(), |_| None).unwrap();
        assert!(!normal.fixture && normal.fixture_github_api_origin.is_none());
        // The variable alone is refused, not ignored.
        let error = parse_options(Vec::new(), origin("http://127.0.0.1:47851")).err().unwrap().to_string();
        assert!(error.contains("--fixture"), "{error}");
        assert!(parse_options(["serve".to_owned()], origin("http://127.0.0.1:47851")).is_err());
        // With the flag a loopback origin is taken, and nothing else.
        let fixture = parse_options(["--fixture".to_owned()], origin("http://[::1]:47851")).unwrap();
        assert_eq!(fixture.fixture_github_api_origin.as_deref(), Some("http://[::1]:47851"));
        assert!(parse_options(["--fixture".to_owned()], |_| None).unwrap().fixture_github_api_origin.is_none());
        for bad in ["https://127.0.0.1:1", "http://api.github.com:80", "http://127.0.0.1:1/x", "http://user@127.0.0.1:1", "http://127.0.0.1"] {
            assert!(parse_options(["--fixture".to_owned()], origin(bad)).is_err(), "{bad}");
        }
    }
    fn tailscale_envelope(enabled: bool, port: u16) -> t3_contracts::DesktopBackendBootstrap {
        serde_json::from_value(json!({"mode":"desktop","noBrowser":true,"port":3773,"host":"127.0.0.1","desktopBootstrapToken":"fixture-IPC","tailscaleServeEnabled":enabled,"tailscaleServePort":port})).unwrap()
    }
    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| pairs.iter().find(|(name, _)| *name == key).map(|(_, value)| (*value).to_owned())
    }
    #[test]
    fn tailscale_serve_follows_the_flag_then_the_environment_then_the_envelope() {
        // Off at 443 when nothing says.
        let mut none = parse_options(Vec::new(), |_| None).unwrap();
        assert_eq!((none.tailscale_serve, none.tailscale_serve_port), (None, None));
        apply_bootstrap(&mut none, tailscale_envelope(false, 443)).unwrap();
        assert_eq!((none.tailscale_serve, none.tailscale_serve_port), (Some(false), Some(443)));
        // The envelope's request is accepted now (it was refused while the service was not ported).
        let mut envelope = parse_options(Vec::new(), |_| None).unwrap();
        apply_bootstrap(&mut envelope, tailscale_envelope(true, 8443)).unwrap();
        assert_eq!((envelope.tailscale_serve, envelope.tailscale_serve_port), (Some(true), Some(8443)));
        // The environment beats the envelope; the flag beats both.
        let mut environment = parse_options(Vec::new(), env_of(&[("T3CODE_TAILSCALE_SERVE", "false"), ("T3CODE_TAILSCALE_SERVE_PORT", "9443")])).unwrap();
        apply_bootstrap(&mut environment, tailscale_envelope(true, 8443)).unwrap();
        assert_eq!((environment.tailscale_serve, environment.tailscale_serve_port), (Some(false), Some(9443)));
        let mut flags = parse_options(["--tailscale-serve", "--tailscale-serve-port", "10443"].map(String::from), env_of(&[("T3CODE_TAILSCALE_SERVE", "false"), ("T3CODE_TAILSCALE_SERVE_PORT", "9443")])).unwrap();
        apply_bootstrap(&mut flags, tailscale_envelope(false, 8443)).unwrap();
        assert_eq!((flags.tailscale_serve, flags.tailscale_serve_port), (Some(true), Some(10443)));
        let explicit = parse_options(["--tailscale-serve=false", "--tailscale-serve-port=1"].map(String::from), |_| None).unwrap();
        assert_eq!((explicit.tailscale_serve, explicit.tailscale_serve_port), (Some(false), Some(1)));
        for bad in [["--tailscale-serve-port", "0"], ["--tailscale-serve-port", "70000"], ["--tailscale-serve=maybe", "--x"]] {
            assert!(parse_options(bad.map(String::from), |_| None).is_err(), "{bad:?}");
        }
        assert!(parse_options(Vec::new(), env_of(&[("T3CODE_TAILSCALE_SERVE", "perhaps")])).is_err());
        assert!(parse_options(Vec::new(), env_of(&[("T3CODE_TAILSCALE_SERVE_PORT", "0")])).is_err());
        for (text, expected) in [("true", true), ("YES", true), ("on", true), ("1", true), ("y", true), ("false", false), ("No", false), ("off", false), ("0", false), ("n", false)] {
            assert_eq!(parse_boolean(text), Some(expected), "{text}");
        }
    }
    #[test]
    fn the_connect_commands_take_their_subcommand_and_flags() {
        use t3_server::connect_cli::Subcommand::*;
        let parsed = |args: &[&str]| parse_options(args.iter().map(|arg| arg.to_string()), |_| None).unwrap();
        assert!(parsed(&["serve"]).connect.is_none());
        let connect = parsed(&["connect", "--headless"]);
        assert_eq!(connect.command, "connect");
        let (subcommand, flags) = connect.connect.unwrap();
        assert!(subcommand == Connect && flags.headless && !flags.publish_only);
        for (name, expected) in [("login", Login), ("link", Link), ("publish", Publish), ("status", Status), ("unlink", Unlink), ("logout", Logout)] {
            assert!(parsed(&["connect", name]).connect.unwrap().0 == expected, "{name}");
        }
        let link = parsed(&["connect", "link", "--publish-only", "--headless"]).connect.unwrap();
        assert!(link.1.publish_only && link.1.headless);
        assert!(parsed(&["connect", "publish", "--disable"]).connect.unwrap().1.disable);
        assert!(parsed(&["connect", "status", "--json"]).connect.unwrap().1.json);
        // The original's flag for the T3 home.
        let home = parsed(&["connect", "status", "--base-dir", "/tmp/t3-home"]);
        assert_eq!(home.state_dir, PathBuf::from("/tmp/t3-home/userdata"));
        // These words mean something only after `connect`.
        assert!(parse_options(["status".to_owned()], |_| None).is_err());
        assert!(parse_options(["--headless".to_owned()], |_| None).is_err());
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
            fixture: false,
            fixture_github_api_origin: None,
            from: None,
            no_import: false,
            strict_import: false,
            tailscale_serve: None,
            tailscale_serve_port: None,
            connect: None,
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
    // `--version`, `pair`, `auth`, `project` and the hidden helpers of the remote launch are not the server.
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if let Some(outcome) = t3_server::cli_commands::dispatch(&arguments, &|key| std::env::var(key).ok()) {
        {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(outcome.stdout.as_bytes());
            let _ = stdout.flush();
        }
        eprint!("{}", outcome.stderr);
        std::process::exit(outcome.code);
    }
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
    let parent = parent_at_start();
    let mut options = options()?;
    let bootstrap_owner = acquire_bootstrap(&mut options)?;
    let descriptors = adopt_desktop_descriptors(&options, bootstrap_owner)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(options, descriptors, parent))
}
/// The signals that stop the server, registered once it is ready to be stopped gracefully: SIGINT, SIGTERM (what the original
/// desktop stops its backend with, followed by SIGKILL after a grace period) and SIGHUP (the original does not catch it, so its
/// process just ends; here the stop is as graceful as the others). Until they are registered a signal ends the process at once,
/// as it does the original's while it starts.
struct StopSignals {
    #[cfg(unix)]
    streams: Option<[tokio::signal::unix::Signal; 3]>,
}
impl StopSignals {
    fn install() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let streams = (|| Some([signal(SignalKind::interrupt()).ok()?, signal(SignalKind::terminate()).ok()?, signal(SignalKind::hangup()).ok()?]))();
            Self { streams }
        }
        #[cfg(not(unix))]
        Self {}
    }
    async fn wait(self) {
        #[cfg(unix)]
        if let Some([mut interrupt, mut terminate, mut hangup]) = self.streams {
            tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {}, _ = hangup.recv() => {} }
            return;
        }
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// The process that started this one, noted before anything else happens.
#[cfg(unix)]
fn parent_at_start() -> i32 {
    unsafe { libc::getppid() }
}
#[cfg(not(unix))]
fn parent_at_start() -> i32 {
    0
}

/// Completes when the process that started this one has gone. The original has no such watch (its desktop ends a backend with
/// SIGTERM and a grace period, and a backend left behind by a killed desktop keeps running); a server this desktop started
/// holds the state directory's runtime lock, so an orphan would keep the next start from opening the same data. A parent that is
/// gone leaves the child to another process (init or a subreaper), so a changed parent id is the signal. `parent` is the id the
/// process had when it began: a parent that died while the server was still starting is already gone by the time this runs.
#[cfg(unix)]
async fn parent_gone(parent: i32) {
    loop {
        if unsafe { libc::getppid() } != parent {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}
#[cfg(not(unix))]
async fn parent_gone(_parent: i32) {
    std::future::pending::<()>().await
}

/// Why the server is stopping, for the log.
async fn stop_requested(signals: StopSignals, watch_parent: Option<i32>) {
    tokio::select! {
        _ = signals.wait() => tracing::info!("stop requested by signal"),
        _ = async { match watch_parent { Some(parent) => parent_gone(parent).await, None => std::future::pending::<()>().await } } => tracing::warn!("the supervising process is gone; stopping"),
    }
}

/// How long open connections get to finish once a stop is asked for.
const CONNECTION_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Serves until a stop is requested, then lets connections finish for [`CONNECTION_GRACE`] and no longer: a client that keeps
/// its WebSocket open must not keep the server from releasing the state directory.
async fn serve_until_stopped(listener: tokio::net::TcpListener, app: axum::Router, signals: StopSignals, watch_parent: Option<i32>, then: impl std::future::Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
    let stopping = std::sync::Arc::new(tokio::sync::Notify::new());
    let notified = stopping.clone();
    let service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
    let serving = axum::serve(listener, service).with_graceful_shutdown(async move {
        stop_requested(signals, watch_parent).await;
        then.await;
        notified.notify_one();
    });
    tokio::select! {
        result = serving => result,
        _ = async { stopping.notified().await; tokio::time::sleep(CONNECTION_GRACE).await } => {
            tracing::warn!("connections were still open after the grace period; closing them");
            Ok(())
        }
    }
}

/// One machine-readable line on standard output once the socket is bound (the original has no such signal: its desktop polls
/// `/.well-known/t3/environment`). `T3_SERVER_READY ` then a JSON object; documented in rust/README.md.
fn announce_ready(listener: &tokio::net::TcpListener) -> std::io::Result<()> {
    use std::io::Write;
    let address = listener.local_addr()?;
    let host = if address.ip().is_unspecified() { "127.0.0.1".to_owned() } else if address.is_ipv6() { format!("[{}]", address.ip()) } else { address.ip().to_string() };
    let line = json!({"pid":std::process::id(),"host":address.ip().to_string(),"port":address.port(),"origin":format!("http://{host}:{}",address.port())});
    let mut out = std::io::stdout().lock();
    writeln!(out, "T3_SERVER_READY {line}")?;
    out.flush()
}

/// RUST-PORT EXTENSION (`import_status`): the import failed, so there is no store to serve. Answer the descriptor, the
/// status block and the web assets, so the client can show what went wrong; the process ends 0 when it is stopped.
async fn serve_import_failed(options: &Options, state_dir: &Path, message: String) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("{message}");
    let router = t3_server::import_status::failed_router(t3_server::import_status::FailedMode {
        environment: t3_server::import_status::failed_environment(state_dir, env!("CARGO_PKG_VERSION")),
        assets: options.assets.clone().map(fs::canonicalize).transpose()?,
        status: t3_server::import_status::failed(&message),
    });
    let listener = tokio::net::TcpListener::bind((options.host.as_str(), options.port)).await?;
    let signals = StopSignals::install();
    announce_ready(&listener)?;
    serve_until_stopped(listener, router, signals, None, async {}).await?;
    Ok(())
}
async fn run(
    options: Options,
    descriptors: DesktopDescriptors,
    parent: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    // Before anything uses the process's source control host.
    if options.fixture {
        t3_server::fixture::install(options.fixture_github_api_origin.as_deref())?;
    }
    fs::create_dir_all(&options.state_dir)?;
    let state_dir = fs::canonicalize(&options.state_dir)?;
    if let Some((subcommand, flags)) = options.connect {
        // T3 Connect's commands work on the state directory and never start a server.
        let environment = t3_server::connect_cli::CliEnvironment::production(state_dir);
        return match t3_server::connect_cli::run(subcommand, flags, &environment).await {
            Ok(()) => Ok(()),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1)
            }
        };
    }
    if options.command == "import" {
        let from = options.from.clone().ok_or("import requires --from, the original's T3 home")?;
        let code = tokio::task::spawn_blocking(move || t3_server::state_import::run_command(from, state_dir)).await?;
        std::process::exit(code);
    }
    // First start beside the original's database: its state is imported once, or the server does not start.
    if !options.no_import && options.command == "serve" {
        let directory = state_dir.clone();
        match tokio::task::spawn_blocking(move || t3_server::state_import::automatic(&directory)).await? {
            Ok(Some(report)) => t3_server::state_import::print_report(&report),
            Ok(None) => {}
            Err(error) => {
                let message = format!("Importing the original T3 Code state failed, so the server did not start: {error} (start with --no-import to use an empty store instead)");
                if options.strict_import {
                    return Err(message.into());
                }
                return serve_import_failed(&options, &state_dir, message).await;
            }
        }
    }
    let store = Store::open(state_dir.join("rust-state.sqlite"))?;
    t3_server::secret_requests::start_sweeper(&store);
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
    let environment = json!({"environmentId":environment_id,"label":"T3 Code Rust","platform":{"os":os,"arch":arch},"serverVersion":env!("CARGO_PKG_VERSION"),"orchestrationProtocolVersion":2,"capabilities":t3_server::environment_capabilities::advertised()});
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
    let mcp_sessions = t3_server::provider_mcp::ProviderMcpSessions::new(
        mcp_registry.clone(),
        native_executable.clone(),
    );
    native_config.providers.set_mcp_sessions(mcp_sessions.clone());
    // Commit messages and pull request content for stacked git actions are written by the settings'
    // provider; thread titles and branch names reach the same service through the effect worker.
    t3_server::git_workflow_rpc::install_text_generation(std::sync::Arc::new(t3_server::text_generation_service::TextGenerationService::new(
        native_config.providers.clone(),
        store.clone(),
    )));
    let execution = t3_server::execution::ExecutionService::try_start_with_lease(
        store.clone(),
        native_config.providers.clone(),
        runtime_lease,
    )?;
    execution.workspace().attach_mcp(mcp_sessions);
    // Started after execution recovery, so a run that comes due launches into a recovered runtime.
    let scheduled_tasks = t3_server::scheduled_tasks::ScheduledTasks::start(&store, execution.clone())?;
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
    let provider_auth = t3_server::provider_auth_service::ProviderAuthService::for_server(
        &execution,
        &native_config.providers,
        std::env::current_dir()?,
        &state_dir,
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
    // Agents T3 Code did not launch sign in to `/mcp` through this authorization server (its signing key is a secret of the state
    // directory, so client ids survive a restart).
    let mcp_oauth = std::sync::Arc::new(t3_server::mcp_oauth::McpOAuth::new(auth.clone(), &t3_server::server_secret_store::ServerSecretStore::open(state_dir.join("secrets"))?)?);
    t3_server::mcp_oauth::install(&store, mcp_oauth.clone());
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
    .with_clients(mcp_oauth.clone(), environment_id.parse()?)
    .with_controls(t3_server::mcp_control::McpControlTools {
        store: store.clone(),
        execution: Some(execution.clone()),
        clock: std::sync::Arc::new(chrono::Utc::now),
        services: t3_server::mcp_control::McpServices {
            settings: settings_service.clone(),
            environment: Some(environment.clone()),
            background: background.clone(),
            terminals: Some(terminals.clone()),
        },
    });
    // Watched pull requests wake their threads' agents; one timer, armed only while a watch exists.
    let pull_request_watcher = {
        let host = t3_server::source_control_host::Host::process_default();
        let env = t3_server::pull_request_service::Env { host: host.clone().with_settings(settings_service.clone()), store: store.clone() };
        let watcher = t3_server::pull_request_watcher::PullRequestWatcher::new(execution.clone(), env, t3_server::pull_request_service::PullRequestService::shared().clone(), host.clock().clone());
        watcher.spawn_timer();
        watcher
    };
    // Every linked pull request's snapshot is kept current once a minute.
    {
        let host = t3_server::source_control_host::Host::process_default();
        let env = t3_server::pull_request_service::Env { host: host.with_settings(settings_service.clone()), store: store.clone() };
        t3_server::pull_request_sync::PullRequestSync::new(execution.clone(), env, t3_server::pull_request_service::PullRequestService::shared().clone()).spawn_timer();
    }
    // The server accepts commands from here: a client following the lifecycle learns it.
    let cwd = std::env::current_dir()?;
    t3_server::server_lifecycle::publish(&store, "welcome", json!({"environment":environment,"cwd":cwd,"projectName":cwd.file_name().map_or("project".to_owned(), |name| name.to_string_lossy().into_owned())}));
    t3_server::server_lifecycle::publish(&store, "ready", json!({"at":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),"environment":environment}));
    // T3 Connect: built now that the listener has an address, started when the routes serve (below).
    let local_port = listener.local_addr()?.port();
    let connect = t3_server::connect::Connect::new(t3_server::connect::ConnectOptions::production(
        store.clone(),
        auth.clone(),
        environment.clone(),
        &state_dir,
        std::sync::Arc::new(move || Some(local_port)),
    ))?;
    t3_server::connect::install(&store, connect.clone());
    // The origin a CLI on this machine reaches the running server at (`t3 connect unlink`).
    let runtime_state = state_dir.join("server-runtime.json");
    t3_server::connect::persist_runtime_state(&runtime_state, &options.host, listener.local_addr()?.port());
    let tailscale = options.tailscale_serve.unwrap_or(false).then(|| {
        t3_server::connect::TailscaleServe::activate(std::sync::Arc::new(t3_remote::tailscale::Tailscale::new(cfg!(windows))), local_port, options.tailscale_serve_port.unwrap_or(443))
    });
    let state = ApiState {
        settings: settings_service.clone(),
        store,
        auth,
        environment,
        config,
        cors_origins: None,
        assets: options.assets.or_else(|| std::env::current_exe().ok().and_then(|exe| bundled_assets(&exe))).map(fs::canonicalize).transpose()?,
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
    let signals = StopSignals::install();
    announce_ready(&listener)?;
    connect.start();
    serve_until_stopped(listener, router(state).merge(mcp.router()), signals, (options.mode == "desktop").then_some(parent), async move { stopping_mcp.shutdown().await }).await?;
    // The server's finalizers: the link (releasing a CLI-managed tunnel unless an update is replacing this server), the
    // connector, and Tailscale Serve if this server configured it.
    connect.shutdown().await;
    if let Some(tailscale) = tailscale {
        tailscale.deactivate().await;
    }
    let _ = fs::remove_file(&runtime_state);
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
    pull_request_watcher.shutdown().await;
    scheduled_tasks.shutdown().await;
    execution.shutdown().await;
    terminals.shutdown().await;
    discovery.shutdown().await;
    tokio::join!(resources.shutdown(), desktop.shutdown());
    telemetry.shutdown().await;
    if let Some(settings) = settings_service {
        settings.shutdown().await;
    }
    t3_server::keybindings_service::KeybindingsService::shutdown_shared().await;
    Ok(())
}
