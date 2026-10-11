//! What the environment descriptor says about the machine: its label and its kind (`environment/ServerEnvironmentLabel.ts`,
//! `environment/ServerEnvironmentMachine.ts`). Both are best-effort: every probe may fail, and a failed one is "no signal"
//! (the label then falls through to the hostname and the working directory's name; the kind is left out and a client draws a
//! generic server until the user picks an icon in Settings). The probes sit behind [`HostProbe`], so the decisions are tested
//! against scripted hosts.
use std::time::Duration;
use t3_contracts::EnvironmentMachineKind;

/// The host as the probes see it.
pub trait HostProbe {
    /// Node's platform names: `darwin`, `linux`, `win32`, ...
    fn platform(&self) -> &str;
    fn hostname(&self) -> Option<String>;
    /// A file's text, or none when it is not there or cannot be read.
    fn read_file(&self, path: &str) -> Option<String>;
    /// Whether a file exists; an error is a failure to know (`None`).
    fn file_exists(&self, path: &str) -> Option<bool>;
    /// A command's exit code and standard output; none when it cannot run or times out.
    fn run(&self, command: &str, args: &[&str]) -> Option<(i32, String)>;
}

/// The machine this process runs on.
pub struct SystemProbe;

impl HostProbe for SystemProbe {
    fn platform(&self) -> &str {
        match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        }
    }
    fn hostname(&self) -> Option<String> {
        #[cfg(unix)]
        {
            let mut buffer = [0u8; 256];
            // SAFETY: the buffer is writable for its whole length, and `gethostname` NUL-terminates within it or fails.
            let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
            (status == 0).then(|| String::from_utf8_lossy(&buffer[..buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len())]).into_owned())
        }
        #[cfg(not(unix))]
        {
            std::env::var("COMPUTERNAME").ok()
        }
    }
    fn read_file(&self, path: &str) -> Option<String> {
        std::fs::read(path).ok().map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
    fn file_exists(&self, path: &str) -> Option<bool> {
        std::fs::exists(path).ok()
    }
    fn run(&self, command: &str, args: &[&str]) -> Option<(i32, String)> {
        use std::io::Read;
        let mut child = std::process::Command::new(command).args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().ok()?;
        let mut stdout = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = stdout.read_to_end(&mut text);
            String::from_utf8_lossy(&text).into_owned()
        });
        // Startup-time probes: a tool that hangs costs the descriptor its field, not the server its start.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            match child.try_wait().ok()? {
                Some(status) => break status,
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        Some((status.code().unwrap_or(-1), reader.join().ok()?))
    }
}

fn normalize(value: Option<&str>) -> Option<String> {
    let trimmed = t3_contracts::trim_wire_string(value?);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

// ---- the label

/// `parseMachineInfoValue`: the first `key=value` line of `/etc/machine-info`, unquoted.
fn machine_info_value(raw: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    for line in raw.split(['\n']).map(|line| line.strip_suffix('\r').unwrap_or(line)) {
        let trimmed = t3_contracts::trim_wire_string(line);
        if trimmed.is_empty() || trimmed.starts_with('#') || !trimmed.starts_with(&prefix) {
            continue;
        }
        let value = t3_contracts::trim_wire_string(&trimmed[prefix.len()..]);
        let quoted = value.len() >= 2 && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\'')));
        // A lone quote is not a pair: `"` and `'` alone have length one.
        return if quoted { normalize(Some(&value[1..value.len() - 1])) } else { normalize(Some(value)) };
    }
    None
}

fn run_label_command(probe: &dyn HostProbe, command: &str, args: &[&str]) -> Option<String> {
    let (code, stdout) = probe.run(command, args)?;
    if code != 0 {
        return None;
    }
    normalize(Some(&stdout))
}

fn friendly_host_label(probe: &dyn HostProbe) -> Option<String> {
    match probe.platform() {
        "darwin" => run_label_command(probe, "scutil", &["--get", "ComputerName"]),
        "linux" => {
            let machine_info = match probe.file_exists("/etc/machine-info") {
                Some(true) => probe.read_file("/etc/machine-info"),
                _ => None,
            };
            if let Some(pretty) = normalize(machine_info.as_deref()).and_then(|info| machine_info_value(&info, "PRETTY_HOSTNAME")) {
                return Some(pretty);
            }
            run_label_command(probe, "hostnamectl", &["--pretty"])
        }
        _ => None,
    }
}

/// `resolveServerEnvironmentLabel`: the machine's friendly name, else its hostname, else the working directory's name.
pub fn resolve_label(probe: &dyn HostProbe, cwd_base_name: &str) -> String {
    friendly_host_label(probe)
        .or_else(|| normalize(probe.hostname().as_deref()))
        .or_else(|| normalize(Some(cwd_base_name)))
        .unwrap_or_else(|| "T3 environment".into())
}

// ---- the kind

/// Marketing names and Intel-era model identifiers share these prefixes.
pub fn machine_kind_from_apple_product_name(name: &str) -> Option<EnvironmentMachineKind> {
    let normalized: String = t3_contracts::trim_wire_string(name).to_lowercase().split(char::is_whitespace).collect();
    if normalized.starts_with("macmini") {
        Some(EnvironmentMachineKind::MacMini)
    } else if normalized.starts_with("macstudio") {
        Some(EnvironmentMachineKind::MacStudio)
    } else if normalized.starts_with("macbook") {
        Some(EnvironmentMachineKind::Laptop)
    } else if normalized.starts_with("imac") || normalized.starts_with("macpro") {
        Some(EnvironmentMachineKind::Desktop)
    } else {
        None
    }
}

/// SMBIOS 3.x System Enclosure types. Codes that describe a shape rather than a machine (docking stations, blade enclosures,
/// IoT gateways) fall through to none on purpose.
fn chassis_kind(code: &str) -> Option<EnvironmentMachineKind> {
    use EnvironmentMachineKind::*;
    Some(match code {
        "3" | "4" | "5" | "6" | "7" | "13" | "15" | "16" | "35" => Desktop,
        "8" | "9" | "10" | "14" | "31" | "32" => Laptop,
        "17" | "18" | "19" | "20" | "21" | "22" | "23" | "24" | "28" => Server,
        _ => return None,
    })
}

/// Hypervisors and cloud providers write themselves into the DMI vendor or product strings; any hit means the box is a VM, and a
/// VM reads as "cloud" whatever chassis type the hypervisor fakes.
const VIRTUALIZATION_MARKERS: [&str; 18] = ["qemu", "kvm", "bochs", "vmware", "virtualbox", "innotek", "xen", "parallels", "amazon ec2", "google compute engine", "digitalocean", "hetzner", "linode", "vultr", "scaleway", "openstack", "cloud", "virtual machine"];

pub fn machine_kind_from_dmi(chassis_type: Option<&str>, sys_vendor: Option<&str>, product_name: Option<&str>) -> Option<EnvironmentMachineKind> {
    let product_name = product_name.unwrap_or_default();
    let vendor_and_product = format!("{} {}", sys_vendor.unwrap_or_default(), product_name).to_lowercase();
    if VIRTUALIZATION_MARKERS.iter().any(|marker| vendor_and_product.contains(marker)) {
        return Some(EnvironmentMachineKind::Cloud);
    }
    // Apple hardware booting Linux (Asahi) still reports the Apple product name.
    machine_kind_from_apple_product_name(product_name).or_else(|| chassis_type.and_then(chassis_kind))
}

fn detect_darwin(probe: &dyn HostProbe) -> Option<EnvironmentMachineKind> {
    let run = |command: &str, args: &[&str]| probe.run(command, args).filter(|(code, _)| *code == 0).and_then(|(_, stdout)| normalize(Some(&stdout)));
    // IOKit's `product` node carries the marketing name on Apple silicon; Intel Macs lack it, so `hw.model` is the fallback.
    static PRODUCT_NAME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PRODUCT_NAME.get_or_init(|| regex::Regex::new(r#""product-name"\s*=\s*<"([^"]+)">"#).unwrap());
    let from_product = run("ioreg", &["-rd1", "-n", "product"]).and_then(|ioreg| pattern.captures(&ioreg).map(|captures| captures[1].to_owned())).and_then(|name| machine_kind_from_apple_product_name(&name));
    from_product.or_else(|| run("sysctl", &["-n", "hw.model"]).and_then(|model| machine_kind_from_apple_product_name(&model)))
}

fn detect_linux(probe: &dyn HostProbe) -> Option<EnvironmentMachineKind> {
    let read = |path: &str| probe.read_file(path).and_then(|text| normalize(Some(&text)));
    let kernel_release = read("/proc/sys/kernel/osrelease");
    let (chassis, vendor, product) = (read("/sys/class/dmi/id/chassis_type"), read("/sys/class/dmi/id/sys_vendor"), read("/sys/class/dmi/id/product_name"));
    // WSL exposes Microsoft in its kernel release on both WSL 1 and WSL 2. Check it before DMI because WSL 2 presents as a Hyper-V VM.
    if kernel_release.is_some_and(|release| release.to_lowercase().contains("microsoft")) {
        return Some(EnvironmentMachineKind::Linux);
    }
    machine_kind_from_dmi(chassis.as_deref(), vendor.as_deref(), product.as_deref())
}

/// `detectServerEnvironmentMachineKind`: best-effort hardware detection for the environment icon.
pub fn detect_machine_kind(probe: &dyn HostProbe) -> Option<EnvironmentMachineKind> {
    match probe.platform() {
        "darwin" => detect_darwin(probe),
        "linux" => detect_linux(probe),
        _ => None,
    }
}
