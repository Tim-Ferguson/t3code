//! A project's icon (`ProjectIconOverride`, `ReceivedProjectIcon` and `ProjectFaviconPath` of packages/contracts/src/project.ts).
//!
//! A project shell carries two icon fields, both optional and nullable on the wire: `faviconPath` (an image file the project
//! uses; absolute when the user picked one outside the workspace, otherwise the file the server found) and `projectIcon` (a
//! chosen lucide icon, emoji or monogram). `projectIcon` wins when set; a client shows the favicon otherwise and a default
//! icon when there is neither. The server validates what clients send (`projects.mutate`, strictly, against the original's
//! schema); this module reads what it sends back.
use crate::{ProjectShell, ValidationError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectIconColor {
    Gray,
    Red,
    Orange,
    Amber,
    Yellow,
    Lime,
    Green,
    Emerald,
    Teal,
    Cyan,
    Sky,
    Blue,
    Indigo,
    Violet,
    Purple,
    Fuchsia,
    Pink,
    Rose,
}

/// The icon a project chose. Serializes as the plain shape the server stores and sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ProjectIcon {
    Lucide { name: String, color: ProjectIconColor },
    Emoji { emoji: String },
    Monogram { text: String, color: ProjectIconColor },
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `^[a-z0-9]+(?:-[a-z0-9]+)*$`, at most 64 characters.
fn is_lucide_name(name: &str) -> bool {
    !name.is_empty() && utf16_len(name) <= 64 && name.split('-').all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()))
}

impl ProjectIcon {
    /// `ReceivedProjectIcon`: what a client does with the `projectIcon` of a shell. A kind this version does not know is no
    /// override (`Ok(None)`, the client shows its default icon), a lucide icon carrying `monogramText` or `monogram` is
    /// the monogram it stood for before monograms had a kind of their own, and a known kind with fields that do not fit is
    /// an error. `null` is no override.
    pub fn from_received(value: &Value) -> Result<Option<Self>, ValidationError> {
        let invalid = |expected| Err(ValidationError { expected });
        let Some(fields) = value.as_object() else {
            return if value.is_null() { Ok(None) } else { invalid("a project icon") };
        };
        let text = |key: &str| fields.get(key).and_then(Value::as_str).map(crate::trim_wire_string).filter(|text| !text.is_empty());
        let color = || fields.get("color").and_then(|color| serde_json::from_value::<ProjectIconColor>(color.clone()).ok());
        match fields.get("kind").and_then(Value::as_str) {
            Some("lucide") => {
                let (Some(name), Some(color)) = (text("name").filter(|name| is_lucide_name(name)), color()) else {
                    return invalid("a lucide project icon");
                };
                Ok(Some(match text("monogramText").or_else(|| text("monogram")) {
                    Some(legacy) => Self::Monogram { text: legacy.to_owned(), color },
                    None => Self::Lucide { name: name.to_owned(), color },
                }))
            }
            Some("emoji") => match text("emoji").filter(|emoji| utf16_len(emoji) <= 32) {
                Some(emoji) => Ok(Some(Self::Emoji { emoji: emoji.to_owned() })),
                None => invalid("an emoji project icon"),
            },
            Some("monogram") => match (text("text").filter(|text| utf16_len(text) <= 32), color()) {
                (Some(text), Some(color)) => Ok(Some(Self::Monogram { text: text.to_owned(), color })),
                _ => invalid("a monogram project icon"),
            },
            _ => Ok(None),
        }
    }
}

/// `ProjectFaviconPath`: a workspace-relative image a project may use as its favicon (at most 1024 characters, ending in an
/// image extension).
pub fn is_project_favicon_path(path: &str) -> bool {
    let path = crate::trim_wire_string(path);
    let lower = path.to_ascii_lowercase();
    !path.is_empty() && utf16_len(path) <= 1024 && ["avif", "gif", "ico", "jpeg", "jpg", "png", "svg", "webp"].iter().any(|extension| lower.ends_with(&format!(".{extension}")))
}

impl ProjectShell {
    /// The `faviconPath` the server sent: the file the user picked, or the one it found in the workspace. `None` for
    /// absent, `null` and blank.
    pub fn favicon_path(&self) -> Option<&str> {
        self.extra.get("faviconPath").and_then(Value::as_str).map(crate::trim_wire_string).filter(|path| !path.is_empty())
    }

    /// The `projectIcon` the server sent, read as `ReceivedProjectIcon` does; `None` for absent, `null`, a kind this version
    /// does not know and an icon that does not decode.
    pub fn project_icon(&self) -> Option<ProjectIcon> {
        self.extra.get("projectIcon").and_then(|icon| ProjectIcon::from_received(icon).ok().flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn received(value: Value) -> Result<Option<ProjectIcon>, ValidationError> {
        ProjectIcon::from_received(&value)
    }

    #[test]
    fn a_received_icon_reads_each_kind_and_the_legacy_monogram() {
        assert_eq!(received(json!(null)), Ok(None));
        assert_eq!(received(json!({"kind":"lucide","name":"git-branch","color":"rose"})), Ok(Some(ProjectIcon::Lucide { name: "git-branch".into(), color: ProjectIconColor::Rose })));
        assert_eq!(received(json!({"kind":"emoji","emoji":" 🚀 "})), Ok(Some(ProjectIcon::Emoji { emoji: "🚀".into() })));
        assert_eq!(received(json!({"kind":"monogram","text":"T3","color":"sky"})), Ok(Some(ProjectIcon::Monogram { text: "T3".into(), color: ProjectIconColor::Sky })));
        // Icons stored before monograms had a kind of their own are lucide icons with the text beside them.
        let legacy = Some(ProjectIcon::Monogram { text: "AB".into(), color: ProjectIconColor::Gray });
        assert_eq!(received(json!({"kind":"lucide","name":"folder","color":"gray","monogramText":"AB"})), Ok(legacy.clone()));
        assert_eq!(received(json!({"kind":"lucide","name":"folder","color":"gray","monogram":"AB"})), Ok(legacy));
    }

    #[test]
    fn a_newer_kind_is_no_override_and_a_malformed_known_kind_is_an_error() {
        assert_eq!(received(json!({"kind":"sticker","id":3})), Ok(None));
        assert_eq!(received(json!({"id":3})), Ok(None));
        for bad in [
            json!({"kind":"lucide","name":"Folder","color":"gray"}),
            json!({"kind":"lucide","name":"a--b","color":"gray"}),
            json!({"kind":"lucide","name":"folder","color":"mauve"}),
            json!({"kind":"emoji","emoji":"  "}),
            json!({"kind":"monogram","text":"","color":"gray"}),
            json!("folder"),
        ] {
            assert!(received(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_shell_reads_both_icon_fields_and_a_favicon_needs_an_image_extension() {
        let shell: ProjectShell = serde_json::from_value(json!({"id":"p","title":"P","workspaceRoot":"/p","defaultModelSelection":null,"scripts":[],"createdAt":"now","updatedAt":"now","faviconPath":"/p/public/favicon.svg","projectIcon":{"kind":"emoji","emoji":"🚀"}})).unwrap();
        assert_eq!(shell.favicon_path(), Some("/p/public/favicon.svg"));
        assert_eq!(shell.project_icon(), Some(ProjectIcon::Emoji { emoji: "🚀".into() }));
        let bare: ProjectShell = serde_json::from_value(json!({"id":"p","title":"P","workspaceRoot":"/p","defaultModelSelection":null,"scripts":[],"createdAt":"now","updatedAt":"now","faviconPath":null,"projectIcon":null})).unwrap();
        assert_eq!((bare.favicon_path(), bare.project_icon()), (None, None));
        assert!(is_project_favicon_path("assets/Logo.PNG") && is_project_favicon_path("a.jpeg"));
        assert!(!is_project_favicon_path("notes.txt") && !is_project_favicon_path("  ") && !is_project_favicon_path(&format!("{}.png", "a".repeat(1021))));
        assert!(is_project_favicon_path(&format!("{}.png", "a".repeat(1020))));
    }
}
