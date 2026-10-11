//! One-shot text generation through OpenCode (port of `textGeneration/OpenCodeTextGeneration.ts` for a 1.x server and
//! `OpenCode2TextGeneration.ts` for a 2.x one). The instance's probe says which server it is; a failed probe keeps 1.x,
//! whose connection reports the failure. Both run the request as a prompt in a temporary session with every tool
//! refused, on the instance's own server (the configured one, or the one T3 Code owns and starts when none runs).
use crate::{
    opencode_adapter::{Frame, start_reader},
    opencode_api::Api,
    opencode_instance::OpenCodeInstance,
    opencode_server::Generation,
    scheduled_tasks::SchedulerClock,
    text_generation::{Request, TextGenerationError},
};
use indexmap::IndexMap;
use reqwest::Method;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

/// `OPENCODE_NATIVE_FILE_PART_MAX_BYTES`.
const NATIVE_FILE_PART_MAX_BYTES: u64 = 20 * 1024 * 1024;
const REMOVE_TIMEOUT: Duration = Duration::from_secs(5);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);

/// `parseOpenCodeModelSlug`.
fn parse_model_slug(slug: &str) -> Option<(String, String)> {
    let trimmed = crate::text_generation::js_trim(slug);
    let separator = trimmed.find('/')?;
    (separator > 0 && separator < trimmed.len() - 1).then(|| (trimmed[..separator].to_owned(), trimmed[separator + 1..].to_owned()))
}

fn option_value<'a>(selection: &'a Value, id: &str) -> Option<&'a str> {
    selection["options"].as_array()?.iter().find(|option| option["id"] == id)?["value"].as_str().filter(|value| !value.is_empty())
}

/// The images of a request that have a file on disk, in order.
fn image_paths(request: &Request, attachments_dir: Option<&Path>) -> Vec<(Value, std::path::PathBuf)> {
    let Some(directory) = attachments_dir else { return vec![] };
    request
        .attachments
        .iter()
        .filter(|attachment| attachment["type"] == "image")
        .filter_map(|attachment| {
            let reference = crate::attachment_store::AttachmentRef::from_value(attachment)?;
            Some((attachment.clone(), crate::attachment_store::resolve_attachment_path(directory, &reference)?))
        })
        .collect()
}

pub(crate) struct Run<'a> {
    pub instance: &'a OpenCodeInstance,
    pub clock: Arc<dyn SchedulerClock>,
    pub attachments_dir: Option<&'a Path>,
    pub timeout: Duration,
}

impl Run<'_> {
    /// Runs the request on whichever generation the instance's server is.
    pub async fn text(&self, request: &Request) -> Result<Value, TextGenerationError> {
        let v2 = matches!(self.instance.probe(false).await, Ok(probed) if probed.generation == Generation::V2);
        let raw = if v2 { self.version_two(request).await? } else { self.version_one(request).await? };
        serde_json::from_str::<Value>(&crate::text_generation_run::extract_json_object(&raw))
            .ok()
            .and_then(|answer| request.shape.decode(&answer))
            .ok_or_else(|| TextGenerationError::new(request.operation, "OpenCode returned invalid structured output."))
    }

    // ---- 1.x ----

    async fn version_one(&self, request: &Request) -> Result<String, TextGenerationError> {
        use crate::opencode1_api::Client;
        let operation = request.operation;
        let failed = |detail: &str| TextGenerationError::new(operation, detail);
        let (provider, model) = parse_model_slug(request.model_selection["model"].as_str().unwrap_or("")).ok_or_else(|| failed("OpenCode model selection must use the 'provider/model' format."))?;
        let files: Vec<Value> = image_paths(request, self.attachments_dir)
            .into_iter()
            .filter(|(attachment, _)| {
                let mime = attachment["mimeType"].as_str().unwrap_or("").trim().to_lowercase();
                attachment["sizeBytes"].as_u64().unwrap_or(0) <= NATIVE_FILE_PART_MAX_BYTES && matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp")
            })
            .filter_map(|(attachment, path)| Some(json!({"type":"file","mime":attachment["mimeType"],"filename":attachment["name"],"url":url::Url::from_file_path(path).ok()?.to_string()})))
            .collect();
        let connection = self.instance.connect(Path::new(&request.cwd), self.clock.clone()).await.map_err(|error| failed(&error.to_string()))?;
        let client = Client::new(&connection.api, &request.cwd);
        // TODO(original bug): 1.x text generation never removes the temporary session it creates; the 2.x path does
        // (apps/server/src/textGeneration/OpenCodeTextGeneration.ts). Copied.
        let session = client
            .session_create_titled(&format!("T3 Code {}", operation.name()), &json!([{"permission":"*","pattern":"*","action":"deny"}]))
            .await
            .map_err(|_| failed("OpenCode session.create request failed."))?;
        let session_id = session["id"].as_str().filter(|_| session.is_object()).ok_or_else(|| failed("OpenCode session.create returned no session payload."))?.to_owned();
        let mut body = json!({"model":{"providerID":provider,"modelID":model}});
        if let Some(agent) = option_value(&request.model_selection, "agent") {
            body["agent"] = json!(agent);
        }
        if let Some(variant) = option_value(&request.model_selection, "variant") {
            body["variant"] = json!(variant);
        }
        let mut parts = vec![json!({"type":"text","text":request.prompt})];
        parts.extend(files);
        body["parts"] = json!(parts);
        let result = client.prompt(&session_id, &body).await.map_err(|_| failed("OpenCode session.prompt request failed."))?;
        if let Some((_, message)) = prompt_failure(&result["info"]["error"]) {
            return Err(failed(&message));
        }
        let parts = result["parts"].as_array().cloned().unwrap_or_default();
        let text = crate::text_generation::js_trim(&parts.iter().filter(|part| part["type"] == "text").filter_map(|part| part["text"].as_str()).collect::<String>()).to_owned();
        if text.is_empty() {
            return Err(failed("OpenCode returned empty output."));
        }
        Ok(text)
    }

    // ---- 2.x ----

    async fn version_two(&self, request: &Request) -> Result<String, TextGenerationError> {
        let operation = request.operation;
        let failed = |detail: &str| TextGenerationError::new(operation, detail);
        let generic = || failed("OpenCode text generation failed.");
        let connection = self.instance.connect(Path::new(&request.cwd), self.clock.clone()).await.map_err(|_| generic())?;
        let api = &connection.api;
        let (provider, model) = parse_model_slug(request.model_selection["model"].as_str().unwrap_or("")).ok_or_else(|| failed("OpenCode model selection must use the 'provider/model' format."))?;
        let mut model_ref = json!({"providerID":provider,"id":model});
        if let Some(variant) = option_value(&request.model_selection, "variant") {
            model_ref["variant"] = json!(variant);
        }
        // Subscribed first: the stream is volatile, and the reply arrives on it.
        let stream = api.subscribe("/api/event").await.map_err(|_| generic())?;
        let (mut frames, reader) = start_reader(stream, self.clock.clone());
        let created = api
            .call(
                Method::POST,
                "/api/session",
                &[],
                // Nothing in a text generation needs a tool, so every tool asks and T3 is not there to answer; asks are
                // rejected as they arrive. Denying outright gets the whole session refused on OpenCode's free models.
                Some(&json!({"title":format!("T3 Code {}", operation.name()),"location":{"directory":request.cwd},"model":model_ref,"permissions":[{"action":"*","resource":"*","effect":"ask"}]})),
                None,
            )
            .await;
        let session_id = match created.as_ref().map(|created| created["data"]["id"].as_str().map(str::to_owned)) {
            Ok(Some(id)) => id,
            _ => {
                reader.abort();
                return Err(generic());
            }
        };
        let outcome = self.generation(api, &mut frames, &session_id, request).await;
        reader.abort();
        // The temporary session goes whatever happened.
        let _ = api.call(Method::DELETE, &format!("/api/session/{}", crate::opencode_projection::enc(&session_id)), &[], None, Some(REMOVE_TIMEOUT)).await;
        match outcome {
            Err(error) => Err(error),
            Ok(Outcome::Failed(detail)) => Err(failed(&detail)),
            Ok(Outcome::Text(text)) if text.is_empty() => Err(failed("OpenCode returned empty output.")),
            Ok(Outcome::Text(text)) => Ok(text),
        }
    }

    async fn generation(&self, api: &Api, frames: &mut tokio::sync::mpsc::UnboundedReceiver<Frame>, session_id: &str, request: &Request) -> Result<Outcome, TextGenerationError> {
        let operation = request.operation;
        let generic = || TextGenerationError::new(operation, "OpenCode text generation failed.");
        let files: Vec<Value> = image_paths(request, self.attachments_dir).into_iter().map(|(attachment, path)| json!({"uri":format!("file://{}",path.display()),"name":attachment["name"]})).collect();
        let mut body = json!({"text":request.prompt});
        if !files.is_empty() {
            body["files"] = json!(files);
        }
        let segment = crate::opencode_projection::enc(session_id);
        api.call(Method::POST, &format!("/api/session/{segment}/prompt"), &[], Some(&body), None).await.map_err(|_| generic())?;
        let mut texts: IndexMap<String, String> = IndexMap::new();
        let joined = |texts: &IndexMap<String, String>| crate::text_generation::js_trim(&texts.values().cloned().collect::<Vec<_>>().join("\n")).to_owned();
        let wait = async {
            loop {
                let event = match frames.recv().await {
                    Some(Frame::Event(event)) => event,
                    // The reply only arrives on this stream, so a lost stream ends the wait.
                    Some(Frame::Closed) | None => return Outcome::Failed("The OpenCode event stream was lost.".into()),
                };
                let kind = event["type"].as_str().unwrap_or("");
                if event["data"]["sessionID"].as_str() != Some(session_id) {
                    continue;
                }
                let data = &event["data"];
                match kind {
                    "session.text.ended" => {
                        texts.insert(format!("{}:{}", plain(&data["assistantMessageID"]), plain(&data["ordinal"])), data["text"].as_str().unwrap_or("").to_owned());
                    }
                    "permission.asked" => {
                        let path = format!("/api/session/{segment}/permission/{}/reply", crate::opencode_projection::enc(data["id"].as_str().unwrap_or("")));
                        let _ = api.call(Method::POST, &path, &[], Some(&json!({"decision":"reject","message":"Tools are not available while generating text."})), Some(REPLY_TIMEOUT)).await;
                    }
                    "session.execution.succeeded" => return Outcome::Text(joined(&texts)),
                    // The provider's own error (`data.error`) is kept out of the detail, as the original keeps it.
                    "session.execution.failed" => return Outcome::Failed("OpenCode could not generate the text.".into()),
                    "session.execution.interrupted" => return Outcome::Failed("OpenCode stopped the generation.".into()),
                    _ => {}
                }
            }
        };
        match tokio::time::timeout(self.timeout, wait).await {
            Ok(outcome) => Ok(outcome),
            Err(_) => {
                let _ = api.call(Method::POST, &format!("/api/session/{segment}/interrupt"), &[], None, Some(INTERRUPT_TIMEOUT)).await;
                Ok(Outcome::Failed("OpenCode did not finish generating in time.".into()))
            }
        }
    }
}

enum Outcome {
    Text(String),
    Failed(String),
}

/// A JavaScript template literal's reading of an id or an ordinal.
fn plain(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "undefined".into(),
        other => other.to_string(),
    }
}

/// `getOpenCodePromptFailure`: the provider's own name and message for a failed prompt.
fn prompt_failure(error: &Value) -> Option<(Option<String>, String)> {
    if !error.is_object() {
        return None;
    }
    let name = error["name"].as_str().map(crate::text_generation::js_trim).filter(|name| !name.is_empty()).map(str::to_owned);
    let message = error["data"]["message"].as_str().map(crate::text_generation::js_trim).unwrap_or("");
    if !message.is_empty() {
        return Some((name, message.to_owned()));
    }
    name.map(|name| (Some(name.clone()), name))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_model_slug_is_a_provider_and_a_model_split_at_the_first_slash() {
        assert_eq!(parse_model_slug("  openai/gpt-5/mini ").unwrap(), ("openai".to_owned(), "gpt-5/mini".to_owned()));
        for bad in ["gpt-5", "/gpt-5", "openai/", "", "  / "] {
            assert!(parse_model_slug(bad).is_none(), "{bad:?}");
        }
    }
    #[test]
    fn a_failed_prompt_names_its_message_or_else_its_name() {
        let failure = |error: Value| prompt_failure(&error);
        assert_eq!(failure(json!({"name":" Auth ","data":{"message":"  no key  "}})), Some((Some("Auth".into()), "no key".into())));
        assert_eq!(failure(json!({"name":"Aborted","data":{"message":"   "}})), Some((Some("Aborted".into()), "Aborted".into())));
        assert_eq!(failure(json!({"data":{"message":"just words"}})), Some((None, "just words".into())));
        assert_eq!(failure(json!({"name":"  ","data":{}})), None);
        assert_eq!(failure(json!("text")), None);
        assert_eq!(failure(Value::Null), None);
    }
}
