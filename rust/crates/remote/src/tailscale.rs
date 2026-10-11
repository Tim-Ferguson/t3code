//! Tailscale helpers (`packages/tailscale/src/tailscale.ts`): reading the node's status, exposing
//! the server on the tailnet with `tailscale serve`, and probing that endpoint. The server calls
//! these at startup when `--tailscale-serve` is on, and when listing advertised endpoints.
//! The executable is injected so tests never run a real `tailscale`.

use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncReadExt;

pub const DEFAULT_TAILSCALE_SERVE_PORT: u16 = 443;
pub const TAILSCALE_STATUS_TIMEOUT: Duration = Duration::from_millis(1_500);
const TAILSCALE_SERVE_TIMEOUT: Duration = Duration::from_secs(10);
const TAILSCALE_PROBE_TIMEOUT: Duration = Duration::from_millis(2_500);

/// `tailscale` is a real executable everywhere (`tailscale.exe` on Windows), so it is always
/// spawned directly, never through a shell.
pub fn tailscale_command_for_platform(windows: bool) -> &'static str {
    if windows {
        "tailscale.exe"
    } else {
        "tailscale"
    }
}

/// Failure kinds named without quoting the CLI. stderr can contain auth keys (`tskey-...`) and node
/// names, and these labels are logged, so anything unrecognised becomes `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StderrDiagnostic {
    NoExistingHandler,
    NotLoggedIn,
    PermissionDenied,
    Unknown,
}

impl StderrDiagnostic {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoExistingHandler => "no-existing-handler",
            Self::NotLoggedIn => "not-logged-in",
            Self::PermissionDenied => "permission-denied",
            Self::Unknown => "unknown",
        }
    }
}

/// Classifies stderr into a safe label, dropping the text itself; `None` for empty stderr.
pub fn stderr_diagnostic_of(stderr: &str) -> Option<StderrDiagnostic> {
    if stderr.trim().is_empty() {
        return None;
    }
    let lower = stderr.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    Some(if has(&["handler does not exist"]) {
        StderrDiagnostic::NoExistingHandler
    } else if has(&["not logged in", "logged out", "needs login", "need login"]) {
        StderrDiagnostic::NotLoggedIn
    } else if has(&[
        "permission denied",
        "access denied",
        "must be root",
        "operation not permitted",
    ]) {
        StderrDiagnostic::PermissionDenied
    } else {
        StderrDiagnostic::Unknown
    })
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TailscaleError {
    #[error("Failed to spawn tailscale {subcommand}.")]
    Spawn { subcommand: &'static str },
    #[error("Failed to read output from tailscale {subcommand}.")]
    Output { subcommand: &'static str },
    #[error("tailscale {subcommand} exited with code {exit_code}.")]
    Exit {
        subcommand: &'static str,
        argument_count: usize,
        exit_code: i32,
        stdout_length: Option<usize>,
        /// UTF-16 length, as the original counts.
        stderr_length: usize,
        stderr_diagnostic: Option<StderrDiagnostic>,
    },
    #[error("tailscale {subcommand} timed out after {timeout_ms}ms.")]
    Timeout {
        subcommand: &'static str,
        timeout_ms: u128,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Failed to decode tailscale status JSON.")]
pub struct StatusParseError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleStatus {
    pub magic_dns_name: Option<String>,
    pub tailnet_ipv4_addresses: Vec<String>,
}

/// Whether an address is in the tailnet range, `100.64.0.0/10`.
pub fn is_tailscale_ipv4_address(address: &str) -> bool {
    let parts: Vec<&str> = address.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    let numbers: Vec<Option<i64>> = parts.iter().map(|p| crate::util::js_parse_int(p)).collect();
    if numbers
        .iter()
        .any(|n| !n.is_some_and(|n| (0..=255).contains(&n)))
    {
        return false;
    }
    let (first, second) = (numbers[0].unwrap(), numbers[1].unwrap());
    first == 100 && (64..=127).contains(&second)
}

fn normalize_magic_dns_name(status: &Value) -> Option<String> {
    let name = status.get("Self")?.get("DNSName")?.as_str()?;
    let normalized = name.trim().strip_suffix('.').unwrap_or(name.trim());
    (!normalized.is_empty()).then(|| normalized.to_owned())
}

fn decode_status_json(raw: &str) -> Result<Value, StatusParseError> {
    let value: Value = serde_json::from_str(raw).map_err(|_| StatusParseError)?;
    // `Self` is optional but must be an object when present.
    match value.get("Self") {
        None | Some(Value::Object(_)) if value.is_object() => Ok(value),
        _ => Err(StatusParseError),
    }
}

pub fn parse_tailscale_magic_dns_name(raw: &str) -> Result<Option<String>, StatusParseError> {
    Ok(normalize_magic_dns_name(&decode_status_json(raw)?))
}

pub fn parse_tailscale_status(raw: &str) -> Result<TailscaleStatus, StatusParseError> {
    let parsed = decode_status_json(raw)?;
    let addresses = parsed
        .get("Self")
        .and_then(|s| s.get("TailscaleIPs"))
        .and_then(Value::as_array)
        .map(|ips| {
            ips.iter()
                .filter_map(Value::as_str)
                .filter(|ip| is_tailscale_ipv4_address(ip))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(TailscaleStatus {
        magic_dns_name: normalize_magic_dns_name(&parsed),
        tailnet_ipv4_addresses: addresses,
    })
}

/// `https://<name>/` or `https://<name>:<port>/` for a non-default serve port.
pub fn build_tailscale_https_base_url(
    magic_dns_name: &str,
    serve_port: Option<u16>,
) -> Result<String, url::ParseError> {
    let mut url = url::Url::parse(&format!("https://{magic_dns_name}"))?;
    let serve_port = serve_port.unwrap_or(DEFAULT_TAILSCALE_SERVE_PORT);
    if serve_port != DEFAULT_TAILSCALE_SERVE_PORT {
        let _ = url.set_port(Some(serve_port));
    }
    url.set_path("/");
    Ok(url.to_string())
}

/// Runs the `tailscale` binary. `executable` is a command name or an absolute path.
#[derive(Debug, Clone)]
pub struct Tailscale {
    pub executable: String,
    /// `tailscale status` gets this long; a test shortens it.
    pub status_timeout: Duration,
}

impl Tailscale {
    pub fn new(windows: bool) -> Self {
        Self::with_executable(tailscale_command_for_platform(windows))
    }

    pub fn with_executable(executable: impl Into<String>) -> Self {
        Self {
            executable: executable.into(),
            status_timeout: TAILSCALE_STATUS_TIMEOUT,
        }
    }

    async fn run(
        &self,
        subcommand: &'static str,
        args: &[String],
        timeout: Duration,
        capture_stdout: bool,
    ) -> Result<String, TailscaleError> {
        let work = async {
            let mut child = tokio::process::Command::new(&self.executable)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|_| TailscaleError::Spawn { subcommand })?;
            let mut stdout = child.stdout.take().expect("piped stdout");
            let mut stderr = child.stderr.take().expect("piped stderr");
            let (out, err, status) = tokio::join!(
                async {
                    let mut buf = Vec::new();
                    stdout.read_to_end(&mut buf).await.map(|_| buf)
                },
                async {
                    let mut buf = Vec::new();
                    stderr.read_to_end(&mut buf).await.map(|_| buf)
                },
                child.wait()
            );
            let (Ok(out), Ok(err), Ok(status)) = (out, err, status) else {
                return Err(TailscaleError::Output { subcommand });
            };
            let stdout = String::from_utf8_lossy(&out).into_owned();
            let stderr = String::from_utf8_lossy(&err).into_owned();
            let exit_code = status.code().unwrap_or(-1);
            if exit_code != 0 {
                return Err(TailscaleError::Exit {
                    subcommand,
                    argument_count: args.len(),
                    exit_code,
                    stdout_length: capture_stdout.then(|| stdout.encode_utf16().count()),
                    stderr_length: stderr.encode_utf16().count(),
                    stderr_diagnostic: stderr_diagnostic_of(&stderr),
                });
            }
            Ok(stdout)
        };
        tokio::time::timeout(timeout, work)
            .await
            .unwrap_or(Err(TailscaleError::Timeout {
                subcommand,
                timeout_ms: timeout.as_millis(),
            }))
    }

    /// `tailscale status --json`, parsed. Tailscale absent or down is the common case, not an error
    /// worth surfacing: callers treat any failure as "no tailnet address".
    pub async fn read_status(&self) -> Result<TailscaleStatus, TailscaleReadError> {
        let stdout = self
            .run(
                "status",
                &["status".into(), "--json".into()],
                self.status_timeout,
                true,
            )
            .await?;
        Ok(parse_tailscale_status(&stdout)?)
    }

    /// `tailscale serve --bg --https=<servePort> http://<localHost>:<localPort>`
    pub async fn ensure_serve(
        &self,
        local_port: u16,
        serve_port: Option<u16>,
        local_host: Option<&str>,
    ) -> Result<(), TailscaleError> {
        let serve_port = serve_port.unwrap_or(DEFAULT_TAILSCALE_SERVE_PORT);
        let local_host = local_host.unwrap_or("127.0.0.1");
        let args = vec![
            "serve".to_owned(),
            "--bg".to_owned(),
            format!("--https={serve_port}"),
            format!("http://{local_host}:{local_port}"),
        ];
        self.run("serve", &args, TAILSCALE_SERVE_TIMEOUT, false)
            .await
            .map(|_| ())
    }

    /// `tailscale serve --https=<servePort> off`
    pub async fn disable_serve(&self, serve_port: Option<u16>) -> Result<(), TailscaleError> {
        let serve_port = serve_port.unwrap_or(DEFAULT_TAILSCALE_SERVE_PORT);
        let args = vec![
            "serve".to_owned(),
            format!("--https={serve_port}"),
            "off".to_owned(),
        ];
        self.run("serve", &args, TAILSCALE_SERVE_TIMEOUT, false)
            .await
            .map(|_| ())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TailscaleReadError {
    #[error(transparent)]
    Command(#[from] TailscaleError),
    #[error(transparent)]
    Parse(#[from] StatusParseError),
}

/// Whether `<baseUrl>/.well-known/t3/environment` answers 2xx within the timeout. Any failure is
/// `false`.
pub async fn probe_tailscale_https_endpoint(base_url: &str, timeout: Option<Duration>) -> bool {
    let Ok(url) =
        url::Url::parse(base_url).and_then(|base| base.join("/.well-known/t3/environment"))
    else {
        return false;
    };
    let call = async { reqwest::Client::new().get(url).send().await };
    match tokio::time::timeout(timeout.unwrap_or(TAILSCALE_PROBE_TIMEOUT), call).await {
        Ok(Ok(response)) => response.status().is_success(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{"Self":{"DNSName":"desktop.tail.ts.net.","TailscaleIPs":["100.100.100.100","fd7a:115c:a1e0::1"]}}"#;

    #[test]
    fn detects_tailnet_ipv4_addresses() {
        assert!(is_tailscale_ipv4_address("100.64.0.1"));
        assert!(is_tailscale_ipv4_address("100.127.255.254"));
        assert!(!is_tailscale_ipv4_address("100.128.0.1"));
        assert!(!is_tailscale_ipv4_address("192.168.1.44"));
        // parseInt reads the leading digits, so the original accepts these too.
        assert!(is_tailscale_ipv4_address("100.64.0.1abc"));
        assert!(!is_tailscale_ipv4_address("100.64.0"));
        assert!(!is_tailscale_ipv4_address("100.64.0.256"));
    }

    #[test]
    fn parses_magic_dns_and_status_facts() {
        assert_eq!(
            parse_tailscale_magic_dns_name(STATUS).unwrap().as_deref(),
            Some("desktop.tail.ts.net")
        );
        assert_eq!(parse_tailscale_magic_dns_name("{}").unwrap(), None);
        let status = parse_tailscale_status(STATUS).unwrap();
        assert_eq!(
            status.magic_dns_name.as_deref(),
            Some("desktop.tail.ts.net")
        );
        assert_eq!(status.tailnet_ipv4_addresses, ["100.100.100.100"]);
        assert_eq!(parse_tailscale_status("{not-json"), Err(StatusParseError));
        assert_eq!(
            StatusParseError.to_string(),
            "Failed to decode tailscale status JSON."
        );
    }

    #[test]
    fn builds_clean_https_base_urls() {
        assert_eq!(
            build_tailscale_https_base_url("desktop.tail.ts.net", None).unwrap(),
            "https://desktop.tail.ts.net/"
        );
        assert_eq!(
            build_tailscale_https_base_url("desktop.tail.ts.net", Some(8443)).unwrap(),
            "https://desktop.tail.ts.net:8443/"
        );
    }

    #[test]
    fn classifies_stderr_without_quoting_it() {
        let secret = "tskey-auth-secret-token-value";
        assert_eq!(
            stderr_diagnostic_of(&format!("not logged in {secret}")),
            Some(StderrDiagnostic::NotLoggedIn)
        );
        assert_eq!(
            stderr_diagnostic_of(&format!("serve permission denied {secret}")),
            Some(StderrDiagnostic::PermissionDenied)
        );
        assert_eq!(
            stderr_diagnostic_of("serve handler does not exist"),
            Some(StderrDiagnostic::NoExistingHandler)
        );
        assert_eq!(
            stderr_diagnostic_of(&format!(
                "something novel went wrong for node fluffy-badger {secret}"
            )),
            Some(StderrDiagnostic::Unknown)
        );
        assert_eq!(stderr_diagnostic_of("  \n"), None);
    }
}
