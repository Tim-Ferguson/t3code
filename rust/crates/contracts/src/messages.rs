//! Attachments and structured composer context crossing message boundaries.
use crate::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapShotStateChecked {
    #[serde(rename = "on")]
    On,
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "mixed")]
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapShotSourceKind {
    #[serde(rename = "snap-shot")]
    SnapShot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapturedImageCoordinateSpace {
    #[serde(rename = "captured-image")]
    CapturedImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PastedTextSourceTag {
    #[serde(rename = "pasted-text")]
    PastedText,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapShotAccessibilityBounds {
    pub x: NonNegativeInt,

    pub y: NonNegativeInt,

    pub width: PositiveInt,

    pub height: PositiveInt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapShotAccessibilityState {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub active: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub busy: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub editable: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub enabled: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub expanded: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub focused: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub selected: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub visible: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub checked: Option<Option<SnapShotStateChecked>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapShotAccessibilityNode {
    pub role: BoundedTrimmedString<100>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub name: Option<BoundedTrimmedString<1000>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub value: Option<BoundedTrimmedString<8000>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub description: Option<BoundedTrimmedString<2000>>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub bounds: Option<SnapShotAccessibilityBounds>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub state: Option<SnapShotAccessibilityState>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub actions: Option<BoundedVec<BoundedTrimmedString<100>, 32>>,

    pub children: BoundedVec<SnapShotAccessibilityNode, 10000>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapShotAccessibilityImageSize {
    pub width: PositiveInt,

    pub height: PositiveInt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapShotSource {
    pub kind: SnapShotSourceKind,

    pub captured_at: IsoDateTime,

    pub app_name: BoundedTrimmedString<255>,

    pub window_title: BoundedTrimmedAllowEmptyString<1000>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub accessible_text: Option<Option<BoundedTrimmedString<32000>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub accessibility: Option<Option<SnapShotAccessibility>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub app_identifier: Option<Option<BoundedTrimmedString<255>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub app_icon_data_url: Option<Option<PngIconDataUrl>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatImageType {
    #[serde(rename = "image")]
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatFileType {
    #[serde(rename = "file")]
    File,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PastedTextAttachmentSource {
    pub _tag: PastedTextSourceTag,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatImageAttachment {
    pub r#type: ChatImageType,

    pub id: ChatAttachmentId,

    pub name: BoundedTrimmedString<255>,

    pub mime_type: ImageMimeType,

    pub size_bytes: RangeInt<0, 10485760>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub source: Option<Option<SnapShotSource>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatFileAttachment {
    pub r#type: ChatFileType,

    pub id: ChatAttachmentId,

    pub name: BoundedTrimmedString<255>,

    pub mime_type: BoundedTrimmedString<100>,

    pub size_bytes: RangeInt<1, 52428800>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub source: Option<Option<PastedTextAttachmentSource>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatUnknownAttachment {
    pub r#type: UnknownAttachmentType,

    pub id: ChatAttachmentId,

    pub name: BoundedTrimmedString<255>,

    pub mime_type: BoundedTrimmedString<100>,

    pub size_bytes: NonNegativeInt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadChatImageAttachment {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub id: Option<Option<ChatAttachmentId>>,

    pub r#type: ChatImageType,

    pub name: BoundedTrimmedString<255>,

    pub mime_type: ImageMimeType,

    pub size_bytes: RangeInt<0, 10485760>,

    pub data_url: BoundedTrimmedString<14000000>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub source: Option<Option<SnapShotSource>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistChatAttachmentsInput {
    pub thread_id: ThreadId,

    pub message_id: MessageId,

    pub attachments: Vec<UploadChatImageAttachment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistChatAttachmentsResult {
    pub attachments: Vec<ChatAttachment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageContextKind {
    #[serde(rename = "image")]
    Image,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContextRecord {
    pub version: LiteralInt<1>,

    pub context_id: ComposerContextId,

    pub label: BoundedString<200>,

    pub kind: ImageContextKind,

    pub attachment_id: ChatAttachmentId,

    pub name: BoundedTrimmedString<255>,

    pub mime_type: BoundedTrimmedString<100>,

    pub size_bytes: NonNegativeInt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileContextKind {
    #[serde(rename = "file")]
    File,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContextRecord {
    pub version: LiteralInt<1>,

    pub context_id: ComposerContextId,

    pub label: BoundedString<200>,

    pub kind: FileContextKind,

    pub attachment_id: ChatAttachmentId,

    pub name: BoundedTrimmedString<255>,

    pub mime_type: BoundedTrimmedString<100>,

    pub size_bytes: NonNegativeInt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalContextKind {
    #[serde(rename = "terminal")]
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalContextRecordWire {
    pub version: LiteralInt<1>,

    pub context_id: ComposerContextId,

    pub label: BoundedString<200>,

    pub kind: TerminalContextKind,

    pub terminal_id: BoundedTrimmedString<255>,

    pub terminal_label: BoundedTrimmedString<255>,

    pub line_start: NonNegativeInt,

    pub line_end: NonNegativeInt,

    pub text: BoundedString<64000>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementContextSource {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub function_name: Option<BoundedString<2048>>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub file_name: Option<BoundedString<2048>>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub line_number: Option<NonNegativeInt>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub column_number: Option<NonNegativeInt>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementContextDetails {
    pub page_url: BoundedString<2048>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub page_title: Option<BoundedString<2048>>,

    pub tag_name: BoundedTrimmedString<255>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub selector: Option<BoundedString<2048>>,

    pub html_preview: BoundedString<8000>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub component_name: Option<BoundedString<2048>>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub source: Option<ElementContextSource>,

    pub styles: BoundedString<8000>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElementContextKind {
    #[serde(rename = "element")]
    Element,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementContextRecord {
    pub version: LiteralInt<1>,

    pub context_id: ComposerContextId,

    pub label: BoundedString<200>,

    pub kind: ElementContextKind,

    pub page_url: BoundedString<2048>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub page_title: Option<BoundedString<2048>>,

    pub tag_name: BoundedTrimmedString<255>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub selector: Option<BoundedString<2048>>,

    pub html_preview: BoundedString<8000>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub component_name: Option<BoundedString<2048>>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub source: Option<ElementContextSource>,

    pub styles: BoundedString<8000>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewAnnotationStyleChange {
    pub target_id: BoundedString<2048>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub selector: Option<BoundedString<2048>>,

    pub property: BoundedString<2048>,

    pub previous_value: BoundedString<8000>,

    pub value: BoundedString<8000>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreviewAnnotationContextKind {
    #[serde(rename = "preview-annotation")]
    PreviewAnnotation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewAnnotationContextRecord {
    pub version: LiteralInt<1>,

    pub context_id: ComposerContextId,

    pub label: BoundedString<200>,

    pub kind: PreviewAnnotationContextKind,

    pub annotation_id: BoundedString<2048>,

    pub page_url: BoundedString<2048>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub page_title: Option<BoundedString<2048>>,

    pub comment: BoundedString<8000>,

    pub target_summary: BoundedString<2048>,

    pub style_changes: BoundedVec<BoundedString<2048>, 200>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub elements: Option<BoundedVec<ElementContextDetails, 50>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub element_ids: Option<BoundedVec<BoundedString<2048>, 50>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub region_count: Option<NonNegativeInt>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub stroke_count: Option<NonNegativeInt>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub style_change_details: Option<BoundedVec<PreviewAnnotationStyleChange, 200>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub screenshot_context_id: Option<ComposerContextId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestContextMetadata {
    pub number: PositiveInt,

    pub title: BoundedString<2048>,

    pub url: BoundedString<2048>,

    pub head_branch: BoundedString<2048>,

    pub base_branch: BoundedString<2048>,

    pub state: PullRequestState,

    pub is_draft: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewCommentContextKind {
    #[serde(rename = "review-comment")]
    ReviewComment,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCommentContextRecordWire {
    pub version: LiteralInt<1>,

    pub context_id: ComposerContextId,

    pub label: BoundedString<200>,

    pub kind: ReviewCommentContextKind,

    pub section_id: BoundedTrimmedString<255>,

    pub section_title: BoundedString<2048>,

    pub file_path: BoundedTrimmedString<2048>,

    pub start_index: NonNegativeInt,

    pub end_index: NonNegativeInt,

    pub range_label: BoundedString<2048>,

    pub text: BoundedString<16000>,

    pub diff: BoundedString<32000>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub fence_language: Option<BoundedString<64>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pull_request: Option<PullRequestContextMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MentionContextKind {
    #[serde(rename = "mention")]
    Mention,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionContextRecord {
    pub version: LiteralInt<1>,

    pub context_id: ComposerContextId,

    pub label: BoundedString<200>,

    pub kind: MentionContextKind,

    pub path: BoundedTrimmedString<2048>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillContextKind {
    #[serde(rename = "skill")]
    Skill,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillContextRecord {
    pub version: LiteralInt<1>,

    pub context_id: ComposerContextId,

    pub label: BoundedString<200>,

    pub kind: SkillContextKind,

    pub name: BoundedTrimmedString<255>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadContextKind {
    #[serde(rename = "thread")]
    Thread,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadContextRecord {
    pub version: LiteralInt<1>,

    pub context_id: ComposerContextId,

    pub label: BoundedString<200>,

    pub kind: ThreadContextKind,

    pub environment_id: EnvironmentId,

    pub thread_id: ThreadId,

    pub title: BoundedString<200>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnknownContextRecordWire {
    pub version: LiteralInt<1>,

    pub context_id: ComposerContextId,

    pub label: BoundedString<200>,

    pub kind: UnknownComposerContextKind,

    pub payload: serde_json::Value,
}

pub const PROVIDER_SEND_TURN_MAX_INPUT_CHARS: usize = 120000;
pub const PROVIDER_SEND_TURN_MAX_ATTACHMENTS: usize = 100;
pub const PROVIDER_SEND_TURN_MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
pub const PROVIDER_SEND_TURN_MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
pub const PROVIDER_SEND_TURN_SUPPORTED_IMAGE_MIME_TYPES: &[&str] =
    &["image/gif", "image/jpeg", "image/png", "image/webp"];
pub const COMPOSER_CONTEXT_KINDS: &[&str] = &[
    "image",
    "file",
    "terminal",
    "element",
    "preview-annotation",
    "review-comment",
    "mention",
    "skill",
    "thread",
];
fn attachment_id(value: &str) -> Result<(), ValidationError> {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "an attachment/context id with at most 128 ASCII letters, digits, underscores or dashes",
        })
    }
}
fn image_mime(value: &str) -> Result<(), ValidationError> {
    if value.encode_utf16().count() <= 100 && value.to_ascii_lowercase().starts_with("image/") {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "an image MIME type of at most 100 characters",
        })
    }
}
fn png_icon(value: &str) -> Result<(), ValidationError> {
    if value.encode_utf16().count() <= 100000
        && value
            .to_ascii_lowercase()
            .starts_with("data:image/png;base64,")
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a PNG data URL of at most 100000 characters",
        })
    }
}
fn unknown_attachment(value: &str) -> Result<(), ValidationError> {
    if !matches!(value, "image" | "file") && !value.is_empty() && value.encode_utf16().count() <= 50
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "an unknown attachment type of at most 50 characters",
        })
    }
}
fn context_kind(value: &str) -> Result<(), ValidationError> {
    if !value.is_empty()
        && value.len() <= 40
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a lowercase context kind of at most 40 characters",
        })
    }
}
fn unknown_context_kind(value: &str) -> Result<(), ValidationError> {
    context_kind(value)?;
    if COMPOSER_CONTEXT_KINDS.contains(&value) {
        Err(ValidationError {
            expected: "an unknown composer context kind",
        })
    } else {
        Ok(())
    }
}
crate::base::string_type!(ChatAttachmentId, attachment_id);
crate::base::string_type!(ComposerContextId, attachment_id);
crate::base::string_type!(ComposerContextReferenceId, attachment_id);
crate::base::string_type!(ComposerContextKind, context_kind);
crate::base::string_type!(UnknownComposerContextKind, unknown_context_kind);
crate::base::string_type!(ImageMimeType, image_mime);
crate::base::string_type!(PngIconDataUrl, png_icon);
crate::base::string_type!(UnknownAttachmentType, unknown_attachment);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatAttachment {
    Image(ChatImageAttachment),
    File(ChatFileAttachment),
    Unknown(ChatUnknownAttachment),
}
pub type UploadChatAttachment = UploadChatImageAttachment;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all_fields = "camelCase")]
pub enum SnapShotAccessibilityWire {
    #[serde(rename = "flat-text")]
    FlatText {
        text: BoundedTrimmedString<32000>,
        truncated: bool,
    },
    #[serde(rename = "element-tree")]
    ElementTree {
        coordinate_space: CapturedImageCoordinateSpace,
        image_size: SnapShotAccessibilityImageSize,
        truncated: bool,
        root: SnapShotAccessibilityNode,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    try_from = "SnapShotAccessibilityWire",
    into = "SnapShotAccessibilityWire"
)]
pub struct SnapShotAccessibility(pub SnapShotAccessibilityWire);
impl TryFrom<SnapShotAccessibilityWire> for SnapShotAccessibility {
    type Error = ValidationError;
    fn try_from(value: SnapShotAccessibilityWire) -> Result<Self, Self::Error> {
        if let SnapShotAccessibilityWire::ElementTree { root, .. } = &value {
            let mut stack = vec![root];
            let mut count = 0;
            while let Some(node) = stack.pop() {
                count += 1;
                if count > 10000 {
                    return Err(ValidationError {
                        expected: "at most 10000 accessibility nodes",
                    });
                }
                stack.extend(node.children.0.iter());
            }
            if serde_json::to_string(&value)
                .expect("JSON accessibility tree")
                .encode_utf16()
                .count()
                > 32000
            {
                return Err(ValidationError {
                    expected: "at most 32000 serialized accessibility characters",
                });
            }
        }
        Ok(Self(value))
    }
}
impl From<SnapShotAccessibility> for SnapShotAccessibilityWire {
    fn from(value: SnapShotAccessibility) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    try_from = "TerminalContextRecordWire",
    into = "TerminalContextRecordWire"
)]
pub struct TerminalContextRecord(pub TerminalContextRecordWire);
impl TryFrom<TerminalContextRecordWire> for TerminalContextRecord {
    type Error = ValidationError;
    fn try_from(value: TerminalContextRecordWire) -> Result<Self, Self::Error> {
        if value.line_end >= value.line_start {
            Ok(Self(value))
        } else {
            Err(ValidationError {
                expected: "terminal lineEnd at or after lineStart",
            })
        }
    }
}
impl From<TerminalContextRecord> for TerminalContextRecordWire {
    fn from(value: TerminalContextRecord) -> Self {
        value.0
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    try_from = "ReviewCommentContextRecordWire",
    into = "ReviewCommentContextRecordWire"
)]
pub struct ReviewCommentContextRecord(pub ReviewCommentContextRecordWire);
impl TryFrom<ReviewCommentContextRecordWire> for ReviewCommentContextRecord {
    type Error = ValidationError;
    fn try_from(value: ReviewCommentContextRecordWire) -> Result<Self, Self::Error> {
        if value.end_index >= value.start_index {
            Ok(Self(value))
        } else {
            Err(ValidationError {
                expected: "review endIndex at or after startIndex",
            })
        }
    }
}
impl From<ReviewCommentContextRecord> for ReviewCommentContextRecordWire {
    fn from(value: ReviewCommentContextRecord) -> Self {
        value.0
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    try_from = "UnknownContextRecordWire",
    into = "UnknownContextRecordWire"
)]
pub struct UnknownContextRecord(pub UnknownContextRecordWire);
impl TryFrom<UnknownContextRecordWire> for UnknownContextRecord {
    type Error = ValidationError;
    fn try_from(value: UnknownContextRecordWire) -> Result<Self, Self::Error> {
        if serde_json::to_string(&value.payload)
            .expect("JSON context payload")
            .encode_utf16()
            .count()
            <= 64000
        {
            Ok(Self(value))
        } else {
            Err(ValidationError {
                expected: "a JSON context payload with at most 64000 characters",
            })
        }
    }
}
impl From<UnknownContextRecord> for UnknownContextRecordWire {
    fn from(value: UnknownContextRecord) -> Self {
        value.0
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KnownComposerContextRecord {
    Image(ImageContextRecord),
    File(FileContextRecord),
    Terminal(TerminalContextRecord),
    Element(ElementContextRecord),
    PreviewAnnotation(PreviewAnnotationContextRecord),
    ReviewComment(ReviewCommentContextRecord),
    Mention(MentionContextRecord),
    Skill(SkillContextRecord),
    Thread(ThreadContextRecord),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ComposerContextRecord {
    Known(KnownComposerContextRecord),
    Unknown(UnknownContextRecord),
}
impl ComposerContextRecord {
    pub fn context_id(&self) -> &ComposerContextId {
        match self {
            Self::Unknown(c) => &c.0.context_id,
            Self::Known(c) => match c {
                KnownComposerContextRecord::Image(c) => &c.context_id,
                KnownComposerContextRecord::File(c) => &c.context_id,
                KnownComposerContextRecord::Terminal(c) => &c.0.context_id,
                KnownComposerContextRecord::Element(c) => &c.context_id,
                KnownComposerContextRecord::PreviewAnnotation(c) => &c.context_id,
                KnownComposerContextRecord::ReviewComment(c) => &c.0.context_id,
                KnownComposerContextRecord::Mention(c) => &c.context_id,
                KnownComposerContextRecord::Skill(c) => &c.context_id,
                KnownComposerContextRecord::Thread(c) => &c.context_id,
            },
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestrationMessageContext {
    pub version: LiteralInt<1>,
    #[serde(deserialize_with = "deserialize_context_records")]
    pub records: Vec<ComposerContextRecord>,
}
fn deserialize_context_records<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<ComposerContextRecord>, D::Error> {
    let raw = Vec::<serde_json::Value>::deserialize(d)?;
    if raw.len() > 200
        || serde_json::to_string(&raw)
            .expect("JSON context records")
            .encode_utf16()
            .count()
            > 16000000
    {
        return Err(serde::de::Error::custom(
            "context exceeds record or serialized size limits",
        ));
    }
    let records: Vec<ComposerContextRecord> = raw
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect();
    let unique: std::collections::BTreeSet<_> = records
        .iter()
        .map(ComposerContextRecord::context_id)
        .collect();
    if unique.len() != records.len() {
        return Err(serde::de::Error::custom("context ids must be unique"));
    }
    Ok(records)
}
pub fn is_provider_send_turn_supported_image_mime_type(value: &str) -> bool {
    PROVIDER_SEND_TURN_SUPPORTED_IMAGE_MIME_TYPES.contains(&value.to_ascii_lowercase().as_str())
}
impl ChatAttachment {
    pub fn size_bytes(&self) -> u64 {
        match self {
            Self::Image(c) => c.size_bytes.0 as u64,
            Self::File(c) => c.size_bytes.0 as u64,
            Self::Unknown(c) => c.size_bytes.0,
        }
    }
    pub fn mime_type(&self) -> &str {
        match self {
            Self::Image(c) => c.mime_type.as_str(),
            Self::File(c) => c.mime_type.0.as_str(),
            Self::Unknown(c) => c.mime_type.0.as_str(),
        }
    }
}
pub fn get_provider_attachment_limit_error(attachments: &[ChatAttachment]) -> Option<&'static str> {
    if attachments.len() > 100 {
        return Some("You can attach up to 100 files per message or question response.");
    }
    let image_bytes = attachments
        .iter()
        .filter(|c| {
            matches!(c, ChatAttachment::Image(_))
                || is_provider_send_turn_supported_image_mime_type(c.mime_type())
        })
        .fold(0u128, |total, c| total + c.size_bytes() as u128);
    if image_bytes > 80 * 1024 * 1024 {
        Some(
            "Images can total up to 80 MiB per message or question response. Use smaller images or send fewer at once.",
        )
    } else {
        None
    }
}
