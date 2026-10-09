//! Signed upload URLs and their byte sink. An upload URL is the only credential the HTTP
//! route checks, so the signed claims carry everything the store needs (id, name, size, kind).
use crate::{
    attachment_service::{AttachmentService, SigningKeyError},
    attachment_store as store,
};
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use std::path::PathBuf;
use t3_contracts::{
    ATTACHMENT_UPLOAD_URL_TTL_MS, AttachmentCreateUploadUrlInput, AttachmentCreateUploadUrlResult,
};
use tokio::io::AsyncWriteExt;

pub const ATTACHMENT_UPLOAD_ROUTE_PREFIX: &str = "/api/attachments/upload";

#[derive(Debug, Clone, PartialEq)]
pub struct UploadClaims {
    pub is_file: bool,
    pub attachment_id: String,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: f64,
    pub expires_at: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreOutcome {
    Stored,
    Rejected { status: u16, detail: String },
}
fn rejected(status: u16, detail: impl Into<String>) -> StoreOutcome {
    StoreOutcome::Rejected {
        status,
        detail: detail.into(),
    }
}
fn count(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        value.to_string()
    }
}

/// An in-flight part file is removed on every exit, including a dropped request future.
struct PartFile(Option<PathBuf>);
impl Drop for PartFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl UploadClaims {
    /// The route requires a declared length equal to the signed size, when one is sent.
    pub fn content_length_matches(&self, header: Option<&str>) -> bool {
        header.is_none_or(|value| {
            let trimmed = value.trim();
            let parsed = if trimmed.is_empty() {
                Some(0.0)
            } else {
                trimmed
                    .parse::<f64>()
                    .ok()
                    .filter(|number| number.is_finite())
            };
            parsed.is_some_and(|number| number.fract() == 0.0 && number == self.size_bytes)
        })
    }
    fn from_value(value: &Value) -> Option<Self> {
        if value["version"] != 1 || value["kind"] != "attachment-upload" {
            return None;
        }
        let is_file = match value.get("type") {
            None => false,
            Some(kind) => match kind.as_str()? {
                "image" => false,
                "file" => true,
                _ => return None,
            },
        };
        Some(Self {
            is_file,
            attachment_id: value["attachmentId"].as_str()?.to_owned(),
            name: value["name"].as_str()?.to_owned(),
            mime_type: value["mimeType"].as_str()?.to_owned(),
            size_bytes: value["sizeBytes"].as_f64()?,
            expires_at: value["expiresAt"].as_f64()?,
        })
    }
    pub fn extension(&self) -> String {
        if self.is_file {
            store::attachment_file_extension(&self.name)
        } else {
            store::infer_image_extension(&self.mime_type, Some(&self.name))
        }
    }
}

impl AttachmentService {
    pub fn issue_upload_url(
        &self,
        input: &AttachmentCreateUploadUrlInput,
        now_ms: i64,
    ) -> Result<AttachmentCreateUploadUrlResult, SigningKeyError> {
        let deleted = self.sweep_if_due(now_ms);
        if deleted > 0 {
            tracing::info!(deleted, "Removed expired attachment uploads.");
        }
        let id = store::create_pending_attachment_id(
            input
                .is_file()
                .then(|| store::attachment_file_extension(input.name()))
                .as_deref(),
        );
        let expires_at = now_ms + ATTACHMENT_UPLOAD_URL_TTL_MS;
        let token = self.sign_claims(&self.upload_claims_json(input, &id, expires_at))?;
        let relative_url = format!("{ATTACHMENT_UPLOAD_ROUTE_PREFIX}/{token}");
        Ok(AttachmentCreateUploadUrlResult {
            attachment_id: serde_json::from_value(json!(id)).expect("bounded pending id"),
            relative_url: serde_json::from_value(json!(relative_url)).expect("bounded upload url"),
            expires_at,
        })
    }
    pub(crate) fn upload_claims_json(
        &self,
        input: &AttachmentCreateUploadUrlInput,
        attachment_id: &str,
        expires_at: i64,
    ) -> Value {
        json!({"version":1,"kind":"attachment-upload","type":if input.is_file(){"file"}else{"image"},"attachmentId":attachment_id,"name":input.name(),"mimeType":input.mime_type(),"sizeBytes":input.size_bytes(),"expiresAt":expires_at})
    }
    /// None for any signature, shape, or expiry failure; callers answer 404 without detail.
    pub fn validate_upload_token(&self, token: &str, now_ms: i64) -> Option<UploadClaims> {
        let mut parts = token.split('.');
        let (payload, signature, unexpected) = (parts.next()?, parts.next()?, parts.next());
        if payload.is_empty()
            || signature.is_empty()
            || unexpected.is_some_and(|rest| !rest.is_empty())
        {
            return None;
        }
        let claims = UploadClaims::from_value(&self.verify_claims(payload, signature)?)?;
        (claims.expires_at > now_ms as f64).then_some(claims)
    }
    /// Streams the body into a `.part` file and renames it into place once the byte count
    /// equals the signed size. Reading stops at the first chunk past the size.
    pub async fn store_upload<B, E>(
        &self,
        claims: &UploadClaims,
        mut body: impl Stream<Item = Result<B, E>> + Unpin,
    ) -> StoreOutcome
    where
        B: AsRef<[u8]>,
        E: std::fmt::Display,
    {
        let relative = format!("{}{}", claims.attachment_id, claims.extension());
        let final_path = store::resolve_relative_path(self.attachments_dir(), &relative);
        let part_path = store::resolve_relative_path(
            self.attachments_dir(),
            &format!("{relative}.{}.part", uuid::Uuid::new_v4()),
        );
        let (Some(final_path), Some(part_path)) = (final_path, part_path) else {
            return rejected(500, "Failed to resolve attachment path.");
        };
        let persist_failed = |cause: &dyn std::fmt::Display| {
            tracing::error!(attachment_id=%claims.attachment_id,error=%cause,"Failed to persist attachment upload.");
            rejected(500, "Failed to persist upload.")
        };
        if let Err(error) = tokio::fs::create_dir_all(final_path.parent().unwrap()).await {
            return persist_failed(&error);
        }
        let mut guard = PartFile(Some(part_path.clone()));
        let mut file = match tokio::fs::File::create(&part_path).await {
            Ok(file) => file,
            Err(error) => return persist_failed(&error),
        };
        let mut received = 0f64;
        while let Some(chunk) = body.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => return persist_failed(&error),
            };
            received += chunk.as_ref().len() as f64;
            if received > claims.size_bytes {
                break;
            }
            if let Err(error) = file.write_all(chunk.as_ref()).await {
                return persist_failed(&error);
            }
        }
        if received != claims.size_bytes {
            return rejected(
                400,
                format!(
                    "Body was {} bytes, expected {}.",
                    count(received),
                    count(claims.size_bytes)
                ),
            );
        }
        if let Err(error) = file.flush().await {
            return persist_failed(&error);
        }
        drop(file);
        if let Err(error) = tokio::fs::rename(&part_path, &final_path).await {
            return persist_failed(&error);
        }
        guard.0 = None;
        StoreOutcome::Stored
    }
    /// Only staged uploads can be deleted by id; thread-owned copies are left alone.
    pub fn delete_pending(&self, attachment_id: &str) {
        if !store::is_pending_attachment_id(attachment_id) {
            return;
        }
        if let Some(path) =
            store::resolve_attachment_path_by_id(self.attachments_dir(), attachment_id)
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachment_service::testing::{access_fixture, fixture_service, service};
    use futures_util::stream;
    use std::convert::Infallible;

    fn image(size: u64) -> AttachmentCreateUploadUrlInput {
        serde_json::from_value(
            json!({"name":"screenshot.png","mimeType":"image/png","sizeBytes":size}),
        )
        .unwrap()
    }
    fn file(name: &str, size: u64) -> AttachmentCreateUploadUrlInput {
        serde_json::from_value(
            json!({"type":"file","name":name,"mimeType":"application/pdf","sizeBytes":size}),
        )
        .unwrap()
    }
    fn token_of(issued: &AttachmentCreateUploadUrlResult) -> String {
        issued
            .relative_url
            .0
            .as_str()
            .strip_prefix("/api/attachments/upload/")
            .unwrap()
            .to_owned()
    }
    fn body(chunks: &[&[u8]]) -> impl Stream<Item = Result<Vec<u8>, Infallible>> + Unpin {
        stream::iter(
            chunks
                .iter()
                .map(|chunk| Ok(chunk.to_vec()))
                .collect::<Vec<_>>(),
        )
    }
    fn names(files: &AttachmentService) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(files.attachments_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn original_signer_and_validator_agree_on_generated_upload_urls() {
        let fixture = access_fixture();
        let (_directory, files) = fixture_service(&fixture);
        for row in fixture["uploads"].as_array().unwrap() {
            let input: AttachmentCreateUploadUrlInput =
                serde_json::from_value(row["input"].clone()).unwrap();
            let issued = &row["issued"];
            let expires_at = issued["expiresAt"].as_i64().unwrap();
            let token = issued["relativeUrl"]
                .as_str()
                .unwrap()
                .strip_prefix("/api/attachments/upload/")
                .unwrap();
            // Byte-identical claims and signature for the same id, input, and expiry.
            let claims = files.upload_claims_json(
                &input,
                issued["attachmentId"].as_str().unwrap(),
                expires_at,
            );
            assert_eq!(claims.to_string(), row["payload"].as_str().unwrap());
            assert_eq!(files.sign_claims(&claims).unwrap(), token);
            let validated = files.validate_upload_token(token, expires_at - 1).unwrap();
            assert_eq!(validated.attachment_id, row["claims"]["attachmentId"]);
            assert_eq!(validated.is_file, row["claims"]["type"] == "file");
            assert_eq!(
                validated.size_bytes,
                row["claims"]["sizeBytes"].as_f64().unwrap()
            );
            assert_eq!(
                files.validate_upload_token(token, expires_at),
                None,
                "expiry instant is expired"
            );
        }
    }

    #[test]
    fn issued_urls_sign_the_metadata_and_reject_tampering() {
        let (_directory, files) = service();
        let issued = files.issue_upload_url(&image(6), 1_000_000).unwrap();
        assert!(store::is_pending_attachment_id(
            issued.attachment_id.0.as_str()
        ));
        assert_eq!(issued.expires_at, 1_000_000 + ATTACHMENT_UPLOAD_URL_TTL_MS);
        let token = token_of(&issued);
        let claims = files.validate_upload_token(&token, 1_000_001).unwrap();
        assert_eq!(
            (
                claims.name.as_str(),
                claims.mime_type.as_str(),
                claims.size_bytes,
                claims.is_file
            ),
            ("screenshot.png", "image/png", 6.0, false)
        );
        let (payload, signature) = token.split_once('.').unwrap();
        for bad in [
            format!("{payload}x.{signature}"),
            format!("{token}.extra"),
            "garbage".to_owned(),
            String::new(),
            format!("{payload}."),
        ] {
            assert_eq!(files.validate_upload_token(&bad, 1_000_001), None, "{bad}");
        }
        // A trailing empty segment is not an unexpected one, as in the original.
        assert!(
            files
                .validate_upload_token(&format!("{token}."), 1_000_001)
                .is_some()
        );
        assert_eq!(files.validate_upload_token(&token, issued.expires_at), None);
        assert_eq!(
            files.validate_upload_token(&token, issued.expires_at + 60_000),
            None
        );
    }

    #[test]
    fn tokens_issued_before_file_support_validate_as_images() {
        let (_directory, files) = service();
        let issued = files.issue_upload_url(&image(6), 0).unwrap();
        let legacy = json!({"version":1,"kind":"attachment-upload","attachmentId":issued.attachment_id.0.as_str(),"name":"screenshot.png","mimeType":"image/png","sizeBytes":6,"expiresAt":issued.expires_at});
        let token = files.sign_claims(&legacy).unwrap();
        let claims = files.validate_upload_token(&token, 1).unwrap();
        assert!(!claims.is_file);
        // Other signed kinds under the shared key never validate as uploads.
        let asset_like = files.sign_claims(&json!({"version":1,"kind":"attachment","attachmentId":"x","expiresAt":issued.expires_at})).unwrap();
        assert_eq!(files.validate_upload_token(&asset_like, 1), None);
    }

    #[test]
    fn issuing_sweeps_expired_pending_uploads_once_per_interval() {
        let (_directory, files) = service();
        let stale = files
            .attachments_dir()
            .join("pending-00000000-0000-4000-8000-0000000000cc.png");
        std::fs::write(&stale, "pixels").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();
        let day_and_hour = 25 * 3_600_000;
        files.issue_upload_url(&image(6), day_and_hour).unwrap();
        assert!(!stale.exists());
        // Within the 15 minute interval a newly stale file survives until the next sweep.
        let second = files
            .attachments_dir()
            .join("pending-00000000-0000-4000-8000-0000000000dd.png");
        std::fs::write(&second, "pixels").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&second)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();
        files
            .issue_upload_url(&image(6), day_and_hour + 60_000)
            .unwrap();
        assert!(second.exists());
        files
            .issue_upload_url(&image(6), day_and_hour + 16 * 60_000)
            .unwrap();
        assert!(!second.exists());
    }

    #[tokio::test]
    async fn stores_exact_bytes_without_leaving_part_files() {
        let (_directory, files) = service();
        let issued = files.issue_upload_url(&image(6), 0).unwrap();
        let claims = files.validate_upload_token(&token_of(&issued), 1).unwrap();
        assert_eq!(
            files.store_upload(&claims, body(&[&[1, 2, 3]])).await,
            StoreOutcome::Rejected {
                status: 400,
                detail: "Body was 3 bytes, expected 6.".into()
            }
        );
        assert_eq!(names(&files), Vec::<String>::new());
        assert_eq!(
            files
                .store_upload(&claims, body(&[&[1, 2], &[3], &[4, 5, 6]]))
                .await,
            StoreOutcome::Stored
        );
        assert_eq!(
            names(&files),
            vec![format!("{}.png", issued.attachment_id.0.as_str())]
        );
        assert_eq!(
            std::fs::read(
                files
                    .attachments_dir()
                    .join(format!("{}.png", issued.attachment_id.0.as_str()))
            )
            .unwrap(),
            vec![1, 2, 3, 4, 5, 6]
        );
    }

    #[tokio::test]
    async fn rejects_bodies_past_the_signed_size_without_leftovers() {
        let (_directory, files) = service();
        let issued = files.issue_upload_url(&image(6), 0).unwrap();
        let claims = files.validate_upload_token(&token_of(&issued), 1).unwrap();
        assert_eq!(
            files.store_upload(&claims, body(&[&[0; 7]])).await,
            StoreOutcome::Rejected {
                status: 400,
                detail: "Body was 7 bytes, expected 6.".into()
            }
        );
        // Reading stops at the first chunk that crosses the limit.
        assert_eq!(
            files
                .store_upload(&claims, body(&[&[0; 4], &[0; 4], &[0; 4]]))
                .await,
            StoreOutcome::Rejected {
                status: 400,
                detail: "Body was 8 bytes, expected 6.".into()
            }
        );
        assert_eq!(names(&files), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_failing_body_is_a_500_and_leaves_nothing() {
        let (_directory, files) = service();
        let issued = files.issue_upload_url(&image(6), 0).unwrap();
        let claims = files.validate_upload_token(&token_of(&issued), 1).unwrap();
        let failing = stream::iter(vec![Ok(vec![1u8, 2]), Err("connection reset")]);
        assert_eq!(
            files.store_upload(&claims, failing).await,
            StoreOutcome::Rejected {
                status: 500,
                detail: "Failed to persist upload.".into()
            }
        );
        assert_eq!(names(&files), Vec::<String>::new());
    }

    #[tokio::test]
    async fn generic_files_keep_their_extension_and_delete_removes_only_pending_copies() {
        let (_directory, files) = service();
        let issued = files.issue_upload_url(&file("report.PDF", 6), 0).unwrap();
        assert!(issued.attachment_id.0.as_str().ends_with("-pdf"));
        let claims = files.validate_upload_token(&token_of(&issued), 1).unwrap();
        assert_eq!(
            files
                .store_upload(&claims, body(&[&[1, 2, 3], &[4, 5, 6]]))
                .await,
            StoreOutcome::Stored
        );
        assert_eq!(
            names(&files),
            vec![format!("{}.pdf", issued.attachment_id.0.as_str())]
        );
        files.delete_pending(issued.attachment_id.0.as_str());
        assert_eq!(names(&files), Vec::<String>::new());

        let uuid = "00000000-0000-4000-8000-0000000000dd";
        let pending = files.attachments_dir().join(format!("pending-{uuid}.png"));
        let claimed = files.attachments_dir().join(format!("thread-1-{uuid}.png"));
        std::fs::write(&pending, "pixels").unwrap();
        std::fs::write(&claimed, "pixels").unwrap();
        files.delete_pending(&format!("pending-{uuid}"));
        files.delete_pending(&format!("pending-{uuid}"));
        files.delete_pending(&format!("thread-1-{uuid}"));
        files.delete_pending("../../outside");
        assert!(!pending.exists());
        assert!(claimed.exists());
    }

    #[tokio::test]
    async fn an_interrupted_upload_removes_its_part_file() {
        let (_directory, files) = service();
        let issued = files.issue_upload_url(&image(6), 0).unwrap();
        let claims = files.validate_upload_token(&token_of(&issued), 1).unwrap();
        let (next_requested, requested) = tokio::sync::oneshot::channel::<()>();
        let mut signal = Some(next_requested);
        // After one written chunk the stream announces it was polled again, then never yields.
        let stalled = stream::iter(vec![Ok::<_, Infallible>(vec![1u8, 2, 3])]).chain(stream::once(
            async move {
                let _ = signal.take().unwrap().send(());
                std::future::pending::<Result<Vec<u8>, Infallible>>().await
            },
        ));
        let upload_files = files.clone();
        let upload =
            tokio::spawn(
                async move { upload_files.store_upload(&claims, Box::pin(stalled)).await },
            );
        requested.await.unwrap();
        assert_eq!(
            names(&files)
                .iter()
                .filter(|name| name.ends_with(".part"))
                .count(),
            1
        );
        upload.abort();
        assert!(upload.await.unwrap_err().is_cancelled());
        assert_eq!(names(&files), Vec::<String>::new());
    }

    #[test]
    fn content_length_must_equal_the_signed_size_when_sent() {
        let (_directory, files) = service();
        let issued = files.issue_upload_url(&image(6), 0).unwrap();
        let claims = files.validate_upload_token(&token_of(&issued), 1).unwrap();
        for (header, ok) in [
            (None, true),
            (Some("6"), true),
            (Some(" 6 "), true),
            (Some("5"), false),
            (Some("7"), false),
            (Some("6.5"), false),
            (Some(""), false),
            (Some("abc"), false),
            (Some("NaN"), false),
            (Some("inf"), false),
        ] {
            assert_eq!(claims.content_length_matches(header), ok, "{header:?}");
        }
    }
}
