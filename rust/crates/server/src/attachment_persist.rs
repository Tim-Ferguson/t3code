//! `assets.persistChatAttachments`: inline base64 images saved under deterministic ids,
//! so retrying the same message writes the same files.
use crate::{attachment_service::AttachmentService, attachment_store as store};
use base64::{
    Engine,
    alphabet::STANDARD,
    engine::{GeneralPurpose, GeneralPurposeConfig},
};
use serde_json::{Value, json};
use t3_contracts::PersistChatAttachmentsInput;

/// Wire form of `PersistChatAttachmentsError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PersistError {
    pub message: String,
    pub cause: Option<String>,
}
impl PersistError {
    fn new(message: String) -> Self {
        Self {
            message,
            cause: None,
        }
    }
    pub fn wire(&self) -> Value {
        let mut value = json!({"_tag":"PersistChatAttachmentsError","message":self.message});
        if let Some(cause) = &self.cause {
            value["cause"] = json!({"name":"Error","message":cause});
        }
        value
    }
}

// Node's decoder ignores non-zero trailing bits; the strict default would reject them.
const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &STANDARD,
    GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
);

impl AttachmentService {
    pub fn persist_chat_attachments(
        &self,
        input: &PersistChatAttachmentsInput,
    ) -> Result<Vec<Value>, PersistError> {
        let mut persisted = Vec::new();
        for (index, attachment) in input.attachments.iter().enumerate() {
            let name = attachment.name.0.as_str();
            let mime_type = attachment.mime_type.as_str();
            let parsed = store::parse_base64_data_url(attachment.data_url.0.as_str());
            let Some((_, base64)) = parsed.filter(|(mime, _)| *mime == mime_type.to_lowercase())
            else {
                return Err(PersistError::new(format!(
                    "Attachment {name} has an invalid image payload."
                )));
            };
            let bytes = LENIENT
                .decode(base64.as_bytes())
                .map_err(|error| PersistError {
                    message: format!("Attachment {name} is not valid base64."),
                    cause: Some(error.to_string()),
                })?;
            if bytes.len() as i64 != attachment.size_bytes.0 {
                return Err(PersistError::new(format!(
                    "Attachment {name} size does not match its payload."
                )));
            }
            let Some(id) = store::create_deterministic_attachment_id(
                input.thread_id.as_str(),
                &format!("{}:{index}", input.message_id.as_str()),
            ) else {
                return Err(PersistError::new(
                    "Could not allocate an attachment identifier.".into(),
                ));
            };
            let value = json!({"type":"image","id":id,"name":name,"mimeType":mime_type,"sizeBytes":attachment.size_bytes.0});
            let relative =
                store::attachment_relative_path(&store::AttachmentRef::from_value(&value).unwrap())
                    .expect("image attachments always have a path");
            let path = self.attachments_dir().join(relative);
            std::fs::create_dir_all(self.attachments_dir())
                .and_then(|()| std::fs::write(&path, &bytes))
                .map_err(|error| PersistError {
                    message: format!("Could not persist attachment {name}."),
                    cause: Some(error.to_string()),
                })?;
            persisted.push(value);
        }
        Ok(persisted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachment_service::testing::service;

    fn input(attachments: Value) -> PersistChatAttachmentsInput {
        serde_json::from_value(
            json!({"threadId":"Thread 1","messageId":"message-1","attachments":attachments}),
        )
        .unwrap()
    }
    fn png(bytes: &[u8]) -> Value {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        json!({"type":"image","name":"a.png","mimeType":"image/png","sizeBytes":bytes.len(),"dataUrl":format!("data:image/png;base64,{encoded}")})
    }

    #[test]
    fn persists_images_under_deterministic_ids_so_retries_write_the_same_files() {
        let (_directory, files) = service();
        let request = input(json!([png(&[1, 2, 3]), png(&[4, 5])]));
        let first = files.persist_chat_attachments(&request).unwrap();
        assert_eq!(first.len(), 2);
        assert_ne!(first[0]["id"], first[1]["id"]);
        for (attachment, bytes) in first.iter().zip([vec![1u8, 2, 3], vec![4, 5]]) {
            assert!(attachment["id"].as_str().unwrap().starts_with("thread-1-"));
            assert_eq!(attachment["type"], "image");
            assert_eq!(
                std::fs::read(
                    files
                        .attachments_dir()
                        .join(format!("{}.png", attachment["id"].as_str().unwrap()))
                )
                .unwrap(),
                bytes
            );
        }
        assert_eq!(files.persist_chat_attachments(&request).unwrap(), first);
        assert_eq!(
            std::fs::read_dir(files.attachments_dir()).unwrap().count(),
            2
        );
        serde_json::from_value::<t3_contracts::PersistChatAttachmentsResult>(
            json!({"attachments":first}),
        )
        .unwrap();
    }

    #[test]
    fn refuses_inconsistent_payloads_with_the_original_messages() {
        let (_directory, files) = service();
        let message = |attachment: Value| {
            files
                .persist_chat_attachments(&input(json!([attachment])))
                .unwrap_err()
                .message
        };
        let mut wrong_mime = png(&[1]);
        wrong_mime["mimeType"] = json!("image/jpeg");
        assert_eq!(
            message(wrong_mime),
            "Attachment a.png has an invalid image payload."
        );
        let mut not_data = png(&[1]);
        not_data["dataUrl"] = json!("https://example.test/a.png");
        assert_eq!(
            message(not_data),
            "Attachment a.png has an invalid image payload."
        );
        let mut wrong_size = png(&[1, 2]);
        wrong_size["sizeBytes"] = json!(3);
        assert_eq!(
            message(wrong_size),
            "Attachment a.png size does not match its payload."
        );
        let mut unpadded = png(&[1, 2]);
        unpadded["dataUrl"] = json!("data:image/png;base64,AQI");
        assert_eq!(
            message(unpadded),
            "Attachment a.png has an invalid image payload."
        );
        assert_eq!(
            std::fs::read_dir(files.attachments_dir()).unwrap().count(),
            0
        );
        let blank_thread: PersistChatAttachmentsInput = serde_json::from_value(
            json!({"threadId":"***","messageId":"m","attachments":[png(&[1])]}),
        )
        .unwrap();
        assert_eq!(
            files
                .persist_chat_attachments(&blank_thread)
                .unwrap_err()
                .message,
            "Could not allocate an attachment identifier."
        );
        assert_eq!(
            files
                .persist_chat_attachments(&blank_thread)
                .unwrap_err()
                .wire()["_tag"],
            "PersistChatAttachmentsError"
        );
    }
}
