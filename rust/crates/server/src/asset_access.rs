//! Signed, expiring asset URLs (apps/server/src/assets/AssetAccess.ts). Every resource kind
//! issues a claim the server signs, and the claim is checked again, against the disk as it is
//! then, whenever the URL is used. Attachments, workspace and media files, project favicons,
//! tool-output images, application icons and GitHub media are all served this way; the
//! disk-backed rules live in `asset_workspace`.
use crate::{
    asset_workspace as workspace,
    attachment_service::{AttachmentService, SigningKeyError},
    attachment_store as store,
    image_dimensions::read_image_dimensions_from_file,
    persistence::Store,
};
use regex::Regex;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::{Arc, LazyLock}};
use t3_contracts::{AssetResource, AttachmentDisposition};

pub const ASSET_ROUTE_PREFIX: &str = "/api/assets";
const ASSET_TOKEN_TTL_MS: i64 = 60 * 60 * 1000;

static VIDEO_MIME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i-u)^video/[A-Za-z0-9_!#$&^.+-]+$").unwrap());

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedAsset {
    File {
        path: PathBuf,
        download: bool,
        file_name: Option<String>,
        mime_type: Option<String>,
        /// A media file is served from the descriptor that was checked, not from its path.
        file: Option<Arc<workspace::OpenMedia>>,
    },
    /// An image a tool returned inline, decoded.
    Bytes { bytes: Vec<u8>, mime_type: String },
    /// Media on GitHub's own hosts, fetched with the server's GitHub credential.
    GithubMedia { url: String, cwd: String, expires_at: i64 },
}

/// Failures of `assets.createUrl`, named after the contract's `AssetAccessError` members.
#[derive(Debug)]
pub enum AssetError {
    AttachmentNotFound(Value),
    SigningKey { resource: Value, cause: String },
    /// Any other member that names the resource, with the cause it carries (if it has one).
    Refused { tag: &'static str, resource: Value, cause: Option<Value> },
    /// `AssetGitHubMediaUrlValidationError`, which names nothing.
    GithubUrl,
}
impl AssetError {
    pub fn wire(&self) -> Value {
        match self {
            Self::AttachmentNotFound(resource) => {
                json!({"_tag":"AssetAttachmentNotFoundError","resource":resource})
            }
            Self::SigningKey { resource, cause } => {
                json!({"_tag":"AssetSigningKeyLoadError","resource":resource,"cause":{"name":"Error","message":cause}})
            }
            Self::Refused { tag, resource, cause } => {
                let mut wire = json!({"_tag":tag,"resource":resource});
                if let Some(cause) = cause {
                    wire["cause"] = cause.clone();
                }
                wire
            }
            Self::GithubUrl => json!({"_tag":"AssetGitHubMediaUrlValidationError"}),
        }
    }
}

/// JavaScript `encodeURIComponent`.
pub fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}
fn inline_preview_mime_type(extension: &str) -> Option<&'static str> {
    match extension {
        "pdf" => Some("application/pdf"),
        "html" | "htm" => Some("text/html"),
        _ => store::audio_mime_type_from_extension(&format!(".{extension}")),
    }
}

impl AttachmentService {
    /// The URL for a resource that needs nothing from the rest of the server: an attachment, an
    /// application icon, GitHub media, or a file named by an absolute path.
    pub fn create_asset_url(
        &self,
        resource: &AssetResource,
        now_ms: i64,
    ) -> Result<Value, AssetError> {
        self.issue_asset_url(resource, &workspace::IssueContext::default(), None, now_ms)
    }

    /// `issueAssetUrl`. `context` carries what the socket handler resolved for the resource (the
    /// workspace a thread works in, a project's saved icon); `store` is read for tool images.
    pub fn issue_asset_url(
        &self,
        resource: &AssetResource,
        context: &workspace::IssueContext,
        store: Option<&Store>,
        now_ms: i64,
    ) -> Result<Value, AssetError> {
        let wire_resource = serde_json::to_value(resource).expect("resources serialize");
        let refuse = |refusal: workspace::Refusal| AssetError::Refused { tag: refusal.tag, resource: wire_resource.clone(), cause: refusal.cause };
        let expires_at = now_ms + ASSET_TOKEN_TTL_MS;
        let issued = match resource {
            AssetResource::Attachment { .. } => return self.issue_attachment_url(resource, wire_resource, expires_at),
            AssetResource::WorkspaceFile { .. } | AssetResource::MediaFile { .. } | AssetResource::DraftWorkspaceFile { .. } => {
                workspace::issue_file_backed(&wire_resource, context, expires_at).map_err(refuse)?
            }
            AssetResource::ProjectFavicon { .. } => workspace::issue_project_favicon(&wire_resource, context, now_ms).map_err(refuse)?,
            AssetResource::ToolOutputImage { thread_id, item_id, index } => {
                let image = match store {
                    Some(store) => workspace::read_tool_output_image(store, thread_id.as_str(), item_id.as_str(), index.0),
                    None => Ok(None),
                }
                .map_err(|error| refuse(workspace::Refusal { tag: "AssetWorkspaceAssetInspectionError", cause: Some(workspace::defect("Error", error)) }))?
                .ok_or_else(|| refuse(workspace::Refusal { tag: "AssetWorkspaceAssetNotFoundError", cause: None }))?;
                workspace::Issued {
                    claims: json!({"version":1,"kind":"tool-output-image","threadId":thread_id.as_str(),"itemId":item_id.as_str(),"index":index.0,"expiresAt":expires_at}),
                    // The allowed types are all `image/<extension>`.
                    file_name: format!("image-{}.{}", index.0 + 1, image.mime_type.trim_start_matches("image/")),
                    image_dimensions: crate::image_dimensions::read_image_dimensions(&image.bytes),
                    source_path: None,
                }
            }
            AssetResource::NativeAppIcon { app } => workspace::Issued {
                claims: json!({"version":1,"kind":"native-app-icon","app":serde_json::to_value(app).expect("app reference serializes"),"expiresAt":expires_at}),
                file_name: "native-app-icon.png".into(),
                image_dimensions: None,
                source_path: None,
            },
            AssetResource::GithubMedia { cwd, url } => {
                let fetch_url = workspace::github_media_fetch_url(url.0.as_str()).ok_or(AssetError::GithubUrl)?;
                workspace::Issued {
                    claims: json!({"version":1,"kind":"github-media","url":fetch_url,"cwd":cwd.0.as_str(),"expiresAt":expires_at}),
                    file_name: workspace::github_media_file_name(&fetch_url),
                    image_dimensions: None,
                    source_path: None,
                }
            }
        };
        let expires_at = issued.claims["expiresAt"].as_i64().unwrap_or(expires_at);
        let token = self.sign_claims(&issued.claims).map_err(|SigningKeyError(cause)| AssetError::SigningKey { resource: wire_resource, cause })?;
        let mut result = json!({"relativeUrl":format!("{ASSET_ROUTE_PREFIX}/{token}/{}",encode_uri_component(&issued.file_name)),"expiresAt":expires_at});
        if let Some(source) = issued.source_path {
            result["sourcePath"] = json!(source);
        }
        if let Some((width, height)) = issued.image_dimensions {
            result["imageDimensions"] = json!({"width":width,"height":height});
        }
        Ok(result)
    }

    fn issue_attachment_url(&self, resource: &AssetResource, wire_resource: Value, expires_at: i64) -> Result<Value, AssetError> {
        let AssetResource::Attachment { attachment_id, file_name, mime_type, disposition } = resource else { unreachable!("attachment only") };
        let attachment_id = attachment_id.0.as_str();
        let path = store::resolve_attachment_path_by_id(self.attachments_dir(), attachment_id)
            .ok_or_else(|| AssetError::AttachmentNotFound(wire_resource.clone()))?;
        // Generic files carry their extension inside the id (that shape resolves the on-disk
        // path); images do not. Videos and images render inline. Other generic files download
        // unless a viewer requests a supported document or audio format inline.
        let extension = store::parse_attachment_file_extension(attachment_id);
        let is_generic = extension.is_some();
        let requested_mime = mime_type.as_ref().map(|mime| mime.0.as_str());
        let video_mime = requested_mime
            .map(|mime| mime.split(';').next().unwrap_or("").trim())
            .unwrap_or("");
        let is_video = VIDEO_MIME.is_match(video_mime);
        let inline_preview = match (disposition, &extension) {
            (Some(AttachmentDisposition::Inline), Some(extension)) => {
                inline_preview_mime_type(extension)
            }
            _ => None,
        };
        let dimensions = (!is_generic)
            .then(|| workspace::read_dimensions_from_header(&path.to_string_lossy()).or_else(|| read_image_dimensions_from_file(&path)))
            .flatten();
        let mut claims = json!({"version":1,"kind":"attachment","attachmentId":attachment_id});
        if is_generic && !is_video && inline_preview.is_none() {
            claims["download"] = json!(true);
        }
        if let Some(file_name) = file_name {
            claims["fileName"] = json!(file_name.0.as_str());
        }
        if let Some(mime) = inline_preview {
            claims["mimeType"] = json!(mime);
        } else if let Some(mime) = requested_mime {
            claims["mimeType"] = json!(if is_video { video_mime } else { mime });
        }
        claims["expiresAt"] = json!(expires_at);
        let token = self
            .sign_claims(&claims)
            .map_err(|SigningKeyError(cause)| AssetError::SigningKey {
                resource: wire_resource,
                cause,
            })?;
        let display_name = file_name
            .as_ref()
            .map(|name| name.0.as_str().to_owned())
            .unwrap_or_else(|| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        let mut result = json!({"relativeUrl":format!("{ASSET_ROUTE_PREFIX}/{token}/{}",encode_uri_component(&display_name)),"expiresAt":expires_at});
        if let Some((width, height)) = dimensions {
            result["imageDimensions"] = json!({"width":width,"height":height});
        }
        Ok(result)
    }

    /// The asset a token grants right now, or None: a bad signature, an expired or unknown
    /// claim, and a file that has since disappeared are indistinguishable to the caller.
    pub fn resolve_asset(&self, token: &str, now_ms: i64) -> Option<ResolvedAsset> {
        self.resolve_asset_request(token, "", now_ms, None)
    }

    /// `resolveAsset`. `relative_path` is the URL after the token, still percent-encoded: a
    /// workspace URL serves siblings of the file it was issued for, so the name matters there.
    pub fn resolve_asset_request(&self, token: &str, relative_path: &str, now_ms: i64, store: Option<&Store>) -> Option<ResolvedAsset> {
        let mut parts = token.split('.');
        let (payload, signature) = (parts.next()?, parts.next()?);
        if payload.is_empty() || signature.is_empty() {
            return None;
        }
        let claims = self.verify_claims(payload, signature)?;
        if claims["version"] != 1 || claims["expiresAt"].as_f64()? <= now_ms as f64 {
            return None;
        }
        match claims["kind"].as_str()? {
            "attachment" => self.resolve_attachment(&claims),
            "github-media" => Some(ResolvedAsset::GithubMedia {
                url: claims["url"].as_str()?.to_owned(),
                cwd: claims["cwd"].as_str()?.to_owned(),
                expires_at: claims["expiresAt"].as_i64()?,
            }),
            "tool-output-image" => {
                let (thread, item, index) = (claims["threadId"].as_str()?, claims["itemId"].as_str()?, claims["index"].as_u64()?);
                match workspace::read_tool_output_image(store?, thread, item, index) {
                    Ok(image) => image.map(|image| ResolvedAsset::Bytes { bytes: image.bytes, mime_type: image.mime_type }),
                    Err(error) => {
                        tracing::error!(thread, item, error, "Failed to read tool output image.");
                        None
                    }
                }
            }
            "native-app-icon" => {
                let cache = self.attachments_dir().parent()?.join("caches/native-app-icons");
                crate::asset_native_icon::resolve(&claims["app"], &cache)
                    .map(|path| ResolvedAsset::File { path, download: false, file_name: None, mime_type: None, file: None })
            }
            _ => match workspace::resolve_claim(&claims, relative_path)? {
                workspace::Resolved::File { path, mime_type, file } => Some(ResolvedAsset::File { path, download: false, file_name: None, mime_type, file }),
            },
        }
    }

    fn resolve_attachment(&self, claims: &Value) -> Option<ResolvedAsset> {
        let optional_string = |key: &str| match claims.get(key) {
            None => Some(None),
            Some(value) => value.as_str().map(|text| Some(text.to_owned())),
        };
        let attachment_id = claims["attachmentId"].as_str()?;
        let download = match claims.get("download") {
            None => false,
            Some(value) => value.as_bool()?,
        };
        let (file_name, mime_type) = (optional_string("fileName")?, optional_string("mimeType")?);
        let path = store::resolve_attachment_path_by_id(self.attachments_dir(), attachment_id)?;
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => Some(ResolvedAsset::File {
                path,
                download,
                file_name,
                mime_type,
                file: None,
            }),
            Ok(_) => None,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::error!(attachment_id,error=%error,"Failed to inspect attachment asset.");
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachment_service::testing::{access_fixture, fixture_service, service};

    #[test]
    fn original_issuer_and_resolver_agree_on_generated_attachment_urls() {
        let fixture = access_fixture();
        let (_directory, files) = fixture_service(&fixture);
        let mut issued_count = 0;
        for row in fixture["assets"].as_array().unwrap() {
            let resource: AssetResource = serde_json::from_value(row["resource"].clone()).unwrap();
            if let Some(error) = row.get("error") {
                let actual = files
                    .create_asset_url(&resource, 1_791_000_000_000)
                    .unwrap_err();
                assert_eq!(&actual.wire(), error, "{}", row["resource"]);
                continue;
            }
            let expires_at = row["issued"]["expiresAt"].as_i64().unwrap();
            let url = files
                .create_asset_url(&resource, expires_at - ASSET_TOKEN_TTL_MS)
                .unwrap();
            assert_eq!(url, row["issued"], "{}", row["resource"]);
            serde_json::from_value::<t3_contracts::AssetCreateUrlResult>(url.clone()).unwrap();
            let suffix = url["relativeUrl"]
                .as_str()
                .unwrap()
                .strip_prefix("/api/assets/")
                .unwrap();
            let token = suffix.split_once('/').unwrap().0;
            let resolved = files.resolve_asset(token, expires_at - 1).unwrap();
            let ResolvedAsset::File {
                path,
                download,
                file_name,
                mime_type,
                ..
            } = resolved
            else {
                panic!("an attachment resolves to a file");
            };
            let expected = &row["resolved"];
            assert_eq!(
                path.strip_prefix(files.attachments_dir())
                    .unwrap()
                    .to_str()
                    .unwrap(),
                expected["file"]
            );
            assert_eq!(
                download,
                expected["download"] == true,
                "{}",
                row["resource"]
            );
            assert_eq!(file_name.as_deref(), expected["fileName"].as_str());
            assert_eq!(mime_type.as_deref(), expected["mimeType"].as_str());
            // The expiry instant itself is already expired, as in the original.
            assert_eq!(files.resolve_asset(token, expires_at), None);
            issued_count += 1;
        }
        assert!(issued_count >= 20, "fixture lost coverage");
    }

    #[test]
    fn tampered_foreign_and_orphaned_tokens_resolve_to_nothing() {
        let fixture = access_fixture();
        let (_directory, files) = fixture_service(&fixture);
        let row = &fixture["assets"][0];
        let expires_at = row["issued"]["expiresAt"].as_i64().unwrap();
        let suffix = row["issued"]["relativeUrl"]
            .as_str()
            .unwrap()
            .strip_prefix("/api/assets/")
            .unwrap();
        let token = suffix.split_once('/').unwrap().0;
        let (payload, signature) = token.split_once('.').unwrap();
        let now = expires_at - 1;
        assert!(files.resolve_asset(token, now).is_some());
        // Extra dot segments are ignored, as the original splits and takes two.
        assert!(
            files
                .resolve_asset(&format!("{token}.extra"), now)
                .is_some()
        );
        for bad in [
            format!("{payload}x.{signature}"),
            format!("{payload}.{}x", signature),
            format!("{payload}."),
            format!(".{signature}"),
            payload.to_owned(),
            String::new(),
            "garbage.garbage".into(),
        ] {
            assert_eq!(files.resolve_asset(&bad, now), None, "{bad}");
        }
        // Another environment's key rejects the token.
        let (_other_directory, other) = service();
        assert_eq!(other.resolve_asset(token, now), None);
        // A signed token whose file is gone no longer resolves.
        std::fs::remove_file(
            files
                .attachments_dir()
                .join(fixture["stage"][0]["file"].as_str().unwrap()),
        )
        .unwrap();
        assert_eq!(files.resolve_asset(token, now), None);
    }

    #[test]
    fn github_media_that_is_not_github_hosted_is_refused() {
        let (_directory, files) = service();
        let resource: AssetResource = serde_json::from_value(json!({"_tag":"github-media","cwd":"/r","url":"https://example.com/a.png"})).unwrap();
        assert_eq!(files.create_asset_url(&resource, 0).unwrap_err().wire(), json!({"_tag":"AssetGitHubMediaUrlValidationError"}));
    }

    #[test]
    fn uri_component_encoding_matches_javascript() {
        assert_eq!(
            encode_uri_component("a b/é😀(1)*'!~-_.\""),
            "a%20b%2F%C3%A9%F0%9F%98%80(1)*'!~-_.%22"
        );
    }
}
