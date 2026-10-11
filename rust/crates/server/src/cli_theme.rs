//! `t3 theme set|clear|show` (`apps/server/src/cli/theme.ts`): inspect and set the environment's theme. Connected web and
//! desktop clients switch when it is set. It writes `defaultTheme` (and `defaultThemeSetAt`, so a re-set of the same value
//! still acts) into the environment's `settings.json`, which a running server watches, so it works before the first launch and
//! on a live server alike; a theme file given instead of an id is published into the themes directory first
//! (`environment_theme.rs`). The settings edit is deliberately minimal, on the parsed object rather than a round trip through
//! the settings model: settings files outlive the build that reads them, and a provisioning command must not drop keys this
//! version does not recognise.
//!
//! The location is `--base-dir` (its `userdata`), else `T3CODE_HOME`, else the default state directory. A failure prints its
//! message on standard error and exits 1; the original prints it as a log record with a timestamp and a source position.
use crate::cli_commands::{Outcome, Parsed, Spec, expand_home, iso, parse, state_dir};
use crate::environment_theme::{BUILT_IN_THEME_IDS, MAX_THEME_FILE_BYTES, decode_file, has_colors, is_publishable_id, read_guarded, read_published};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

const INVALID_THEME_ID_REASON: &str = "is not a valid theme id (lowercase letters, digits, and hyphens; not an appearance keyword)";
const CONCURRENT_WRITE_ATTEMPTS: u32 = 5;

const SET: Spec = Spec { values: &["base-dir", "id"], booleans: &[], repeated: &[], positionals: (1, 1) };
const LOCATION: Spec = Spec { values: &["base-dir"], booleans: &[], repeated: &[], positionals: (0, 0) };

struct Paths {
    settings: PathBuf,
    themes: PathBuf,
}

fn paths(parsed: &Parsed, env: &dyn Fn(&str) -> Option<String>) -> Result<Paths, String> {
    let directory = state_dir(parsed, env)?;
    Ok(Paths { settings: directory.join("settings.json"), themes: directory.join("themes") })
}

pub fn theme_command(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    theme_command_at(args, env, Utc::now())
}

/// With the clock the set-generation stamp reads, so a test can name it.
pub fn theme_command_at(args: &[String], env: &dyn Fn(&str) -> Option<String>, now: DateTime<Utc>) -> Outcome {
    let result = match args.first().map(String::as_str) {
        Some("set") => set(&args[1..], env, now),
        Some("clear") => clear(&args[1..], env, now),
        Some("show") => show(&args[1..], env),
        Some(other) => Err(format!("Unknown subcommand \"{other}\" for \"t3 theme\"")),
        None => Err("Missing subcommand for \"t3 theme\"".into()),
    };
    result.unwrap_or_else(Outcome::failure)
}

// ---------------------------------------------------------------------------------------------
// The settings file
// ---------------------------------------------------------------------------------------------

/// Reads the sparse settings object, treating only a genuinely absent file as empty. A permission or I/O error must propagate:
/// reading it as "no settings" would have the caller write a fresh sparse file over settings it never saw.
fn read_settings(path: &Path) -> Result<(String, Map<String, Value>), String> {
    let unreadable = || format!("Could not read {}. Fix its permissions, then run this again.", path.display());
    let raw = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((String::new(), Map::new())),
        Err(_) => return Err(unreadable()),
    };
    if t3_contracts::trim_wire_string(&raw).is_empty() {
        return Ok((raw, Map::new()));
    }
    match crate::server_settings::decode_jsonc(raw.as_bytes()) {
        Ok(Value::Object(settings)) => Ok((raw, settings)),
        _ => Err(format!("{} is not a JSON object. Fix or remove it, then run this again.", path.display())),
    }
}

/// `writeFileStringAtomically`: a sibling temp directory and a rename. A symlinked target is resolved first so the link survives
/// and its destination is rewritten.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let target = crate::server_secret_store::resolve_symlink_target(path)?;
    let directory = target.parent().map(Path::to_path_buf).unwrap_or_default();
    std::fs::create_dir_all(&directory)?;
    let name = target.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let temporary = tempfile_in(&directory, &format!("{name}."))?;
    let result = (|| {
        let staged = temporary.join("contents.tmp");
        std::fs::write(&staged, contents)?;
        std::fs::rename(&staged, &target)
    })();
    let _ = std::fs::remove_dir_all(&temporary);
    result
}

fn tempfile_in(directory: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    for _ in 0..16 {
        let candidate = directory.join(format!("{prefix}{}", &uuid::Uuid::new_v4().simple().to_string()[..6]));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("no unused temporary directory name"))
}

/// A running server owns this file too, and its write path is an in-process lock that cannot serialize against another
/// process. So the document is re-read immediately before the rename and the whole edit is retried when it moved underneath us:
/// "last writer merges", and an edit that keeps losing the race fails loudly rather than overwriting.
fn write_default_theme(settings_path: &Path, theme_id: &str, now: DateTime<Utc>) -> Result<(), String> {
    for attempt in 1.. {
        let (raw, settings) = read_settings(settings_path)?;
        let next = if theme_id.is_empty() {
            // Clearing removes the keys rather than storing empty strings, so the file reads like one that never set a theme.
            settings.into_iter().filter(|(key, _)| key != "defaultTheme" && key != "defaultThemeSetAt").collect::<Map<_, _>>()
        } else {
            // The timestamp is the set-generation: it lets clients apply a re-set of the same value they already applied once.
            let mut settings = settings;
            settings.insert("defaultTheme".into(), theme_id.into());
            settings.insert("defaultThemeSetAt".into(), iso(now).into());
            settings
        };
        let contents = serde_json::to_string_pretty(&Value::Object(next)).map_err(|error| error.to_string())?;
        let current = std::fs::read(settings_path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default();
        if current != raw {
            // Falling through would overwrite whatever landed in between, which is the loss this loop exists to prevent.
            if attempt >= CONCURRENT_WRITE_ATTEMPTS {
                return Err(format!("{} kept changing while writing (gave up after {CONCURRENT_WRITE_ATTEMPTS} attempts). Try again.", settings_path.display()));
            }
            continue;
        }
        return write_atomically(settings_path, &format!("{contents}\n")).map_err(|_| format!("Could not write {}.", settings_path.display()));
    }
    unreachable!("the loop returns")
}

// ---------------------------------------------------------------------------------------------
// Publishing a theme file
// ---------------------------------------------------------------------------------------------

/// A publish that can still be taken back: the destination holds the new file, and the previous occupant (if any) is set aside.
struct Published {
    theme_id: String,
    destination: PathBuf,
    backup: PathBuf,
    staging: PathBuf,
    had_previous: bool,
    staged_identity: Option<u64>,
}

fn identity(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path).ok().map(|metadata| metadata.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

impl Published {
    /// The destination is touched only while it is empty or still holds the exact file this process put there; a concurrent
    /// publisher's newer file wins, and this process's obsolete copy is discarded.
    fn revert(&self) {
        let _ = std::fs::remove_file(&self.staging);
        let destination = identity(&self.destination);
        if self.had_previous {
            if destination.is_none() || destination == self.staged_identity {
                let _ = std::fs::rename(&self.backup, &self.destination);
            } else {
                let _ = std::fs::remove_file(&self.backup);
            }
        } else if destination.is_some() && destination == self.staged_identity {
            let _ = std::fs::remove_file(&self.destination);
        }
    }
    /// A stray backup is inert (it is not `.json`, so nothing serves it).
    fn cleanup(&self) {
        if self.had_previous {
            let _ = std::fs::remove_file(&self.backup);
        }
    }
}

fn basename_without_json(path: &str) -> String {
    let name = path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or_default();
    match name.strip_suffix(".json") {
        Some(stem) if !stem.is_empty() => stem.to_owned(),
        _ => name.to_owned(),
    }
}

fn publish_file(themes_dir: &Path, file_path: &str, explicit_id: Option<&str>) -> Result<Published, String> {
    let unreadable = || format!("Could not read {file_path}.");
    // A preflight for error quality only: it tells a FIFO from an oversized file. Enforcement happens at the guarded read.
    let info = std::fs::metadata(file_path).map_err(|_| unreadable())?;
    if !info.is_file() {
        return Err(unreadable());
    }
    if info.len() > MAX_THEME_FILE_BYTES {
        return Err(format!("{file_path} is larger than {MAX_THEME_FILE_BYTES} bytes, which is more than a theme can publish."));
    }
    // An explicit source path is the user's own input, and a symlink there is a normal way to point at a theme (desktop hooks
    // symlink the current palette), so it is resolved before the guarded read.
    let resolved = std::fs::canonicalize(file_path).map_err(|_| unreadable())?;
    let raw = read_guarded(&resolved, MAX_THEME_FILE_BYTES).ok_or_else(unreadable)?;
    let Some(file) = decode_file(&raw) else {
        return Err(format!("{file_path} is not a valid theme file. Use a theme exported from T3 Code, or a seeded file with name, appearance, canvas, and accent."));
    };
    if !has_colors(&file) {
        return Err(format!("{file_path} has no colors to publish."));
    }
    let theme_id = explicit_id.map_or_else(|| basename_without_json(file_path), str::to_owned);
    // The same rules the watcher applies when it reads the directory back, so a publish cannot report success for a file that
    // will then be skipped.
    if !is_publishable_id(&theme_id) {
        return Err(format!("\"{theme_id}\" {INVALID_THEME_ID_REASON}. Pass one with --id."));
    }
    let failure = || format!("Could not publish the theme into {}.", themes_dir.display());
    let destination = themes_dir.join(format!("{theme_id}.json"));
    // Neither ends in `.json`, so the watcher never mistakes them for themes. Both names carry the pid, so concurrent
    // publishers of one id cannot unlink or restore over each other's staging and rollback copies.
    let backup = themes_dir.join(format!("{theme_id}.json.rollback-{}", std::process::id()));
    let staging = themes_dir.join(format!("{theme_id}.json.staging-{}", std::process::id()));
    std::fs::create_dir_all(themes_dir).map_err(|_| failure())?;
    // Staged in full before anything moves, so the commit below is two adjacent renames with no I/O between them. The staging
    // entry is created exclusively after clearing any stale leftover, so a symlink or file already at that predictable name is
    // never followed or written through. Written verbatim: appending a newline could push a file at the size limit past it.
    let _ = std::fs::remove_file(&staging);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let mut staged = options.open(&staging).map_err(|_| failure())?;
    staged.write_all(raw.as_bytes()).map_err(|_| failure())?;
    drop(staged);
    let staged_identity = identity(&staging);
    // Whatever occupies the destination is moved aside in one atomic step rather than inspected and then replaced: there is no
    // window between a check and the commit, and rollback restores that exact directory entry.
    let had_previous = match std::fs::rename(&destination, &backup) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Err(failure()),
    };
    let published = Published { theme_id, destination, backup, staging, had_previous, staged_identity };
    if std::fs::rename(&published.staging, &published.destination).is_err() {
        published.revert();
        return Err(failure());
    }
    Ok(published)
}

// ---------------------------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------------------------

/// Ids a client can actually resolve: this build's built-ins plus what the machine publishes, read through the same function
/// the watcher uses so a file it would skip can never be accepted here.
fn resolvable_theme_ids(themes_dir: &Path) -> Vec<String> {
    let mut ids: Vec<String> = BUILT_IN_THEME_IDS.iter().map(|id| (*id).to_owned()).chain(read_published(themes_dir).into_iter().map(|theme| theme.id.into_string())).collect();
    ids.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    ids
}

fn set(args: &[String], env: &dyn Fn(&str) -> Option<String>, now: DateTime<Utc>) -> Result<Outcome, String> {
    let parsed = parse(args, &SET)?;
    let target = expand_home(t3_contracts::trim_wire_string(&parsed.positionals[0]), env).to_string_lossy().into_owned();
    if target.is_empty() {
        return Err("Provide a theme id or file, or run `t3 theme clear` to remove the theme.".into());
    }
    let paths = paths(&parsed, env)?;
    // Path-shaped first, existence second. Deciding on existence alone would make `t3 theme set ocean` publish ./ocean whenever
    // the cwd happens to hold a file by that name, instead of selecting the built-in.
    let looks_like_path = target.ends_with(".json") || target.contains('/') || target.contains('\\') || target.starts_with('~');
    let target_is_file = looks_like_path && Path::new(&target).exists();
    let (theme_id, publish) = if target_is_file {
        // Settings are preflighted before publishing, so a settings file the set step cannot read or parse fails the command
        // before it mutates the themes directory.
        read_settings(&paths.settings)?;
        let published = publish_file(&paths.themes, &target, parsed.value("id"))?;
        (published.theme_id.clone(), Some(published))
    } else if looks_like_path {
        return Err(format!("Could not read {target}."));
    } else if crate::environment_theme::is_environment_theme_id(&target) {
        let known = resolvable_theme_ids(&paths.themes);
        if !known.contains(&target) {
            return Err(format!("No theme named \"{target}\". Available: {}. Publish one by passing a theme file instead of an id.", known.join(", ")));
        }
        (target.clone(), None)
    } else {
        return Err(format!("\"{target}\" {INVALID_THEME_ID_REASON}."));
    };
    // set means set: if the default cannot be written, the publish that rode along with it is undone rather than left as a side
    // effect of a command that reported failure.
    if let Err(error) = write_default_theme(&paths.settings, &theme_id, now) {
        if let Some(publish) = &publish {
            publish.revert();
        }
        return Err(error);
    }
    if let Some(publish) = &publish {
        publish.cleanup();
    }
    Ok(Outcome::log(if target_is_file {
        format!("Published {target} as \"{theme_id}\" and set it as the environment theme.\n")
    } else {
        format!("Environment theme set to \"{theme_id}\" in {}.\n", paths.settings.display())
    }))
}

fn clear(args: &[String], env: &dyn Fn(&str) -> Option<String>, now: DateTime<Utc>) -> Result<Outcome, String> {
    let parsed = parse(args, &LOCATION)?;
    let paths = paths(&parsed, env)?;
    write_default_theme(&paths.settings, "", now)?;
    Ok(Outcome::log(format!("Environment theme cleared in {}.\n", paths.settings.display())))
}

fn show(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<Outcome, String> {
    let parsed = parse(args, &LOCATION)?;
    let paths = paths(&parsed, env)?;
    let (_, settings) = read_settings(&paths.settings)?;
    let default_theme = settings.get("defaultTheme").and_then(Value::as_str).filter(|theme| !theme.is_empty());
    let mut published: Vec<String> = read_published(&paths.themes).into_iter().map(|theme| theme.id.into_string()).collect();
    published.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    let first = match default_theme {
        None => "Environment theme: not set.\n".to_owned(),
        Some(theme) => format!("Environment theme: \"{theme}\".\n"),
    };
    let second = if published.is_empty() {
        format!("Published themes: none (publish into {}).\n", paths.themes.display())
    } else {
        format!("Published themes: {}.\n", published.join(", "))
    };
    Ok(Outcome { stdout: format!("{first}\n{second}\n"), stderr: String::new(), code: 0 })
}
