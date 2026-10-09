//! Composer attachment rules shared by every client surface. Ports the pure parts of
//! apps/web/src/components/chat/composerAttachmentFiles.ts, packages/client-runtime/
//! src/state/attachments.ts, apps/web/src/lib/attachmentUploadState.ts and the limits
//! in packages/contracts/src/chatAttachment.ts. Transfers and storage live in the UI.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::rc::Rc;

pub const MAX_ATTACHMENTS: usize = 100;
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_TOTAL_IMAGE_BYTES: u64 = 80 * 1024 * 1024;
/// Uploads that may transfer at once; the rest wait their turn.
pub const MAX_CONCURRENT_UPLOADS: usize = 3;
const SUPPORTED_IMAGE_MIME_TYPES: [&str; 4] = ["image/gif", "image/jpeg", "image/png", "image/webp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Image,
    File,
}
impl Kind {
    pub fn wire(self) -> &'static str {
        match self {
            Kind::Image => "image",
            Kind::File => "file",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    Image,
    File,
    UnsupportedImage,
}

/// Drags from other apps often arrive with an empty or generic type; the extension
/// then decides, so a plain `photo.jpg` still lands on the image path.
pub fn infer_image_mime(name: &str) -> Option<&'static str> {
    let dot = name.rfind('.').filter(|dot| *dot > 0)?;
    match name[dot + 1..].to_ascii_lowercase().as_str() {
        "gif" => Some("image/gif"),
        "jpeg" | "jpg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}
fn generic(mime: &str) -> bool {
    mime.is_empty() || mime.eq_ignore_ascii_case("application/octet-stream")
}
pub fn is_supported_image_mime(mime: &str) -> bool {
    SUPPORTED_IMAGE_MIME_TYPES
        .iter()
        .any(|supported| supported.eq_ignore_ascii_case(mime))
}
pub fn classify(name: &str, mime: &str) -> Classification {
    if generic(mime) {
        return if infer_image_mime(name).is_some() {
            Classification::Image
        } else {
            Classification::File
        };
    }
    if !mime.to_ascii_lowercase().starts_with("image/") {
        Classification::File
    } else if is_supported_image_mime(mime) {
        Classification::Image
    } else {
        Classification::UnsupportedImage
    }
}
/// The type recorded on the attachment and sent to the upload endpoint.
pub fn effective_mime(name: &str, mime: &str, kind: Kind) -> String {
    match kind {
        Kind::Image if generic(mime) => infer_image_mime(name).unwrap_or("image/png").into(),
        Kind::Image => mime.to_ascii_lowercase(),
        Kind::File if mime.is_empty() => "application/octet-stream".into(),
        Kind::File => mime.chars().take(100).collect(),
    }
}

/// "3.2 MB" / "48 KB" label for attachment rows. Never shows "0 KB".
pub fn format_size(size_bytes: u64) -> String {
    if size_bytes >= 1024 * 1024 {
        format!("{:.1} MB", size_bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", size_bytes.div_ceil(1024).max(1))
    }
}
pub fn too_large_message(name: &str, max_bytes: u64) -> String {
    let limit = if max_bytes >= 1024 * 1024 && max_bytes % (1024 * 1024) == 0 {
        format!("{} MB", max_bytes / (1024 * 1024))
    } else if max_bytes >= 1024 && max_bytes % 1024 == 0 {
        format!("{} KB", max_bytes / 1024)
    } else {
        format!("{max_bytes} {}", if max_bytes == 1 { "byte" } else { "bytes" })
    };
    format!("'{name}' exceeds the {limit} attachment limit.")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Admitted {
    /// Position in the candidate list.
    pub index: usize,
    pub kind: Kind,
    pub name: String,
    pub mime_type: String,
}
/// Validation runs synchronously, before any bytes are read, so concurrent picks
/// and pastes see each other's reserved slots. As in the original, the last
/// refusal in a batch is the one reported.
pub fn admit(existing: usize, candidates: &[Candidate]) -> (Vec<Admitted>, Option<String>) {
    let mut admitted = vec![];
    let mut error = None;
    let mut reserved = existing;
    for (index, candidate) in candidates.iter().enumerate() {
        if reserved >= MAX_ATTACHMENTS {
            error = Some(format!(
                "You can attach up to {MAX_ATTACHMENTS} files per message."
            ));
            continue;
        }
        let class = classify(&candidate.name, &candidate.mime_type);
        let kind = match class {
            Classification::UnsupportedImage => {
                error = Some(format!(
                    "'{}' is not a supported image type. Attach GIF, JPEG, PNG, or WebP images.",
                    candidate.name
                ));
                continue;
            }
            Classification::Image => Kind::Image,
            Classification::File => Kind::File,
        };
        let limit = if kind == Kind::Image {
            MAX_IMAGE_BYTES
        } else {
            MAX_FILE_BYTES
        };
        if candidate.size_bytes == 0 {
            error = Some(format!("'{}' is empty or could not be read.", candidate.name));
            continue;
        }
        if candidate.size_bytes > limit {
            error = Some(too_large_message(&candidate.name, limit));
            continue;
        }
        let name: String = if candidate.name.trim().is_empty() {
            if kind == Kind::Image { "image" } else { "file" }.into()
        } else {
            candidate.name.chars().take(255).collect()
        };
        admitted.push(Admitted {
            index,
            kind,
            mime_type: effective_mime(&candidate.name, &candidate.mime_type, kind),
            name,
        });
        reserved += 1;
    }
    (admitted, error)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Send the bytes again.
    Upload,
    /// Ask the server whether the saved upload still exists.
    Verify,
    /// Nothing left to retry from; only removal helps.
    None,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Upload {
    Uploading,
    Verifying,
    Ready,
    Failed { reason: String, retry: Retry },
    /// Restored without bytes or a server copy; the user must attach it again.
    NeedsReattach,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    /// Stable local identity; never the server id.
    pub id: String,
    pub kind: Kind,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    /// The pending upload this entry owns on the server: the one a send claims when
    /// `Ready`, or a stale one left by a failed transfer that retry/removal deletes.
    pub attachment_id: Option<String>,
    pub upload: Upload,
    /// Held only until the server has the copy (or for retry).
    pub bytes: Option<Rc<[u8]>>,
    pub preview: Option<Preview>,
}
impl Attachment {
    pub fn new(id: String, admitted: &Admitted, size_bytes: u64) -> Self {
        Self {
            id,
            kind: admitted.kind,
            name: admitted.name.clone(),
            mime_type: admitted.mime_type.clone(),
            size_bytes,
            attachment_id: None,
            upload: Upload::Uploading,
            bytes: None,
            preview: None,
        }
    }
}

pub fn block_reason(attachments: &[Attachment]) -> Option<String> {
    let count = |matches: fn(&Upload) -> bool| {
        attachments
            .iter()
            .filter(|attachment| matches(&attachment.upload))
            .count()
    };
    let failed = count(|upload| matches!(upload, Upload::Failed { .. }));
    if failed > 0 {
        return Some(
            if failed == 1 {
                "Retry or remove the failed attachment"
            } else {
                "Retry or remove the failed attachments"
            }
            .into(),
        );
    }
    let missing = count(|upload| matches!(upload, Upload::NeedsReattach));
    if missing > 0 {
        return Some(
            if missing == 1 {
                "Attach the missing file again or remove it"
            } else {
                "Attach the missing files again or remove them"
            }
            .into(),
        );
    }
    let pending = count(|upload| matches!(upload, Upload::Uploading | Upload::Verifying));
    (pending > 0).then(|| {
        if pending == 1 {
            "Attachment still uploading"
        } else {
            "Attachments still uploading"
        }
        .into()
    })
}
/// Mirrors getProviderAttachmentLimitError.
pub fn limit_error(attachments: &[Attachment]) -> Option<String> {
    if attachments.len() > MAX_ATTACHMENTS {
        return Some(format!(
            "You can attach up to {MAX_ATTACHMENTS} files per message or question response."
        ));
    }
    let image_bytes: u64 = attachments
        .iter()
        .filter(|attachment| attachment.kind == Kind::Image || is_supported_image_mime(&attachment.mime_type))
        .map(|attachment| attachment.size_bytes)
        .sum();
    (image_bytes > MAX_TOTAL_IMAGE_BYTES).then(|| "Images can total up to 80 MiB per message or question response. Use smaller images or send fewer at once.".into())
}
/// The `attachments` array of a message: ready uploads only, by their server ids.
pub fn wire(attachments: &[Attachment]) -> Option<Vec<Value>> {
    attachments
        .iter()
        .map(|attachment| {
            Some(json!({"type":attachment.kind.wire(),"id":attachment.attachment_id.as_ref().filter(|_|attachment.upload==Upload::Ready)?,"name":attachment.name,"mimeType":attachment.mime_type,"sizeBytes":attachment.size_bytes}))
        })
        .collect()
}

/// What the sidecar keeps. Bytes and previews are rebuilt from the server copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Persisted {
    pub id: String,
    pub r#type: Kind,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<String>,
}
impl Persisted {
    pub fn of(attachment: &Attachment) -> Self {
        Self {
            id: attachment.id.clone(),
            r#type: attachment.kind,
            name: attachment.name.clone(),
            mime_type: attachment.mime_type.clone(),
            size_bytes: attachment.size_bytes,
            // A failed transfer's id is never the draft's copy.
            attachment_id: attachment
                .attachment_id
                .clone()
                .filter(|_| attachment.upload == Upload::Ready),
        }
    }
    /// A saved server copy is verified before use; without one the user re-attaches.
    pub fn restore(self) -> Attachment {
        Attachment {
            upload: if self.attachment_id.is_some() {
                Upload::Verifying
            } else {
                Upload::NeedsReattach
            },
            id: self.id,
            kind: self.r#type,
            name: self.name,
            mime_type: self.mime_type,
            size_bytes: self.size_bytes,
            attachment_id: self.attachment_id,
            bytes: None,
            preview: None,
        }
    }
}

/// Restores attachments from a version-9 original draft, which keeps images as data
/// URLs (re-uploaded like a fresh pick) and files as a server id plus environment
/// (verified) or, when the upload never finished, as a marker to attach again.
pub fn restore_source(draft: &Value, environment: &str) -> Vec<Attachment> {
    let text = |value: &Value, key: &str| value[key].as_str().unwrap_or_default().to_owned();
    let mut restored = vec![];
    for image in draft["attachments"].as_array().into_iter().flatten() {
        let mut attachment = Attachment {
            id: text(image, "id"),
            kind: Kind::Image,
            name: text(image, "name"),
            mime_type: text(image, "mimeType"),
            size_bytes: image["sizeBytes"].as_u64().unwrap_or_default(),
            attachment_id: None,
            upload: Upload::NeedsReattach,
            bytes: None,
            preview: None,
        };
        if let Some((mime, bytes)) = image["dataUrl"].as_str().and_then(decode_data_url) {
            if is_supported_image_mime(&mime) && !bytes.is_empty() && bytes.len() as u64 <= MAX_IMAGE_BYTES {
                attachment.mime_type = mime;
                attachment.size_bytes = bytes.len() as u64;
                attachment.bytes = Some(bytes.into());
                attachment.upload = Upload::Uploading;
            }
        }
        restored.push(attachment);
    }
    for file in draft["files"].as_array().into_iter().flatten() {
        let saved = file["attachmentId"]
            .as_str()
            .filter(|_| file["environmentId"] == environment);
        restored.push(Attachment {
            id: text(file, "id"),
            kind: Kind::File,
            name: text(file, "name"),
            mime_type: text(file, "mimeType"),
            size_bytes: file["sizeBytes"].as_u64().unwrap_or_default(),
            attachment_id: saved.map(str::to_owned),
            upload: if saved.is_some() {
                Upload::Verifying
            } else {
                Upload::NeedsReattach
            },
            bytes: None,
            preview: None,
        });
    }
    restored
}

pub fn decode_data_url(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    let mut parts = header.split(';');
    let mime = parts.next().unwrap_or_default().to_ascii_lowercase();
    let bytes = if parts.any(|part| part.eq_ignore_ascii_case("base64")) {
        decode_base64(payload)?
    } else {
        percent_decode(payload)?
    };
    Some((mime, bytes))
}
/// Standard-alphabet base64, tolerant of padding, whitespace and the URL alphabet.
pub fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        buffer = buffer << 6 | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(output)
}
fn percent_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
            output.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            output.push(bytes[at]);
            at += 1;
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate(name: &str, mime: &str, size: u64) -> Candidate {
        Candidate {
            name: name.into(),
            mime_type: mime.into(),
            size_bytes: size,
        }
    }

    #[test]
    fn classification_follows_the_original_extension_fallback_and_provider_image_types() {
        assert_eq!(classify("photo.JPG", ""), Classification::Image);
        assert_eq!(classify("photo.jpg", "application/octet-stream"), Classification::Image);
        assert_eq!(classify("notes.txt", ""), Classification::File);
        assert_eq!(classify("a.png", "image/png"), Classification::Image);
        assert_eq!(classify("a.svg", "image/svg+xml"), Classification::UnsupportedImage);
        assert_eq!(classify("a.heic", "image/heic"), Classification::UnsupportedImage);
        assert_eq!(classify("report.pdf", "application/pdf"), Classification::File);
        assert_eq!(effective_mime("photo.jpg", "", Kind::Image), "image/jpeg");
        assert_eq!(effective_mime("blob", "", Kind::File), "application/octet-stream");
    }

    #[test]
    fn admission_reports_the_last_refusal_and_keeps_acceptable_files() {
        let (admitted, error) = admit(
            0,
            &[
                candidate("a.png", "image/png", 10),
                candidate("empty.txt", "text/plain", 0),
                candidate("huge.bin", "application/zip", MAX_FILE_BYTES + 1),
                candidate("big.png", "image/png", MAX_IMAGE_BYTES + 1),
                candidate("v.svg", "image/svg+xml", 4),
                candidate("ok.pdf", "application/pdf", 5),
            ],
        );
        assert_eq!(
            admitted.iter().map(|file| file.index).collect::<Vec<_>>(),
            vec![0, 5]
        );
        assert_eq!(
            error.unwrap(),
            "'v.svg' is not a supported image type. Attach GIF, JPEG, PNG, or WebP images."
        );
        assert_eq!(
            admit(0, &[candidate("empty.txt", "text/plain", 0)]).1.unwrap(),
            "'empty.txt' is empty or could not be read."
        );
        assert_eq!(
            admit(0, &[candidate("huge.bin", "x/y", MAX_FILE_BYTES + 1)]).1.unwrap(),
            "'huge.bin' exceeds the 50 MB attachment limit."
        );
        assert_eq!(
            admit(0, &[candidate("big.png", "image/png", MAX_IMAGE_BYTES + 1)]).1.unwrap(),
            "'big.png' exceeds the 10 MB attachment limit."
        );
    }

    #[test]
    fn admission_stops_at_the_attachment_count_including_slots_already_held() {
        let many: Vec<_> = (0..3).map(|n| candidate(&format!("{n}.txt"), "text/plain", 1)).collect();
        let (admitted, error) = admit(MAX_ATTACHMENTS - 2, &many);
        assert_eq!(admitted.len(), 2);
        assert_eq!(error.unwrap(), "You can attach up to 100 files per message.");
    }

    #[test]
    fn sizes_are_labelled_like_the_original() {
        assert_eq!(format_size(1), "1 KB");
        assert_eq!(format_size(1025), "2 KB");
        assert_eq!(format_size(3 * 1024 * 1024 + 200 * 1024), "3.2 MB");
        assert_eq!(too_large_message("a", 1), "'a' exceeds the 1 byte attachment limit.");
        assert_eq!(too_large_message("a", 2048), "'a' exceeds the 2 KB attachment limit.");
    }

    fn entry(upload: Upload) -> Attachment {
        let mut attachment = Attachment::new(
            "local".into(),
            &Admitted {
                index: 0,
                kind: Kind::File,
                name: "notes.pdf".into(),
                mime_type: "application/pdf".into(),
            },
            9,
        );
        attachment.upload = upload;
        attachment
    }

    #[test]
    fn sending_waits_on_failed_missing_and_pending_attachments_in_that_order() {
        let failed = Upload::Failed { reason: "x".into(), retry: Retry::Upload };
        assert_eq!(block_reason(&[]), None);
        assert_eq!(block_reason(&[entry(Upload::Ready)]), None);
        assert_eq!(
            block_reason(&[entry(Upload::Uploading), entry(failed.clone())]).unwrap(),
            "Retry or remove the failed attachment"
        );
        assert_eq!(
            block_reason(&[entry(failed.clone()), entry(failed)]).unwrap(),
            "Retry or remove the failed attachments"
        );
        assert_eq!(
            block_reason(&[entry(Upload::NeedsReattach)]).unwrap(),
            "Attach the missing file again or remove it"
        );
        assert_eq!(block_reason(&[entry(Upload::Verifying)]).unwrap(), "Attachment still uploading");
        assert_eq!(
            block_reason(&[entry(Upload::Uploading), entry(Upload::Verifying)]).unwrap(),
            "Attachments still uploading"
        );
    }

    #[test]
    fn only_ready_uploads_have_a_wire_form_and_totals_are_limited() {
        let mut ready = entry(Upload::Ready);
        ready.attachment_id = Some("pending-1.pdf".into());
        assert_eq!(
            wire(&[ready.clone()]).unwrap(),
            vec![json!({"type":"file","id":"pending-1.pdf","name":"notes.pdf","mimeType":"application/pdf","sizeBytes":9})]
        );
        assert!(wire(&[ready.clone(), entry(Upload::Uploading)]).is_none());
        let mut stale = entry(Upload::Failed { reason: "x".into(), retry: Retry::Upload });
        stale.attachment_id = Some("pending-2".into());
        assert!(wire(&[stale.clone()]).is_none());
        assert_eq!(Persisted::of(&stale).attachment_id, None, "a failed transfer's id is not the draft's copy");

        let image = |bytes| {
            let mut image = entry(Upload::Ready);
            image.kind = Kind::Image;
            image.mime_type = "image/png".into();
            image.size_bytes = bytes;
            image
        };
        assert!(limit_error(&vec![image(MAX_IMAGE_BYTES); 8]).is_none());
        assert!(limit_error(&vec![image(MAX_IMAGE_BYTES); 9]).unwrap().starts_with("Images can total up to 80 MiB"));
        assert!(limit_error(&vec![entry(Upload::Ready); 101]).unwrap().contains("up to 100 files"));
    }

    #[test]
    fn saved_uploads_are_verified_and_unfinished_ones_must_be_attached_again() {
        let mut ready = entry(Upload::Ready);
        ready.attachment_id = Some("pending-1".into());
        let saved = Persisted::of(&ready);
        let json = serde_json::to_string(&saved).unwrap();
        assert!(json.contains("\"type\":\"file\"") && json.contains("\"attachmentId\":\"pending-1\""));
        let restored = serde_json::from_str::<Persisted>(&json).unwrap().restore();
        assert_eq!((restored.upload.clone(), restored.attachment_id.clone()), (Upload::Verifying, Some("pending-1".into())));
        let unfinished = Persisted::of(&entry(Upload::Uploading)).restore();
        assert_eq!(unfinished.upload, Upload::NeedsReattach);
    }

    #[test]
    fn original_drafts_restore_images_for_upload_and_files_by_environment() {
        let draft = json!({
            "attachments":[
                {"id":"img","name":"s.png","mimeType":"image/png","sizeBytes":99,"dataUrl":"data:image/png;base64,iVBORw0="},
                {"id":"bad","name":"b.png","mimeType":"image/png","sizeBytes":1,"dataUrl":"data:text/plain;base64,aGk="}
            ],
            "files":[
                {"id":"here","name":"a.pdf","mimeType":"application/pdf","sizeBytes":5,"attachmentId":"pending-a","environmentId":"env"},
                {"id":"elsewhere","name":"b.pdf","mimeType":"application/pdf","sizeBytes":5,"attachmentId":"pending-b","environmentId":"other"},
                {"id":"unfinished","name":"c.pdf","mimeType":"application/pdf","sizeBytes":5}
            ]});
        let restored = restore_source(&draft, "env");
        let states: Vec<_> = restored.iter().map(|a| (a.id.as_str(), a.upload.clone())).collect();
        assert_eq!(
            states,
            vec![
                ("img", Upload::Uploading),
                ("bad", Upload::NeedsReattach),
                ("here", Upload::Verifying),
                ("elsewhere", Upload::NeedsReattach),
                ("unfinished", Upload::NeedsReattach),
            ]
        );
        assert_eq!(restored[0].bytes.as_deref(), Some(&[0x89, 0x50, 0x4e, 0x47, 0x0d][..]));
        assert_eq!(restored[0].size_bytes, 5);
        assert_eq!(restored[2].attachment_id.as_deref(), Some("pending-a"));
        assert_eq!(restored[3].attachment_id, None);
    }

    #[test]
    fn data_urls_decode_base64_and_percent_encoding() {
        assert_eq!(decode_data_url("data:text/plain;base64,aGVsbG8=").unwrap(), ("text/plain".into(), b"hello".to_vec()));
        assert_eq!(decode_data_url("data:text/plain,a%20b").unwrap().1, b"a b");
        assert_eq!(decode_base64("aGVsbG8").unwrap(), b"hello");
        assert_eq!(decode_base64("aGV sbG8=\n").unwrap(), b"hello");
        assert!(decode_base64("a*b").is_none());
        assert!(decode_data_url("https://x").is_none());
    }
}
