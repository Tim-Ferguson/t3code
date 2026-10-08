//! Opaque stdio MCP transport. Credentials are inherited environment values,
//! never command-line arguments or diagnostics.
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{ffi::OsString, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Mutex, watch},
    task::JoinSet,
};

#[derive(Default)]
struct Session {
    id: Option<String>,
    version: Option<String>,
}
#[derive(Clone)]
pub struct HttpBridge {
    endpoint: String,
    authorization: String,
    client: reqwest::Client,
    session: Arc<Mutex<Session>>,
}
impl HttpBridge {
    pub fn new(endpoint: String, authorization: String) -> Self {
        Self {
            endpoint,
            authorization,
            client: reqwest::Client::new(),
            session: Default::default(),
        }
    }
    async fn send<F, Fut>(&self, line: String, mut payload: F) -> Result<(), String>
    where
        F: FnMut(Value) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
    {
        // fetch rejects credentials embedded in URLs rather than synthesizing Basic auth.
        if let Ok(url) = url::Url::parse(&self.endpoint) {
            if !url.username().is_empty() || url.password().is_some() {
                return Err(
                    "Request cannot be constructed from a URL that includes credentials".into(),
                );
            }
        }
        let session = self.session.lock().await;
        let mut request = self
            .client
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", &self.authorization)
            .body(line);
        if let Some(id) = &session.id {
            request = request.header("mcp-session-id", id);
        }
        if let Some(version) = &session.version {
            request = request.header("mcp-protocol-version", version);
        }
        drop(session);
        let response = request
            .send()
            .await
            .map_err(|error| error.without_url().to_string())?;
        if let Some(id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            self.session.lock().await.id = Some(id.to_owned());
        }
        if !response.status().is_success() {
            return Err(format!(
                "T3 Code MCP endpoint responded with HTTP {}.",
                response.status().as_u16()
            ));
        }
        if matches!(response.status().as_u16(), 202 | 204) {
            return Ok(());
        }
        let mut emit = |value: Value| {
            let session = self.session.clone();
            let future = payload(value.clone());
            async move {
                if let Some(version) = value
                    .get("result")
                    .and_then(|value| value.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    session.lock().await.version = Some(version.to_owned());
                }
                future.await
            }
        };
        response_payloads(response, &mut emit).await?;
        Ok(())
    }
    pub async fn call(&self, tool: String, arguments: Value) -> Result<Value, String> {
        let initialize_id = "t3-acp-cli-initialize";
        let initialized = self.collect(json!({"jsonrpc":"2.0","id":initialize_id,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t3-code-acp-cli","version":"0.0.0"}}})).await?;
        if !initialized.iter().any(|entry| {
            entry.get("id").is_some_and(|id| id == initialize_id) && entry.get("error").is_none()
        }) {
            return Err("T3 Code MCP endpoint rejected initialization.".into());
        }
        self.collect(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await?;
        let call_id = "t3-acp-cli-tool-call";
        let responses = self.collect(json!({"jsonrpc":"2.0","id":call_id,"method":"tools/call","params":{"name":tool,"arguments":arguments}})).await?;
        let response = responses
            .iter()
            .find(|entry| entry.get("id").is_some_and(|id| id == call_id));
        match response {
            Some(response) if response.get("error").is_none() => {
                Ok(response.get("result").cloned().unwrap_or(Value::Null))
            }
            Some(response) if response.get("error").is_some() => Err(format!(
                "T3 Code MCP tool call failed: {}",
                crate::device_actions::stringify(&response["error"])
            )),
            _ => Err("T3 Code MCP tool call failed.".into()),
        }
    }
    async fn collect(&self, value: Value) -> Result<Vec<Value>, String> {
        let output = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collected = output.clone();
        self.send(crate::device_actions::stringify(&value), move |value| {
            collected.lock().unwrap().push(value);
            async { Ok(()) }
        })
        .await?;
        Ok(std::mem::take(&mut *output.lock().unwrap()))
    }
}
/// Shared opaque HTTP MCP JSON/SSE decoder. Each transport retains its own
/// protocol-version publication ordering around this decoder.
pub(crate) async fn response_payloads<F, Fut>(
    response: reqwest::Response,
    mut emit: F,
) -> Result<(), String>
where
    F: FnMut(Value) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    if matches!(response.status().as_u16(), 202 | 204) {
        return Ok(());
    }
    if response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .contains("text/event-stream")
    {
        let mut stream = response.bytes_stream();
        let mut parser = SseParser::default();
        while let Some(chunk) = stream.next().await {
            for data in parser.feed(&chunk.map_err(|error| error.without_url().to_string())?) {
                emit(serde_json::from_str(&data).map_err(|error| error.to_string())?).await?;
            }
        }
    } else {
        let bytes = response
            .bytes()
            .await
            .map_err(|error| error.without_url().to_string())?;
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        if !t3_contracts::trim_wire_string(text).is_empty() {
            emit(serde_json::from_str(text).map_err(|error| error.to_string())?).await?;
        }
    }
    Ok(())
}
#[derive(Default)]
struct SseParser {
    decoder: crate::terminal_utf8::TerminalUtf8Decoder,
    buffered: String,
    started: bool,
}
impl SseParser {
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut text = self.decoder.feed(bytes);
        if !self.started && !text.is_empty() {
            self.started = true;
            text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
        }
        self.buffered.push_str(&text);
        let mut result = Vec::new();
        loop {
            let separator = [
                self.buffered.find("\n\n").map(|index| (index, 2)),
                self.buffered.find("\r\n\r\n").map(|index| (index, 4)),
            ]
            .into_iter()
            .flatten()
            .min_by_key(|(index, _)| *index);
            let Some((index, length)) = separator else {
                break;
            };
            let event = self.buffered[..index].to_owned();
            self.buffered.drain(..index + length);
            let data = event
                .split('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line))
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|line| {
                    line.trim_start_matches(|c: char| {
                        let mut bytes = [0; 4];
                        t3_contracts::trim_wire_string(c.encode_utf8(&mut bytes)).is_empty()
                    })
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                result.push(data);
            }
        }
        // The source deliberately discards an incomplete final event.
        result
    }
}
async fn write_message<W: AsyncWrite + Unpin>(
    writer: &Arc<Mutex<W>>,
    value: Value,
) -> Result<(), String> {
    let mut writer = writer.lock().await;
    writer
        .write_all(format!("{}\n", crate::device_actions::stringify(&value)).as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    writer.flush().await.map_err(|error| error.to_string())
}
async fn forward<W: AsyncWrite + Unpin>(
    bridge: HttpBridge,
    writer: Arc<Mutex<W>>,
    line: String,
    envelope: Value,
) {
    let output = writer.clone();
    let session = bridge.session.clone();
    let result = bridge
        .send(line, move |payload| {
            let output = output.clone();
            let session = session.clone();
            async move {
                for entry in match payload {
                    Value::Array(entries) => entries,
                    entry => vec![entry],
                } {
                    if let Some(version) = entry
                        .get("result")
                        .and_then(|value| value.get("protocolVersion"))
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                    {
                        session.lock().await.version = Some(version.to_owned());
                    }
                    write_message(&output, entry).await?;
                }
                Ok(())
            }
        })
        .await;
    if let Err(error) = result {
        if let Some(id) = envelope.get("id") {
            let message = if error.starts_with("T3 Code MCP endpoint responded with HTTP ") {
                error
            } else {
                format!("T3 Code MCP bridge request failed: {error}")
            };
            let _ = write_message(
                &writer,
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":message}}),
            )
            .await;
        }
    }
}
pub async fn run<R, W>(bridge: HttpBridge, mut input: R, output: W) -> Result<(), String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let writer = Arc::new(Mutex::new(output));
    let mut tasks = JoinSet::new();
    let mut barrier: Option<watch::Receiver<bool>> = None;
    let mut bytes = [0u8; 8192];
    let mut decoder = crate::terminal_utf8::TerminalUtf8Decoder::default();
    let mut buffered = String::new();
    let mut skip_lf = false;
    loop {
        let count = tokio::select! {result=input.read(&mut bytes)=>match result {Ok(count)=>count,Err(_)=>break},_ = tasks.join_next(),if !tasks.is_empty()=>continue};
        let text = if count == 0 {
            decoder.finish()
        } else {
            decoder.feed(&bytes[..count])
        };
        let mut lines = Vec::new();
        for character in text.chars() {
            if skip_lf && character == '\n' {
                skip_lf = false;
                continue;
            }
            skip_lf = false;
            if matches!(character, '\n' | '\r') {
                lines.push(std::mem::take(&mut buffered));
                skip_lf = character == '\r';
            } else {
                buffered.push(character);
            }
        }
        if count == 0 && !buffered.is_empty() {
            lines.push(std::mem::take(&mut buffered));
        }
        for line in lines {
            if t3_contracts::trim_wire_string(&line).is_empty() {
                continue;
            }
            let envelope = match serde_json::from_str::<Value>(&line) {
                Ok(value) if value.is_object() || value.is_array() => value,
                Ok(_) => continue,
                Err(_) => {
                    write_message(&writer,json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})).await?;
                    continue;
                }
            };
            let response = envelope.get("id").is_some()
                && (envelope.get("result").is_some() || envelope.get("error").is_some());
            let handshake = matches!(
                envelope.get("method").and_then(Value::as_str),
                Some("initialize" | "notifications/initialized")
            );
            let prior = if response { None } else { barrier.clone() };
            let completion = if handshake {
                let (sender, receiver) = watch::channel(false);
                barrier = Some(receiver);
                Some(sender)
            } else {
                None
            };
            let bridge = bridge.clone();
            let writer = writer.clone();
            tasks.spawn(async move {
                if let Some(mut prior) = prior {
                    let _ = prior.wait_for(|complete| *complete).await;
                }
                forward(bridge, writer, line, envelope).await;
                if let Some(completion) = completion {
                    completion.send_replace(true);
                }
            });
        }
        if count == 0 {
            break;
        }
    }
    while let Some(result) = tasks.join_next().await {
        result.map_err(|error| error.to_string())?;
    }
    Ok(())
}
/// The CLI fast path runs before bootstrap descriptor adoption or app services.
pub fn dispatch(args: impl IntoIterator<Item = OsString>) -> Option<Result<i32, String>> {
    let mut args = args.into_iter();
    let command = args.next()?;
    if command != "acp-mcp-bridge" && command != "acp-mcp-call" {
        return None;
    }
    Some((|| {
        let command = command.to_string_lossy();
        let (Some(endpoint), Some(authorization)) = (
            std::env::var_os("T3_ACP_MCP_ENDPOINT"),
            std::env::var_os("T3_ACP_MCP_AUTHORIZATION"),
        ) else {
            eprintln!("{command} requires T3_ACP_MCP_ENDPOINT and T3_ACP_MCP_AUTHORIZATION.");
            return Ok(2);
        };
        let call = if command == "acp-mcp-call" {
            let (Some(tool), Some(arguments)) = (args.next(), args.next()) else {
                eprintln!("acp-mcp-call requires <tool> and <arguments-json>.");
                return Ok(2);
            };
            let arguments: Value = match serde_json::from_str(&arguments.to_string_lossy()) {
                Ok(value) => value,
                Err(_) => {
                    eprintln!("acp-mcp-call arguments must be valid JSON.");
                    return Ok(2);
                }
            };
            if !arguments.is_object() {
                eprintln!("acp-mcp-call arguments must be a JSON object.");
                return Ok(2);
            }
            Some((tool.to_string_lossy().into_owned(), arguments))
        } else {
            None
        };
        let bridge = HttpBridge::new(
            endpoint.to_string_lossy().into_owned(),
            authorization.to_string_lossy().into_owned(),
        );
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?
            .block_on(async move {
                if let Some((tool, args)) = call {
                    let result = bridge.call(tool, args).await?;
                    println!("{}", crate::device_actions::stringify(&result));
                } else {
                    run(bridge, tokio::io::stdin(), tokio::io::stdout()).await?;
                }
                Ok(0)
            })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sse_decodes_all_utf8_and_delimiter_splits_and_ignores_incomplete_tail() {
        let source = "\u{feff}event: message\r\ndata: {\"result\":\r\ndata: {\"value\":\"👩‍💻中文\"}}\r\n\r\ndata: [1,2]\n\ndata: ignored";
        for split in 0..=source.len() {
            let mut parser = SseParser::default();
            let mut actual = parser.feed(&source.as_bytes()[..split]);
            actual.extend(parser.feed(&source.as_bytes()[split..]));
            assert_eq!(
                actual,
                vec!["{\"result\":\n{\"value\":\"👩‍💻中文\"}}", "[1,2]"],
                "split {split}"
            );
        }
    }
}
