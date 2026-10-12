//! The host-setup diagnosis: what a failed launch is blamed on, and the text that names the fix.
use crate::preview_browser_host::*;
use std::path::Path;

#[test]
fn only_an_explicit_zero_turns_the_sandbox_off() {
    let with = |value: Option<&'static str>| sandbox_disabled(move |name| if name == "T3CODE_SERVER_BROWSER_SANDBOX" { value.map(str::to_owned) } else { None });
    assert!(with(Some("0")));
    for other in [None, Some(""), Some("1"), Some("false"), Some(" 0"), Some("00")] {
        assert!(!with(other), "{other:?}");
    }
}

#[tokio::test]
async fn the_sandbox_is_blamed_when_chrome_says_it_has_none_on_any_platform() {
    let blamed = diagnose("darwin", "[1:1:ERROR] No usable sandbox! Update your kernel", "sudo t3 browser setup", || async { unreachable!("the libraries are not looked at") }).await;
    assert_eq!(blamed, Some(PreviewBrowserHostError::Sandbox { setup_command: "sudo t3 browser setup".into() }));
    assert_eq!(
        blamed.unwrap().message(),
        "This host blocks the sandbox T3's browser runs in (AppArmor on Ubuntu 23.10+). Run `sudo t3 browser setup` on the host once to allow it, then try again."
    );
}

#[tokio::test]
async fn libraries_are_looked_for_on_linux_only_and_named_when_missing() {
    let missing = || async { vec!["libnss3.so".to_owned(), "libgbm.so.1".to_owned()] };
    let blamed = diagnose("linux", "Failed to launch", "sudo t3 browser setup", missing).await.unwrap();
    assert_eq!(blamed.tag(), "PreviewBrowserLibrariesError");
    assert_eq!(blamed.message(), "This host is missing libraries T3's browser needs (libnss3.so, libgbm.so.1). Run `sudo t3 browser setup` on the host to install them, then try again.");
    // Other hosts never need them, so the check is not even made.
    assert_eq!(diagnose("macos", "Failed to launch", "x", || async { unreachable!() }).await, None);
    assert_eq!(diagnose("windows", "Failed to launch", "x", || async { unreachable!() }).await, None);
    // Nothing missing, nothing to blame.
    assert_eq!(diagnose("linux", "Failed to launch", "x", || async { Vec::new() }).await, None);
}

#[test]
fn an_ldd_report_gives_the_libraries_the_loader_cannot_find() {
    let report = "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibnss3.so => not found\n\tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f)\n  libgbm.so.1 => not found\nnot a dynamic executable => not found => not found\n\t => not found\n";
    assert_eq!(missing_libraries_in(report), ["libnss3.so", "libgbm.so.1"]);
    assert!(missing_libraries_in("").is_empty());
}

#[tokio::test]
async fn the_sandbox_counts_as_blocked_only_where_namespaces_are_restricted_and_no_profile_is_installed() {
    let directory = tempfile::tempdir().unwrap();
    let restriction = directory.path().join("restrict");
    let profile = directory.path().join("profile");
    // No restriction file at all: the host does not restrict.
    assert!(!sandbox_blocked_on("linux", &restriction, &profile).await);
    std::fs::write(&restriction, "0\n").unwrap();
    assert!(!sandbox_blocked_on("linux", &restriction, &profile).await);
    std::fs::write(&restriction, "1\n").unwrap();
    assert!(sandbox_blocked_on("linux", &restriction, &profile).await);
    // The profile fixes it, and other platforms are never blocked.
    std::fs::write(&profile, "profile").unwrap();
    assert!(!sandbox_blocked_on("linux", &restriction, &profile).await);
    std::fs::remove_file(&profile).unwrap();
    assert!(!sandbox_blocked_on("macos", &restriction, &profile).await);
}

#[tokio::test]
async fn a_missing_ldd_or_executable_finds_nothing() {
    assert!(missing_libraries(Path::new("/nonexistent/t3-test-no-such-binary")).await.is_empty());
}

#[test]
fn the_setup_files_name_the_command_and_the_packages_the_original_names() {
    assert_eq!(SETUP_SUBCOMMAND, "browser setup");
    assert!(APPARMOR_PROFILE.starts_with("# Written by `t3 browser setup`") && APPARMOR_PROFILE.contains("userns,"));
    assert_eq!(DEBIAN_PACKAGES.len(), 17);
    assert_eq!(DEBIAN_PACKAGES[1], ["libglib2.0-0t64", "libglib2.0-0"]);
}
