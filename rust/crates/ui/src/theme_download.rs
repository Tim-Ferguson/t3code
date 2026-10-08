//! Public Open VSX HTTP transport: bounded streaming, scope cancellation and source deadlines.
#[cfg(not(target_arch = "wasm32"))]
use futures_util::StreamExt;
use std::{future::Future, pin::Pin, rc::Rc};
use t3_client::themes::{
    Catalog, Definition,
    openvsx::{self, Extension},
};
#[derive(Clone, Debug)]
pub struct Request {
    pub url: String,
    pub head: bool,
    pub limit: usize,
    pub too_large: &'static str,
}
pub struct Reply {
    pub ok: bool,
    pub length: Option<String>,
    pub body: Pin<Box<dyn Future<Output = Result<Vec<u8>, String>>>>,
}
struct Data {
    ok: bool,
    bytes: Vec<u8>,
}
pub type FetchFuture = Pin<Box<dyn Future<Output = Result<Reply, String>>>>;
pub trait Fetcher {
    fn request(&self, request: Request) -> FetchFuture;
}
#[derive(Clone)]
pub struct Source(pub Rc<dyn Fetcher>);
impl Default for Source {
    fn default() -> Self {
        Self(Rc::new(Http::default()))
    }
}
impl Source {
    async fn headers(
        &self,
        url: &str,
        head: bool,
        limit: usize,
        too_large: &'static str,
    ) -> Result<Reply, String> {
        self.0
            .request(Request {
                url: url.into(),
                head,
                limit,
                too_large,
            })
            .await
    }
    async fn get(
        &self,
        url: &str,
        head: bool,
        limit: usize,
        too_large: &'static str,
    ) -> Result<Data, String> {
        let response = self.headers(url, head, limit, too_large).await?;
        let ok = response.ok;
        let bytes = response.body.await?;
        Ok(Data { ok, bytes })
    }
    pub async fn search(&self, query: &str, sort: &str) -> Result<Vec<Extension>, String> {
        let Some(url) = openvsx::query_url(query, sort) else {
            return Ok(vec![]);
        };
        let value = deadline(async {
            let response = self
                .get(
                    &url,
                    false,
                    openvsx::MAX_SEARCH_BYTES,
                    "Open VSX returned an unexpectedly large response.",
                )
                .await?;
            if !response.ok {
                return Err("Open VSX search is unavailable right now.".into());
            }
            openvsx::json_value(openvsx::decode_text(&response.bytes).as_bytes())
                .map_err(|_| "Open VSX returned an unreadable response.".into())
        })
        .await?;
        let identities = openvsx::identities(&value)?;
        let details =
            futures_util::future::join_all(identities.iter().take(16).map(|(namespace, name)| {
                deadline(async {
                    let response = self
                        .get(
                            &openvsx::detail_url(namespace, name),
                            false,
                            openvsx::MAX_TEXT_BYTES,
                            "Open VSX returned an unexpectedly large detail response.",
                        )
                        .await?;
                    if !response.ok {
                        return Err("Open VSX theme details are unavailable.".into());
                    }
                    let validated = async {
                        let value =
                            openvsx::json_value(openvsx::decode_text(&response.bytes).as_bytes())
                                .map_err(|_| "invalid JSON")?;
                        let Some(extension) = openvsx::detail(&value)? else {
                            return Ok(None);
                        };
                        let (manifest, package) = futures_util::try_join!(
                            self.headers(
                                &extension.manifest_url,
                                false,
                                openvsx::MAX_TEXT_BYTES,
                                "Open VSX returned an unexpectedly large manifest."
                            ),
                            self.headers(&extension.vsix_url, true, 0, "")
                        )?;
                        if !manifest.ok {
                            return Err("manifest unavailable".into());
                        }
                        if !package.ok {
                            return Ok(None);
                        }
                        if package
                            .length
                            .as_deref()
                            .and_then(openvsx::header_number)
                            .is_some_and(|v| v.is_finite() && v > openvsx::MAX_PACKAGE_BYTES as f64)
                        {
                            return Ok(None);
                        }
                        let manifest =
                            openvsx::manifest(&openvsx::decode_text(&manifest.body.await?))?;
                        Ok((!openvsx::contributions(&manifest).is_empty()
                            && openvsx::license_matches(&manifest, &extension.license))
                        .then_some(extension))
                    }
                    .await;
                    validated
                        .map_err(|_: String| "Open VSX returned unreadable theme details.".into())
                })
            }))
            .await;
        if !identities.is_empty() && details.iter().all(Result::is_err) {
            return Err("Open VSX theme details are unavailable right now.".into());
        }
        Ok(details
            .into_iter()
            .filter_map(Result::ok)
            .flatten()
            .take(8)
            .collect())
    }
    pub async fn package(
        &self,
        catalog: &Catalog,
        extension: &Extension,
    ) -> Result<Vec<Definition>, String> {
        let manifest = self
            .get(
                &extension.manifest_url,
                false,
                openvsx::MAX_TEXT_BYTES,
                "That Open VSX extension manifest is too large.",
            )
            .await?;
        if !manifest.ok {
            return Err("That Open VSX extension has no readable manifest.".into());
        }
        let manifest = openvsx::decode_text(&manifest.bytes);
        openvsx::validate_manifest(&manifest)?;
        let package = self
            .get(
                &extension.vsix_url,
                false,
                openvsx::MAX_PACKAGE_BYTES,
                "That theme extension is too large to import safely.",
            )
            .await?;
        if !package.ok {
            return Err("That Open VSX theme could not be downloaded.".into());
        }
        let checksum = self
            .get(
                &extension.sha256_url,
                false,
                256,
                "That Open VSX checksum response is invalid.",
            )
            .await?;
        if !checksum.ok {
            return Err("That Open VSX theme has no readable checksum.".into());
        }
        openvsx::import_package(
            catalog,
            extension,
            &manifest,
            &package.bytes,
            &openvsx::decode_text(&checksum.bytes),
        )
    }
}
async fn deadline<T>(future: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    #[cfg(target_arch = "wasm32")]
    let timer = gloo_timers::future::TimeoutFuture::new(10000);
    #[cfg(not(target_arch = "wasm32"))]
    let timer = tokio::time::sleep(std::time::Duration::from_secs(10));
    futures_util::pin_mut!(timer);
    futures_util::pin_mut!(future);
    match futures_util::future::select(future, timer).await {
        futures_util::future::Either::Left((result, _)) => result,
        futures_util::future::Either::Right(_) => Err("Open VSX took too long to respond.".into()),
    }
}
#[derive(Default)]
pub(crate) struct Http {
    #[cfg(not(target_arch = "wasm32"))]
    client: reqwest::Client,
}
impl Fetcher for Http {
    fn request(&self, request: Request) -> FetchFuture {
        #[cfg(not(target_arch = "wasm32"))]
        let client = self.client.clone();
        Box::pin(async move {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let response = client
                    .request(
                        if request.head {
                            reqwest::Method::HEAD
                        } else {
                            reqwest::Method::GET
                        },
                        &request.url,
                    )
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                let ok = response.status().is_success();
                let length = response
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let response_length = length.clone();
                let body = Box::pin(async move {
                    if !ok || request.head {
                        return Ok(Vec::new());
                    }
                    if length
                        .as_deref()
                        .and_then(openvsx::header_number)
                        .is_some_and(|v| v > request.limit as f64)
                    {
                        return Err(request.too_large.into());
                    }
                    let mut bytes = Vec::new();
                    let stream = response.bytes_stream();
                    futures_util::pin_mut!(stream);
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk.map_err(|error| error.to_string())?;
                        if bytes
                            .len()
                            .checked_add(chunk.len())
                            .is_none_or(|v| v > request.limit)
                        {
                            return Err(request.too_large.into());
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(bytes)
                });
                Ok(Reply {
                    ok,
                    length: response_length,
                    body,
                })
            }
            #[cfg(target_arch = "wasm32")]
            {
                browser::request(request).await
            }
        })
    }
}
#[cfg(target_arch = "wasm32")]
mod browser {
    use super::*;
    use js_sys::{Function, Promise, Reflect, Uint8Array};
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    struct Abort(web_sys::AbortController);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    struct Reader(web_sys::ReadableStreamDefaultReader);
    impl Drop for Reader {
        fn drop(&mut self) {
            let cancellation = self.0.cancel();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = JsFuture::from(cancellation).await;
            });
            self.0.release_lock();
        }
    }
    fn error(value: JsValue) -> String {
        Reflect::get(&value, &JsValue::from_str("message"))
            .ok()
            .and_then(|v| v.as_string())
            .or_else(|| value.as_string())
            .unwrap_or_else(|| "Open VSX request failed.".into())
    }
    pub async fn request(request: Request) -> Result<Reply, String> {
        let abort = Abort(web_sys::AbortController::new().map_err(error)?);
        let init = web_sys::RequestInit::new();
        init.set_method(if request.head { "HEAD" } else { "GET" });
        init.set_signal(Some(&abort.0.signal()));
        let global = js_sys::global();
        let fetch: Function = Reflect::get(&global, &JsValue::from_str("fetch"))
            .map_err(error)?
            .dyn_into()
            .map_err(error)?;
        let promise: Promise = fetch
            .call2(&global, &JsValue::from_str(&request.url), init.as_ref())
            .map_err(error)?
            .dyn_into()
            .map_err(error)?;
        let response: web_sys::Response = JsFuture::from(promise)
            .await
            .map_err(error)?
            .dyn_into()
            .map_err(error)?;
        let ok = response.ok();
        let length = response.headers().get("content-length").map_err(error)?;
        let body_length = length.clone();
        let body = Box::pin(async move {
            // The abort controller remains owned by the body even before it is
            // polled; dropping a rejected search detail cancels that fetch.
            let _abort = abort;
            if !ok || request.head {
                return Ok(Vec::new());
            }
            if body_length
                .as_deref()
                .and_then(openvsx::header_number)
                .is_some_and(|v| v > request.limit as f64)
            {
                return Err(request.too_large.into());
            }
            let mut bytes = Vec::new();
            if let Some(body) = response.body() {
                let reader =
                    Reader(web_sys::ReadableStreamDefaultReader::new(&body).map_err(error)?);
                loop {
                    let row = JsFuture::from(reader.0.read()).await.map_err(error)?;
                    if Reflect::get(&row, &JsValue::from_str("done"))
                        .map_err(error)?
                        .as_bool()
                        == Some(true)
                    {
                        break;
                    }
                    let chunk: Uint8Array = Reflect::get(&row, &JsValue::from_str("value"))
                        .map_err(error)?
                        .dyn_into()
                        .map_err(error)?;
                    let length = chunk.length() as usize;
                    if bytes
                        .len()
                        .checked_add(length)
                        .is_none_or(|v| v > request.limit)
                    {
                        return Err(request.too_large.into());
                    }
                    let start = bytes.len();
                    bytes.resize(start + length, 0);
                    chunk.copy_to(&mut bytes[start..]);
                }
            } else {
                let buffer = JsFuture::from(response.array_buffer().map_err(error)?)
                    .await
                    .map_err(error)?;
                let buffer = Uint8Array::new(&buffer);
                if buffer.length() as usize > request.limit {
                    return Err(request.too_large.into());
                }
                bytes = buffer.to_vec();
            }
            Ok(bytes)
        });
        Ok(Reply { ok, length, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::cell::Cell;
    struct Fixture {
        input: Value,
        reads: Rc<Cell<usize>>,
    }
    impl Fetcher for Fixture {
        fn request(&self, request: Request) -> FetchFuture {
            let input = self.input.clone();
            let reads = self.reads.clone();
            Box::pin(async move {
                let root = "https://open-vsx.org/api/demo/theme/1.0.0/file";
                let mut ok = true;
                let mut length = None;
                let body: Pin<Box<dyn Future<Output = Result<Vec<u8>, String>>>> = if request
                    .url
                    .contains("/-/search")
                {
                    Box::pin(async {
                        Ok(br#"{"extensions":[{"namespace":"demo","name":"theme"}]}"#.to_vec())
                    })
                } else if request.url.ends_with("/api/demo/theme") {
                    let detail = json!({"namespace":"demo","name":"theme","displayName":"Demo Theme","version":"1.0.0","license":"MIT","description":"A nice theme","downloadCount":123456,"repository":"https://github.com/demo/theme","files":{"icon":format!("{root}/icon.png"),"manifest":format!("{root}/package.json"),"sha256":format!("{root}/theme.sha256"),"download":format!("{root}/theme.vsix")}});
                    Box::pin(async move { Ok(serde_json::to_vec(&detail).unwrap()) })
                } else if request.head {
                    // The original fixture constructs WHATWG Headers; non-byte
                    // values reject the HEAD fetch before any manifest read.
                    if input["packageLength"]
                        .as_str()
                        .is_some_and(|v| v.chars().any(|c| c as u32 > 255))
                    {
                        return Err("Invalid header value".into());
                    }
                    ok = input["packageOk"].as_bool().unwrap();
                    length = input["packageLength"].as_str().map(str::to_owned);
                    Box::pin(async { Ok(vec![]) })
                } else {
                    ok = input["manifestOk"].as_bool().unwrap();
                    if input["body"] == "oversized" {
                        length = Some("262145".into());
                    }
                    Box::pin(async move {
                        if !ok {
                            return Ok(vec![]);
                        }
                        if input["body"] == "oversized" {
                            return Err(request.too_large.into());
                        }
                        reads.set(reads.get() + 1);
                        if input["body"] == "failure" {
                            return Err("body failed".into());
                        }
                        Ok(br#"{"license":"MIT","contributes":{"themes":[{"path":"./theme.json"}]}}"#.to_vec())
                    })
                };
                Ok(Reply { ok, length, body })
            })
        }
    }
    #[tokio::test]
    async fn original_search_header_and_body_error_precedence() {
        for (index, line) in include_str!("../tests/fixtures/openvsx-network.jsonl")
            .lines()
            .enumerate()
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let reads = Rc::new(Cell::new(0));
            let source = Source(Rc::new(Fixture {
                input: row["input"].clone(),
                reads: reads.clone(),
            }));
            let result = source.search("demo", "downloadCount").await;
            let actual = match result {
                Ok(value) => json!({"value":value,"error":null,"bodyReads":reads.get()}),
                Err(error) => json!({"value":null,"error":error,"bodyReads":reads.get()}),
            };
            // JavaScript's single Number type does not distinguish the count's
            // integer JSON spelling from the Rust download_count f64 field.
            let mut expected = row["expected"].clone();
            if let Some(items) = expected["value"].as_array_mut() {
                for item in items {
                    item["downloadCount"] = json!(item["downloadCount"].as_f64().unwrap());
                }
            }
            assert_eq!(actual, expected, "original HTTP sequence {index}");
        }
    }
    #[tokio::test]
    async fn native_http_caps_chunks_and_dropping_pending_body_keeps_other_requests_responsive() {
        use axum::{
            Router,
            body::{Body, Bytes},
            routing::get,
        };
        use std::sync::{Arc, Mutex};
        struct Notify(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Notify {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let notifier = Arc::new(Mutex::new(Some(tx)));
        let app = Router::new()
            .route(
                "/cap",
                get(|| async {
                    Body::from_stream(futures_util::stream::iter([
                        Ok::<_, std::io::Error>(Bytes::from_static(b"abc")),
                        Ok(Bytes::from_static(b"def")),
                    ]))
                }),
            )
            .route(
                "/pending",
                get(move || {
                    let notifier = notifier.clone();
                    async move {
                        let guard = Notify(notifier.lock().unwrap().take());
                        let tail = futures_util::stream::once(async move {
                            let _guard = guard;
                            std::future::pending::<Result<Bytes, std::io::Error>>().await
                        });
                        Body::from_stream(
                            futures_util::stream::iter([Ok(Bytes::from_static(b"a"))]).chain(tail),
                        )
                    }
                }),
            )
            .route("/ready", get(|| async { "ready" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = Http::default();
        let request = |path: &str| Request {
            url: format!("http://{address}/{path}"),
            head: false,
            limit: 4,
            too_large: "body too large",
        };
        let cap = http.request(request("cap")).await.unwrap().body.await;
        assert_eq!(cap, Err("body too large".into()));
        let mut pending = http.request(request("pending")).await.unwrap().body;
        futures_util::future::poll_fn(|cx| {
            let _ = pending.as_mut().poll(cx);
            std::task::Poll::Ready(())
        })
        .await;
        drop(pending);
        let mut ready = request("ready");
        ready.limit = 5;
        assert_eq!(
            http.request(ready).await.unwrap().body.await.unwrap(),
            b"ready"
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), rx)
            .await
            .expect("server observed canceled body")
            .unwrap();
        server.abort();
        let _ = server.await;
    }
}
