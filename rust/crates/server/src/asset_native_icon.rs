//! Resolves and caches macOS application icons for `native-app-icon` assets (port of
//! `assets/NativeAppIconResolver.ts`). The app comes from the signed claims, never from the
//! request, and the client never sees a host path: it gets a 64px PNG made from the bundle's own
//! `.icns`, kept under the server's cache directory. Anything but macOS has no icons to give.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const ICON_SIZE: u32 = 64;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const RESOLUTION_CACHE_TTL: Duration = Duration::from_secs(3600);

/// The system tools icon lookup runs, by absolute path so a `PATH` entry can never stand in.
#[derive(Clone)]
pub struct Tools {
    pub mdfind: PathBuf,
    pub mdls: PathBuf,
    pub plutil: PathBuf,
    pub sips: PathBuf,
}

impl Default for Tools {
    fn default() -> Self {
        let tool = |name: &str| PathBuf::from("/usr/bin").join(name);
        Self { mdfind: tool("mdfind"), mdls: tool("mdls"), plutil: tool("plutil"), sips: tool("sips") }
    }
}

fn command_output(program: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).stdout(Stdio::piped()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut text).ok().map(|_| text)
    });
    let started = Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(status) => return status.success().then(|| reader.join().ok().flatten()).flatten(),
            None if started.elapsed() > COMMAND_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn plist_value(tools: &Tools, plist: &Path, key: &str) -> String {
    command_output(&tools.plutil, &["-extract", key, "raw", "-o", "-", &plist.to_string_lossy()]).map(|value| value.trim().to_string()).unwrap_or_default()
}

fn escape_spotlight(value: &str) -> String {
    value.chars().flat_map(|c| if matches!(c, '\\' | '\'' | '*' | '?') { vec!['\\', c] } else { vec![c] }).collect()
}

fn existing_file(path: &Path) -> Option<PathBuf> {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()).then(|| path.to_path_buf())
}

fn application_path(tools: &Tools, app: &Value) -> Option<String> {
    let by_id = app["_tag"] == "app-id";
    let display_name = app["displayName"].as_str().unwrap_or_default();
    let query = if by_id {
        format!("kMDItemCFBundleIdentifier == '{}'", app["appId"].as_str()?)
    } else {
        format!("kMDItemContentType == 'com.apple.application-bundle' && kMDItemDisplayName == '{}'", escape_spotlight(display_name))
    };
    let output = command_output(&tools.mdfind, &[&query])?;
    let candidates: Vec<String> = output.lines().map(str::trim).filter(|line| line.ends_with(".app")).map(str::to_owned).collect();
    let matching: Vec<String> = if by_id {
        candidates.clone()
    } else {
        candidates.iter().filter(|candidate| Path::new(candidate).file_stem().is_some_and(|stem| stem.to_string_lossy().to_lowercase() == display_name.to_lowercase())).cloned().collect()
    };
    let ranked = if matching.is_empty() { candidates } else { matching };
    let mut best: Option<(String, String)> = None;
    for candidate in ranked {
        let used = command_output(&tools.mdls, &["-raw", "-name", "kMDItemLastUsedDate", &candidate]).map(|value| value.trim().to_string()).unwrap_or_default();
        if best.as_ref().is_none_or(|(_, last)| used > *last) {
            best = Some((candidate, used));
        }
    }
    best.map(|(path, _)| path)
}

fn resolve_uncached(tools: &Tools, app: &Value, cache_directory: &Path) -> Option<PathBuf> {
    let app_path = application_path(tools, app)?;
    let canonical_app = std::fs::canonicalize(app_path).ok()?;
    let info_plist = canonical_app.join("Contents/Info.plist");
    let resources = canonical_app.join("Contents/Resources");
    let icon_name = Some(plist_value(tools, &info_plist, "CFBundleIconFile")).filter(|name| !name.is_empty()).unwrap_or_else(|| plist_value(tools, &info_plist, "CFBundleIconName"));
    if !icon_name.is_empty() && Path::new(&icon_name).file_name().is_none_or(|name| name.to_string_lossy() != icon_name) {
        return None;
    }
    let icon_file = (!icon_name.is_empty()).then(|| if Path::new(&icon_name).extension().is_some() { icon_name.clone() } else { format!("{icon_name}.icns") });
    let entries: Vec<String> = std::fs::read_dir(&resources).map(|entries| entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    let source = icon_file
        .and_then(|name| existing_file(&resources.join(name)))
        .or_else(|| existing_file(&resources.join("AppIcon.icns")))
        .or_else(|| entries.iter().find(|entry| entry.to_lowercase().ends_with(".icns")).and_then(|entry| existing_file(&resources.join(entry))))?;
    let source = std::fs::canonicalize(source).ok()?;
    // The icon must be a file of the bundle's own Resources, whatever a link inside it says.
    if !source.starts_with(std::fs::canonicalize(&resources).ok()?) {
        return None;
    }
    let version = Some(plist_value(tools, &info_plist, "CFBundleVersion")).filter(|v| !v.is_empty()).unwrap_or_else(|| plist_value(tools, &info_plist, "CFBundleShortVersionString"));
    let key: String = Sha256::digest(format!("{}\0{}\0{}", canonical_app.display(), version, source.display()).as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect();
    let cache_path = cache_directory.join(format!("{key}.png"));
    if existing_file(&cache_path).is_some() {
        return Some(cache_path);
    }
    std::fs::create_dir_all(cache_directory).ok()?;
    let temporary = cache_directory.join(format!(".{key}-{}-{}.png", std::process::id(), uuid::Uuid::new_v4()));
    let size = ICON_SIZE.to_string();
    let converted = command_output(&tools.sips, &["-z", &size, &size, "-s", "format", "png", &source.to_string_lossy(), "--out", &temporary.to_string_lossy()]).is_some() && std::fs::rename(&temporary, &cache_path).is_ok();
    let _ = std::fs::remove_file(&temporary);
    if converted { existing_file(&cache_path) } else { None }
}

fn contains_control_character(value: &str) -> bool {
    value.chars().any(|character| (character as u32) <= 31 || character as u32 == 127)
}

/// The cached PNG for the application, or `None` when no icon is available. `darwin` is false on
/// every other host, where there are no application bundles to read.
pub fn resolve_with(tools: &Tools, darwin: bool, app: &Value, cache_directory: &Path) -> Option<PathBuf> {
    if !darwin || (app["_tag"] == "display-name" && contains_control_character(app["displayName"].as_str().unwrap_or_default())) {
        return None;
    }
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Option<PathBuf>)>>> = OnceLock::new();
    let key = format!("{}\0{}", cache_directory.display(), app);
    let cached = CACHE.get_or_init(Default::default).lock().unwrap().get(&key).cloned();
    if let Some((at, found)) = cached {
        if at.elapsed() < RESOLUTION_CACHE_TTL && found.as_ref().is_none_or(|path| existing_file(path).is_some()) {
            return found;
        }
    }
    let found = resolve_uncached(tools, app, cache_directory);
    CACHE.get_or_init(Default::default).lock().unwrap().insert(key, (Instant::now(), found.clone()));
    found
}

pub fn resolve(app: &Value, cache_directory: &Path) -> Option<PathBuf> {
    resolve_with(&Tools::default(), cfg!(target_os = "macos"), app, cache_directory)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use serde_json::json;

    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A bundle, and fake `mdfind`/`mdls`/`plutil`/`sips` at absolute temp paths, so nothing real runs.
    fn fixture() -> (tempfile::TempDir, Tools, PathBuf, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let app = base.join("Fake.app");
        std::fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        std::fs::write(app.join("Contents/Resources/Fake.icns"), "icns").unwrap();
        std::fs::write(app.join("Contents/Info.plist"), "plist").unwrap();
        let tools = Tools { mdfind: base.join("mdfind"), mdls: base.join("mdls"), plutil: base.join("plutil"), sips: base.join("sips") };
        script(&tools.mdfind, &format!("echo '{}'", app.display()));
        script(&tools.mdls, "echo '2026-01-01'");
        script(&tools.plutil, "case \"$2\" in CFBundleIconFile) echo Fake;; CFBundleVersion) echo 1.2;; esac");
        // The fake converter copies its input to `--out`.
        script(&tools.sips, "shift 6; cp \"$1\" \"$3\"");
        (directory, tools, app, base.join("cache"))
    }

    #[test]
    fn an_app_resolves_to_a_cached_png_and_other_hosts_have_none() {
        let (_directory, tools, _app, cache) = fixture();
        let app = json!({"_tag":"app-id","appId":"com.example.Fake"});
        assert_eq!(resolve_with(&tools, false, &app, &cache), None);
        let png = resolve_with(&tools, true, &app, &cache).unwrap();
        assert!(png.starts_with(&cache) && png.extension().unwrap() == "png");
        assert_eq!(std::fs::read_to_string(&png).unwrap(), "icns");
        assert_eq!(resolve_with(&tools, true, &app, &cache), Some(png));
        // A display name with a control character never reaches Spotlight.
        let hostile = json!({"_tag":"display-name","displayName":"Fake\nApp"});
        assert_eq!(resolve_with(&tools, true, &hostile, &cache), None);
    }

    #[test]
    fn an_icon_that_a_link_points_out_of_the_bundle_is_refused() {
        let (_directory, tools, app, cache) = fixture();
        let outside = app.parent().unwrap().join("secret.icns");
        std::fs::write(&outside, "secret").unwrap();
        std::fs::remove_file(app.join("Contents/Resources/Fake.icns")).unwrap();
        std::os::unix::fs::symlink(&outside, app.join("Contents/Resources/Fake.icns")).unwrap();
        let app = json!({"_tag":"display-name","displayName":"Fake"});
        assert_eq!(resolve_with(&tools, true, &app, &cache), None);
    }
}
