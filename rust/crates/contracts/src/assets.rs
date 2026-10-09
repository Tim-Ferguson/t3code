//! Attachment upload and asset URL wire types (packages/contracts/src/assets.ts).
//! Failures are `_tag` envelopes built by the server, matching the other RPC modules.
use crate::history::object_struct;
use crate::*;
use serde::{Deserialize, Serialize};

pub const ATTACHMENT_UPLOAD_URL_TTL_MS: i64 = 10 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachmentDisposition {
    #[serde(rename = "inline")]
    Inline,
    #[serde(rename = "attachment")]
    Attachment,
}

/// Source: `TrimmedNonEmptyString` of at most 1024 characters ending in an image extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ProjectFaviconPath(pub BoundedTrimmedString<1024>);
impl<'de> Deserialize<'de> for ProjectFaviconPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = BoundedTrimmedString::<1024>::deserialize(d)?;
        let lower = value.0.as_str().to_ascii_lowercase();
        if ["avif", "gif", "ico", "jpeg", "jpg", "png", "svg", "webp"]
            .iter()
            .any(|extension| lower.ends_with(&format!(".{extension}")))
        {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "expected a path ending in an image extension",
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum AssetResource {
    #[serde(rename = "workspace-file", rename_all = "camelCase")]
    WorkspaceFile {
        thread_id: ThreadId,
        path: BoundedTrimmedString<1024>,
    },
    /// Served in place from anywhere the host can read; relative paths use the thread workspace.
    #[serde(rename = "media-file", rename_all = "camelCase")]
    MediaFile {
        thread_id: ThreadId,
        path: BoundedTrimmedString<1024>,
    },
    /// A draft with no thread names its workspace root explicitly.
    #[serde(rename = "draft-workspace-file", rename_all = "camelCase")]
    DraftWorkspaceFile {
        cwd: BoundedTrimmedString<1024>,
        path: BoundedTrimmedString<1024>,
    },
    /// Display name and mime are baked into the signed URL; `disposition` is an inline request.
    #[serde(rename = "attachment", rename_all = "camelCase")]
    Attachment {
        attachment_id: BoundedTrimmedString<256>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        file_name: Option<BoundedTrimmedString<255>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        mime_type: Option<BoundedTrimmedString<100>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        disposition: Option<AttachmentDisposition>,
    },
    #[serde(rename = "tool-output-image", rename_all = "camelCase")]
    ToolOutputImage {
        thread_id: ThreadId,
        item_id: TurnItemId,
        index: NonNegativeInt,
    },
    #[serde(rename = "project-favicon", rename_all = "camelCase")]
    ProjectFavicon {
        cwd: BoundedTrimmedString<1024>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        path: Option<ProjectFaviconPath>,
    },
    #[serde(rename = "native-app-icon", rename_all = "camelCase")]
    NativeAppIcon { app: ToolActivityNativeAppReference },
    #[serde(rename = "github-media", rename_all = "camelCase")]
    GithubMedia {
        cwd: BoundedTrimmedString<1024>,
        url: BoundedTrimmedString<2048>,
    },
}

object_struct! {pub struct AssetCreateUrlInput {pub resource:AssetResource,}}
object_struct! {pub struct AssetImageDimensions {pub width:PositiveInt,pub height:PositiveInt,}}
object_struct! {pub struct AssetCreateUrlResult {
    pub relative_url:BoundedTrimmedString<4096>,
    pub expires_at:i64,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub source_path:Option<BoundedTrimmedString<1024>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub image_dimensions:Option<AssetImageDimensions>,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UploadImageMimeType {
    #[serde(rename = "image/gif")]
    Gif,
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/webp")]
    Webp,
}
impl UploadImageMimeType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gif => "image/gif",
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachmentCreateUploadUrlInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<ChatImageType>,
    pub name: BoundedTrimmedString<255>,
    pub mime_type: UploadImageMimeType,
    pub size_bytes: RangeInt<1, 10485760>,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileAttachmentCreateUploadUrlInput {
    pub r#type: ChatFileType,
    pub name: BoundedTrimmedString<255>,
    pub mime_type: BoundedTrimmedString<100>,
    pub size_bytes: RangeInt<1, 52428800>,
}
/// Source union: the image member (optional `type`) is tried before the file member.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AttachmentCreateUploadUrlInput {
    Image(ImageAttachmentCreateUploadUrlInput),
    File(FileAttachmentCreateUploadUrlInput),
}
impl<'de> Deserialize<'de> for AttachmentCreateUploadUrlInput {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Image {
            #[serde(default, deserialize_with = "deserialize_optional")]
            r#type: Option<ChatImageType>,
            name: BoundedTrimmedString<255>,
            mime_type: UploadImageMimeType,
            size_bytes: RangeInt<1, 10485760>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct File {
            r#type: ChatFileType,
            name: BoundedTrimmedString<255>,
            mime_type: BoundedTrimmedString<100>,
            size_bytes: RangeInt<1, 52428800>,
        }
        let value = serde_json::Value::deserialize(d)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom("expected a JSON object"));
        }
        if let Ok(image) = serde_json::from_value::<Image>(value.clone()) {
            return Ok(Self::Image(ImageAttachmentCreateUploadUrlInput {
                r#type: image.r#type,
                name: image.name,
                mime_type: image.mime_type,
                size_bytes: image.size_bytes,
            }));
        }
        let file = serde_json::from_value::<File>(value).map_err(serde::de::Error::custom)?;
        Ok(Self::File(FileAttachmentCreateUploadUrlInput {
            r#type: file.r#type,
            name: file.name,
            mime_type: file.mime_type,
            size_bytes: file.size_bytes,
        }))
    }
}
impl AttachmentCreateUploadUrlInput {
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }
    pub fn name(&self) -> &str {
        match self {
            Self::Image(input) => input.name.0.as_str(),
            Self::File(input) => input.name.0.as_str(),
        }
    }
    pub fn mime_type(&self) -> &str {
        match self {
            Self::Image(input) => input.mime_type.as_str(),
            Self::File(input) => input.mime_type.0.as_str(),
        }
    }
    pub fn size_bytes(&self) -> u64 {
        match self {
            Self::Image(input) => input.size_bytes.0 as u64,
            Self::File(input) => input.size_bytes.0 as u64,
        }
    }
}

object_struct! {pub struct AttachmentCreateUploadUrlResult {
    pub attachment_id:BoundedTrimmedString<256>,
    pub relative_url:BoundedTrimmedString<4096>,
    pub expires_at:i64,
}}
object_struct! {pub struct AttachmentDeleteInput {pub attachment_id:BoundedTrimmedString<256>,}}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn upload_input_accepts_the_source_union_members_only() {
        let image: AttachmentCreateUploadUrlInput = serde_json::from_value(
            json!({"name":" a.png ","mimeType":"image/png","sizeBytes":6,"ignored":true}),
        )
        .unwrap();
        assert!(!image.is_file());
        assert_eq!(image.name(), "a.png");
        let file: AttachmentCreateUploadUrlInput = serde_json::from_value(
            json!({"type":"file","name":"r.pdf","mimeType":"application/pdf","sizeBytes":50*1024*1024}),
        )
        .unwrap();
        assert!(file.is_file());
        for rejected in [
            json!({"name":"a.svg","mimeType":"image/svg+xml","sizeBytes":6}),
            json!({"name":"a.png","mimeType":"image/png","sizeBytes":0}),
            json!({"name":"a.png","mimeType":"image/png","sizeBytes":10*1024*1024+1}),
            json!({"type":"file","name":"r","mimeType":"x/y","sizeBytes":50*1024*1024+1}),
            json!({"type":"video","name":"r","mimeType":"x/y","sizeBytes":1}),
            json!({"name":"  ","mimeType":"image/png","sizeBytes":1}),
            json!([]),
        ] {
            assert!(
                serde_json::from_value::<AttachmentCreateUploadUrlInput>(rejected.clone()).is_err(),
                "{rejected}"
            );
        }
    }

    #[test]
    fn asset_resources_round_trip_and_enforce_bounds() {
        let input: AssetCreateUrlInput = serde_json::from_value(json!({"resource":{"_tag":"attachment","attachmentId":"thread-00000000-0000-4000-8000-000000000001","fileName":"x.pdf","mimeType":"application/pdf","disposition":"inline"}})).unwrap();
        assert!(matches!(
            input.resource,
            AssetResource::Attachment {
                disposition: Some(AttachmentDisposition::Inline),
                ..
            }
        ));
        let bare: AssetResource =
            serde_json::from_value(json!({"_tag":"attachment","attachmentId":"a"})).unwrap();
        assert_eq!(
            serde_json::to_value(bare).unwrap(),
            json!({"_tag":"attachment","attachmentId":"a"})
        );
        for rejected in [
            json!({"_tag":"attachment","attachmentId":"a","disposition":"download"}),
            json!({"_tag":"attachment","attachmentId":"a","fileName":null}),
            json!({"_tag":"attachment","attachmentId":"x".repeat(257)}),
            json!({"_tag":"project-favicon","cwd":"/p","path":"icon.txt"}),
            json!({"_tag":"nope"}),
        ] {
            assert!(
                serde_json::from_value::<AssetResource>(rejected.clone()).is_err(),
                "{rejected}"
            );
        }
        serde_json::from_value::<AssetResource>(
            json!({"_tag":"project-favicon","cwd":"/p","path":"ICON.PNG"}),
        )
        .unwrap();
    }
}
