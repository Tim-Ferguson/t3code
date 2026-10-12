//! Where the server's browser keeps a profile's storage, and clearing it (apps/server/src/preview/ServerBrowserContexts.ts).
//! A profile is a directory of the browser's own files under `<state dir>/server-browser/profiles`, named by the profile id.
use std::path::{Path, PathBuf};
use t3_contracts::INCOGNITO_BROWSER_PROFILE_ID;

/// `<state dir>/server-browser/profiles`.
pub fn profiles_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("server-browser").join("profiles")
}

/// JavaScript's `encodeURIComponent`: everything but letters, digits and `-_.!~*'()` is written as percent-encoded UTF-8.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The directory of one profile. The id is client-supplied, so it is encoded to one path segment that cannot name a parent
/// (`/` becomes `%2F`) and a segment made only of dots is spelled out as escapes (`..` would be the parent itself).
pub fn profile_directory(profiles_dir: &Path, profile_id: &str) -> PathBuf {
    let encoded = encode_uri_component(profile_id);
    let segment = if profile_id == "." || profile_id == ".." { encoded.replace('.', "%2E") } else { encoded };
    profiles_dir.join(segment)
}

/// Deletes a profile's storage; a profile with none is already clear. The incognito profile has no storage on disk.
/// The server has no browser holding the profile open yet, so there is no context to close first.
pub async fn clear_profile(profiles_dir: &Path, profile_id: &str) -> std::io::Result<()> {
    if profile_id == INCOGNITO_BROWSER_PROFILE_ID {
        return Ok(());
    }
    match tokio::fs::remove_dir_all(profile_directory(profiles_dir, profile_id)).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_id_is_one_path_segment_whatever_it_holds() {
        let root = Path::new("/state/server-browser/profiles");
        let directory = |id| profile_directory(root, id);
        assert_eq!(directory("work"), root.join("work"));
        assert_eq!(directory("a b"), root.join("a%20b"));
        assert_eq!(directory("../escape"), root.join("..%2Fescape"));
        assert_eq!(directory("a/b"), root.join("a%2Fb"));
        assert_eq!(directory("a\\b"), root.join("a%5Cb"));
        assert_eq!(directory("%2e%2e"), root.join("%252e%252e"));
        assert_eq!(directory("naïve 😀"), root.join("na%C3%AFve%20%F0%9F%98%80"));
        assert_eq!(directory("it's (1)!~*"), root.join("it's%20(1)!~*"));
        // A segment of dots alone would be this directory or its parent.
        assert_eq!(directory("."), root.join("%2E"));
        assert_eq!(directory(".."), root.join("%2E%2E"));
        assert_eq!(directory("..."), root.join("..."));
        for id in ["..", ".", "../..", "/etc", "a/../../b", "\u{0}"] {
            let path = directory(id);
            assert_eq!(path.parent(), Some(root), "{id:?} stays directly under the profiles directory");
        }
    }

    #[tokio::test]
    async fn clearing_removes_only_that_profile() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().to_path_buf();
        let profiles = dir.join("profiles");
        for id in ["work", "personal", "a/b"] {
            let path = profile_directory(&profiles, id);
            std::fs::create_dir_all(path.join("Default")).unwrap();
            std::fs::write(path.join("Default").join("Cookies"), "x").unwrap();
        }
        clear_profile(&profiles, "work").await.unwrap();
        assert!(!profile_directory(&profiles, "work").exists());
        assert!(profile_directory(&profiles, "personal").exists());
        clear_profile(&profiles, "a/b").await.unwrap();
        assert!(!profile_directory(&profiles, "a/b").exists());
        assert!(profile_directory(&profiles, "personal").exists());
        // Clearing twice, and clearing a profile that never had storage, succeed.
        clear_profile(&profiles, "work").await.unwrap();
        clear_profile(&profiles, "never-opened").await.unwrap();
        // A profile id that spells out the parent clears nothing outside the profiles directory.
        std::fs::write(dir.join("keep"), "x").unwrap();
        clear_profile(&profiles, "..").await.unwrap();
        clear_profile(&profiles, "../").await.unwrap();
        assert!(dir.join("keep").exists() && profiles.exists());
        clear_profile(&profiles, INCOGNITO_BROWSER_PROFILE_ID).await.unwrap();
        assert!(profile_directory(&profiles, "personal").exists());
    }
}
