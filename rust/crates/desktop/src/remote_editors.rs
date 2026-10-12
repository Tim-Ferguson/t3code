//! Which editors that take a remote link are installed on this machine (`probeRemoteEditors` of `ipc/methods/window.ts`, over
//! `resolveEditorCommand` of `packages/shared/src/editor.ts`). The server cannot say: when a window shows an environment on another
//! machine, what matters is what runs here. Reading only: nothing is run, files and folders are looked at.
use crate::remote_open::REMOTE_CAPABLE;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Macos,
    Windows,
    Linux,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Os::Macos
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }
}

/// What the lookup reads of this computer; a test names a machine of its own.
pub trait Machine {
    fn os(&self) -> Os;
    fn var(&self, name: &str) -> Option<String>;
    /// Whether `command` runs: a path to an executable file, or a name found in a folder of `PATH`.
    fn command_available(&self, command: &str) -> bool;
    /// The names in a folder; none when it cannot be read.
    fn list(&self, directory: &Path) -> Vec<String>;
    /// The folders applications are installed in for everyone: macOS `/Applications`.
    fn applications(&self) -> Vec<PathBuf>;
    /// Linux folders searched after `~/.local/bin`.
    fn linux_directories(&self) -> Vec<PathBuf>;
}

/// What an editor is called in the folder it is installed in, where that differs from its label (`installNames`).
fn install_names(id: &str, label: &str) -> Vec<String> {
    match id {
        "vscode" => vec!["Visual Studio Code".into()],
        "vscode-insiders" => vec!["Visual Studio Code - Insiders".into()],
        _ => vec![label.to_owned()],
    }
}

/// The editors of the remote-link table and the commands that start them (`EDITORS`).
const EDITORS: [(&str, &str, &[&str]); 5] = [
    ("cursor", "Cursor", &["cursor"]),
    ("vscode", "VS Code", &["code"]),
    ("vscode-insiders", "VS Code Insiders", &["code-insiders"]),
    ("vscodium", "VSCodium", &["codium"]),
    ("zed", "Zed", &["zed", "zeditor"]),
];

/// `<name> <version>.app`, where the version is digits and dots. A plain prefix match would let stable VS Code resolve to the
/// Insiders bundle.
fn is_versioned_bundle(entry: &str, name: &str) -> bool {
    let Some(rest) = entry.strip_prefix(&format!("{name} ")) else { return false };
    let Some(version) = rest.strip_suffix(".app") else { return false };
    version.chars().next().is_some_and(|c| c.is_ascii_digit()) && version.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// `resolveEditorCommand` for an editor that is not a JetBrains one: on `PATH`, else in the folders its installer uses.
fn is_installed(id: &str, label: &str, commands: &[&str], machine: &dyn Machine) -> bool {
    if commands.iter().any(|command| machine.command_available(command)) {
        return true;
    }
    let home = machine.var("HOME").map(PathBuf::from);
    let names = install_names(id, label);
    let command = commands[0];
    let mut candidates: Vec<PathBuf> = Vec::new();
    match machine.os() {
        Os::Macos => {
            let roots: Vec<PathBuf> = home.iter().map(|home| home.join("Applications")).chain(machine.applications()).collect();
            for root in roots {
                let mut bundles: Vec<String> = names.iter().map(|name| format!("{name}.app")).collect();
                for entry in machine.list(&root) {
                    if names.iter().any(|name| entry == format!("{name}.app") || is_versioned_bundle(&entry, name)) && !bundles.contains(&entry) {
                        bundles.push(entry);
                    }
                }
                for bundle in bundles {
                    let contents = root.join(&bundle).join("Contents");
                    if id == "zed" {
                        candidates.push(contents.join("MacOS").join("cli"));
                    } else {
                        candidates.push(contents.join("Resources/app/bin").join(command));
                        candidates.push(contents.join("Resources/app/bin/code"));
                    }
                }
            }
        }
        Os::Windows => {
            let mut roots: Vec<PathBuf> = machine.var("LOCALAPPDATA").map(|dir| vec![PathBuf::from(dir).join("Programs")]).unwrap_or_default();
            roots.extend(["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"].iter().filter_map(|name| machine.var(name).filter(|dir| !dir.is_empty()).map(PathBuf::from)));
            let name = match id {
                "vscode" => "Microsoft VS Code",
                "vscode-insiders" => "Microsoft VS Code Insiders",
                _ => label,
            };
            for root in &roots {
                candidates.push(root.join(name).join("resources/app/bin").join(format!("{command}.cmd")));
                candidates.push(root.join(name).join("resources/app/bin/code.cmd"));
                candidates.push(root.join(name).join("bin").join(format!("{command}.cmd")));
                candidates.push(root.join(name).join("bin/code.cmd"));
                if id == "zed" {
                    candidates.push(root.join(name).join("bin/zed.exe"));
                    candidates.push(root.join(name).join("zed.exe"));
                }
            }
        }
        Os::Linux => {
            let dirs: Vec<PathBuf> = home.iter().map(|home| home.join(".local/bin")).chain(machine.linux_directories()).collect();
            for dir in dirs {
                for name in commands {
                    candidates.push(dir.join(name));
                }
            }
        }
    }
    candidates.iter().any(|candidate| machine.command_available(&candidate.to_string_lossy()))
}

/// `probeRemoteEditors`: the ids, in the table's order, of the remote-capable editors this machine has.
pub fn probe(machine: &dyn Machine) -> Vec<String> {
    REMOTE_CAPABLE
        .iter()
        .filter(|(id, _)| EDITORS.iter().find(|(known, ..)| known == id).is_some_and(|(id, label, commands)| is_installed(id, label, commands, machine)))
        .map(|(id, _)| (*id).to_owned())
        .collect()
}

/// This computer, with `PATH` as the person's own shell has it: a program started from the Finder gets a bare one.
pub struct Host {
    path: Option<String>,
}

impl Host {
    pub fn new(path: Option<String>) -> Self {
        Self { path: path.or_else(|| std::env::var("PATH").ok()) }
    }
}

#[cfg(unix)]
fn can_execute(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).map(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0).unwrap_or(false)
}
#[cfg(not(unix))]
fn can_execute(_: &Path) -> bool {
    true
}

impl Machine for Host {
    fn os(&self) -> Os {
        Os::current()
    }
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn command_available(&self, command: &str) -> bool {
        let is_file = |path: &Path| std::fs::metadata(path).is_ok_and(|info| info.is_file()) && (cfg!(windows) || can_execute(path));
        let windows_names = |name: &str| -> Vec<String> {
            if cfg!(windows) {
                let extensions = self.var("PATHEXT").filter(|raw| !raw.is_empty()).unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
                std::iter::once(name.to_owned()).chain(extensions.split(';').filter(|extension| !extension.is_empty()).map(|extension| format!("{name}{extension}"))).collect()
            } else {
                vec![name.to_owned()]
            }
        };
        if command.contains('/') || command.contains('\\') {
            return windows_names(command).iter().any(|candidate| is_file(Path::new(candidate)));
        }
        let Some(path) = &self.path else { return false };
        std::env::split_paths(path).filter(|entry| !entry.as_os_str().is_empty()).any(|entry| windows_names(command).iter().any(|name| is_file(&entry.join(name))))
    }
    fn list(&self, directory: &Path) -> Vec<String> {
        std::fs::read_dir(directory).map(|entries| entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
    }
    fn applications(&self) -> Vec<PathBuf> {
        vec![PathBuf::from("/Applications")]
    }
    fn linux_directories(&self) -> Vec<PathBuf> {
        ["/usr/local/bin", "/usr/bin", "/snap/bin"].into_iter().map(PathBuf::from).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// A machine described by what it has: executables at paths (a bare name is `PATH`'s), and folders with names in them.
    struct Fake {
        os: Os,
        vars: BTreeMap<&'static str, String>,
        executables: BTreeSet<String>,
        folders: BTreeMap<PathBuf, Vec<String>>,
        applications: Vec<PathBuf>,
    }
    impl Fake {
        fn new(os: Os) -> Self {
            Self { os, vars: BTreeMap::new(), executables: BTreeSet::new(), folders: BTreeMap::new(), applications: vec![PathBuf::from("/Applications")] }
        }
        fn has(mut self, path: &str) -> Self {
            self.executables.insert(path.to_owned());
            self
        }
    }
    impl Machine for Fake {
        fn os(&self) -> Os {
            self.os
        }
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
        fn command_available(&self, command: &str) -> bool {
            self.executables.contains(command)
        }
        fn list(&self, directory: &Path) -> Vec<String> {
            self.folders.get(directory).cloned().unwrap_or_default()
        }
        fn applications(&self) -> Vec<PathBuf> {
            self.applications.clone()
        }
        fn linux_directories(&self) -> Vec<PathBuf> {
            vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")]
        }
    }

    #[test]
    fn an_editor_on_the_path_is_found_and_the_answer_keeps_the_tables_order() {
        let machine = Fake::new(Os::Linux).has("zeditor").has("code").has("cursor");
        assert_eq!(probe(&machine), ["cursor", "vscode", "zed"], "zed by its second command, in the order of the table and not of the lookup");
        assert_eq!(probe(&Fake::new(Os::Linux)), Vec::<String>::new());
        // A command of another editor is not this one: `code-insiders` is not `code`.
        assert_eq!(probe(&Fake::new(Os::Linux).has("code-insiders")), ["vscode-insiders"]);
        // Editors that cannot take a remote link are not asked about at all.
        assert_eq!(probe(&Fake::new(Os::Linux).has("idea").has("trae")), Vec::<String>::new());
    }

    #[test]
    fn an_editor_off_the_path_is_found_where_its_installer_puts_it() {
        let mut mac = Fake::new(Os::Macos).has("/Users/me/Applications/Cursor.app/Contents/Resources/app/bin/cursor").has("/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code").has("/Applications/Zed.app/Contents/MacOS/cli");
        mac.vars.insert("HOME", "/Users/me".into());
        assert_eq!(probe(&mac), ["cursor", "vscode", "zed"]);

        // Stable VS Code does not take the Insiders bundle, and a versioned bundle is found.
        let mut mac = Fake::new(Os::Macos).has("/Applications/Visual Studio Code - Insiders.app/Contents/Resources/app/bin/code-insiders").has("/Applications/VSCodium 1.99.2.app/Contents/Resources/app/bin/codium");
        mac.vars.insert("HOME", "/Users/me".into());
        mac.folders.insert(PathBuf::from("/Applications"), vec!["Visual Studio Code - Insiders.app".into(), "VSCodium 1.99.2.app".into()]);
        assert_eq!(probe(&mac), ["vscode-insiders", "vscodium"]);

        let mut linux = Fake::new(Os::Linux).has("/home/me/.local/bin/zed").has("/usr/local/bin/code");
        linux.vars.insert("HOME", "/home/me".into());
        assert_eq!(probe(&linux), ["vscode", "zed"]);

        let mut windows = Fake::new(Os::Windows).has("C:\\Users\\me\\AppData\\Local\\Programs\\Microsoft VS Code\\resources/app/bin\\code.cmd").has("C:\\Program Files\\Zed\\zed.exe");
        windows.vars.insert("LOCALAPPDATA", "C:\\Users\\me\\AppData\\Local".into());
        windows.vars.insert("ProgramFiles", "C:\\Program Files".into());
        // The folders are joined with this machine's separator, so a Windows path is only matched on Windows; the structure is what is tested.
        let found = probe(&windows);
        assert!(found.iter().all(|id| id == "vscode" || id == "zed"));
    }

    #[test]
    fn the_versioned_bundle_rule_is_exact() {
        assert!(is_versioned_bundle("VSCodium 1.99.2.app", "VSCodium"));
        assert!(!is_versioned_bundle("VSCodium Insiders.app", "VSCodium"));
        assert!(!is_versioned_bundle("Visual Studio Code - Insiders.app", "Visual Studio Code"));
        assert!(!is_versioned_bundle("VSCodium 1.99.2", "VSCodium"));
    }

    #[cfg(unix)]
    #[test]
    fn the_real_host_looks_for_executables_on_the_given_path_and_nowhere_else() {
        use std::os::unix::fs::PermissionsExt;
        let bin = tempfile::tempdir().unwrap();
        let runnable = bin.path().join("code");
        std::fs::write(&runnable, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&runnable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let flat = bin.path().join("cursor");
        std::fs::write(&flat, "not executable").unwrap();
        std::fs::set_permissions(&flat, std::fs::Permissions::from_mode(0o644)).unwrap();

        let host = Host::new(Some(bin.path().to_string_lossy().into_owned()));
        assert!(host.command_available("code"));
        assert!(!host.command_available("cursor"), "a file without the execute bit is not a command");
        assert!(!host.command_available("codium"));
        assert!(host.command_available(&runnable.to_string_lossy()), "an explicit path is checked as it is");
        assert!(!host.command_available(&bin.path().to_string_lossy()), "a folder is not a command");
        assert_eq!(host.list(bin.path()).len(), 2);
        assert!(host.list(&bin.path().join("missing")).is_empty());
    }
}
