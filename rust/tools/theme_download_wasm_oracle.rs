//! Development-only target proof of the production browser HTTP adapter.
#[path = "../crates/ui/src/theme_download.rs"]
mod transport;
use serde_json::json;
use transport::Fetcher;
use wasm_bindgen::prelude::*;
#[wasm_bindgen]
pub async fn search() -> String {
    match transport::Source::default()
        .search("demo", "downloadCount")
        .await
    {
        Ok(value) => json!({"value":value,"error":null}),
        Err(error) => json!({"value":null,"error":error}),
    }
    .to_string()
}
#[wasm_bindgen]
pub async fn capped(url: String, limit: usize) -> String {
    let result = async {
        let reply = transport::Http::default()
            .request(transport::Request {
                url,
                head: false,
                limit,
                too_large: "body too large",
            })
            .await?;
        reply.body.await
    }
    .await;
    match result {
        Ok(bytes) => json!({"bytes":bytes.len()}),
        Err(error) => json!({"error":error}),
    }
    .to_string()
}
#[wasm_bindgen]
pub async fn cancel(url: String) -> Result<(), JsValue> {
    let reply = transport::Http::default()
        .request(transport::Request {
            url,
            head: false,
            limit: 20 * 1024 * 1024,
            too_large: "body too large",
        })
        .await
        .map_err(|e| JsValue::from_str(&e))?;
    let mut body = reply.body;
    futures_util::future::poll_fn(|cx| {
        let _ = body.as_mut().poll(cx);
        std::task::Poll::Ready(())
    })
    .await;
    drop(body);
    Ok(())
}
