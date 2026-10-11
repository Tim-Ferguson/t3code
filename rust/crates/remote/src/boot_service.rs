//! The pure part of the background service installer (`cloud/bootService.ts`): the systemd unit and
//! launchd plist it writes, reading the T3 home back out of them, and the wording of the problems
//! `t3 service status` reports. Running `systemctl` / `launchctl`, the pinned runtime and the
//! update launcher (`pinnedRuntime.ts`, `serviceLauncherClient.ts`, `selfUpdate.ts`) are not
//! ported: they belong to the service/update feature, not to remote access.

pub const BOOT_SERVICE_NAME: &str = "t3code";
pub const BOOT_SERVICE_UNIT_FILE: &str = "t3code.service";
/// The `.service` suffix keeps the label distinct from the desktop app's bundle id
/// (`com.t3tools.t3code`), so launchd and TCC records never collide.
pub const BOOT_SERVICE_LAUNCHD_LABEL: &str = "com.t3tools.t3code.service";
pub const BOOT_SERVICE_PLIST_FILE: &str = "com.t3tools.t3code.service.plist";
pub const BOOT_SERVICE_UNIT_ENV: &str = "T3_BOOT_SERVICE_UNIT";
/// The file in the logs dir that receives the service's stdout and stderr.
pub const BOOT_SERVICE_LOG_FILE: &str = "boot-service.log";

/// systemd expands `%` specifiers, including in unquoted append-log paths.
fn escape_systemd_specifiers(value: &str) -> String {
    value.replace('%', "%%")
}

fn quote_systemd_value(value: &str) -> String {
    let escaped = escape_systemd_specifiers(value);
    if escaped
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\'))
    {
        format!("\"{}\"", escaped.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        escaped
    }
}

/// Reads `T3CODE_HOME` back out of a rendered unit or plist; only values this file writes are
/// expected (`bootServiceBaseDirOf`).
pub fn boot_service_base_dir_of(contents: &str) -> Option<String> {
    if let Some(line) = contents
        .lines()
        .find_map(|l| l.strip_prefix("Environment=T3CODE_HOME="))
    {
        let raw = line.trim();
        let unquoted = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
            raw[1..raw.len() - 1]
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        } else {
            raw.to_owned()
        };
        return Some(unquoted.replace("%%", "%"));
    }
    let marker = "<key>T3CODE_HOME</key>";
    let after = &contents[contents.find(marker)? + marker.len()..];
    let after = after.trim_start();
    let value = after.strip_prefix("<string>")?;
    let value = &value[..value.find('<')?];
    Some(
        value
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

pub struct BootServicePlan {
    /// What the service manager executes.
    pub program: Vec<String>,
    pub base_dir: String,
    pub log_path: String,
    pub unit_path: String,
}

/// The systemd user unit. Service units cannot rely on the user's shell or PATH.
pub fn render_boot_service_unit(plan: &BootServicePlan) -> String {
    [
        "[Unit]".to_owned(),
        "Description=T3 Code server".to_owned(),
        "StartLimitIntervalSec=300".to_owned(),
        "StartLimitBurst=5".to_owned(),
        String::new(),
        "[Service]".to_owned(),
        "Type=simple".to_owned(),
        "WorkingDirectory=%h".to_owned(),
        format!(
            "Environment=T3CODE_HOME={}",
            quote_systemd_value(&plan.base_dir)
        ),
        format!("Environment={BOOT_SERVICE_UNIT_ENV}={BOOT_SERVICE_UNIT_FILE}"),
        format!(
            "ExecStart={}",
            plan.program
                .iter()
                .map(|p| quote_systemd_value(p))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        // Let the launcher mark an explicit stop before it signals the server.
        "KillMode=mixed".to_owned(),
        // Agent tool calls run as children of the server; keep running if the kernel kills one.
        "OOMPolicy=continue".to_owned(),
        "Restart=always".to_owned(),
        "RestartSec=5".to_owned(),
        format!(
            "StandardOutput=append:{}",
            escape_systemd_specifiers(&plan.log_path)
        ),
        format!(
            "StandardError=append:{}",
            escape_systemd_specifiers(&plan.log_path)
        ),
        String::new(),
        "[Install]".to_owned(),
        "WantedBy=default.target".to_owned(),
        String::new(),
    ]
    .join("\n")
}

/// Plist values are XML text nodes; only these three characters need escaping.
fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The launchd agent plist. Launch agents cannot rely on the user's shell or PATH.
pub fn render_boot_service_plist(
    plan: &BootServicePlan,
    home_dir: &str,
    environment_path: &str,
) -> String {
    let mut lines: Vec<String> = vec![
        r#"<?xml version="1.0" encoding="UTF-8"?>"#.to_owned(),
        r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#.to_owned(),
        r#"<plist version="1.0">"#.to_owned(),
        "<dict>".to_owned(),
        "  <key>Label</key>".to_owned(),
        format!("  <string>{BOOT_SERVICE_LAUNCHD_LABEL}</string>"),
        "  <key>ProgramArguments</key>".to_owned(),
        "  <array>".to_owned(),
    ];
    lines.extend(
        plan.program
            .iter()
            .map(|a| format!("    <string>{}</string>", escape_xml_text(a))),
    );
    lines.extend([
        "  </array>".to_owned(),
        "  <key>EnvironmentVariables</key>".to_owned(),
        "  <dict>".to_owned(),
        "    <key>PATH</key>".to_owned(),
        format!("    <string>{}</string>", escape_xml_text(environment_path)),
        "    <key>T3CODE_HOME</key>".to_owned(),
        format!("    <string>{}</string>", escape_xml_text(&plan.base_dir)),
        format!("    <key>{BOOT_SERVICE_UNIT_ENV}</key>"),
        format!("    <string>{BOOT_SERVICE_PLIST_FILE}</string>"),
        "  </dict>".to_owned(),
        "  <key>WorkingDirectory</key>".to_owned(),
        format!("  <string>{}</string>", escape_xml_text(home_dir)),
        "  <key>RunAtLoad</key>".to_owned(),
        "  <true/>".to_owned(),
        "  <key>KeepAlive</key>".to_owned(),
        "  <true/>".to_owned(),
        "  <key>ThrottleInterval</key>".to_owned(),
        "  <integer>5</integer>".to_owned(),
        "  <key>ExitTimeOut</key>".to_owned(),
        "  <integer>90</integer>".to_owned(),
        "  <key>ProcessType</key>".to_owned(),
        "  <string>Interactive</string>".to_owned(),
        "  <key>StandardOutPath</key>".to_owned(),
        format!("  <string>{}</string>", escape_xml_text(&plan.log_path)),
        "  <key>StandardErrorPath</key>".to_owned(),
        format!("  <string>{}</string>", escape_xml_text(&plan.log_path)),
        "</dict>".to_owned(),
        "</plist>".to_owned(),
        String::new(),
    ]);
    lines.join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootServiceProblem {
    UserManagerUnavailable,
    LingerUnavailable,
    LingerDisabled,
    ServiceDisabled,
    ServiceStopped,
    RestartPending,
}

impl BootServiceProblem {
    pub fn code(self) -> &'static str {
        match self {
            Self::UserManagerUnavailable => "user-manager-unavailable",
            Self::LingerUnavailable => "linger-unavailable",
            Self::LingerDisabled => "linger-disabled",
            Self::ServiceDisabled => "service-disabled",
            Self::ServiceStopped => "service-stopped",
            Self::RestartPending => "restart-pending",
        }
    }

    /// These codes and recovery steps are documented in `docs/user/background-service.md`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::UserManagerUnavailable => {
                "Cannot reach the systemd user manager. Run `systemctl --user status` in a login session for the service user. Install your distribution's systemd user-session support if it is missing; do not run T3 with sudo."
            }
            Self::LingerUnavailable => {
                "Cannot check whether this user can run services after logout. Run `loginctl show-user \"$(id -un)\" --property=Linger` and check that systemd-logind is available."
            }
            Self::LingerDisabled => {
                "Lingering is disabled. T3 Code will stop when your last login session ends and will not start at boot. Run `sudo loginctl enable-linger \"$(id -un)\"` on this machine, then retry the service command as your normal user."
            }
            Self::ServiceDisabled => {
                "The service is not enabled to start automatically. Run `t3 service install` to repair it."
            }
            Self::ServiceStopped => {
                "The service is not running. Check the service log and `systemctl --user status t3code.service`, then run `t3 service install`."
            }
            Self::RestartPending => {
                "A newer version is installed but the service is still running the previous one. Run `t3 service restart` to switch."
            }
        }
    }

    /// `[code] description`, as the prerequisite error prints it.
    pub fn message(self) -> String {
        format!("[{}] {}", self.code(), self.describe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(base_dir: &str) -> BootServicePlan {
        BootServicePlan {
            program: vec!["/opt/t3/bin/t3".into(), "service-launcher".into()],
            base_dir: base_dir.into(),
            log_path: "/home/u/.t3/logs/boot-service.log".into(),
            unit_path: "/home/u/.config/systemd/user/t3code.service".into(),
        }
    }

    #[test]
    fn unit_and_plist_round_trip_the_home_directory() {
        for base in [
            "/home/u/.t3",
            "/home/u/My Dir/.t3",
            "/home/u/100%/\"q\"\\x",
            "/home/u/a&b<c>",
        ] {
            assert_eq!(
                boot_service_base_dir_of(&render_boot_service_unit(&plan(base))).as_deref(),
                Some(base),
                "{base}"
            );
            assert_eq!(
                boot_service_base_dir_of(&render_boot_service_plist(
                    &plan(base),
                    "/home/u",
                    "/usr/bin"
                ))
                .as_deref(),
                Some(base),
                "{base}"
            );
        }
        assert_eq!(boot_service_base_dir_of("nothing here"), None);
    }

    #[test]
    fn unit_quotes_arguments_with_spaces() {
        let mut p = plan("/h");
        p.program = vec!["/opt/my t3/bin/t3".into(), "run".into()];
        assert!(render_boot_service_unit(&p).contains("ExecStart=\"/opt/my t3/bin/t3\" run\n"));
    }
}
