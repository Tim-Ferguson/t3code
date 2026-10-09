#![cfg(unix)]
//! The disk-backed `assets.createUrl` kinds. Parity: the original issues and resolves URLs over the
//! same tree (`generate_asset_workspace_fixtures.mjs`), and every row must agree on claims, errors
//! and what each request name resolves to. Security: over the real socket and HTTP transport, a
//! traversal, a symlink out of the workspace, an expired or tampered URL and a file replaced
//! after signing must all serve nothing. Raw requests keep the path exactly as written, because an
//! HTTP client would fold `..` before the server ever saw it.
mod rpc_support;
use hmac::{Hmac, Mac};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rpc_support::*;
use serde_json::{Value, json};
use sha2::Sha256;
use std::{os::unix::fs::symlink, path::Path};
use t3_contracts::AssetResource;
use t3_server::asset_access::ResolvedAsset;
use t3_server::{
    asset_workspace::{github_media_file_name, github_media_fetch_url, host_preview_mime_type, percent_decode, favicon_expiry},
    attachment_service::AttachmentService,
    server_secret_store::ServerSecretStore,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PNG: [u8; 29] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52, 0, 0, 0, 3, 0, 0, 0, 5, 8, 2, 0, 0, 0];

fn put(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// The tree `generate_asset_workspace_fixtures.mjs` records the original over.
fn build_tree(base: &Path) {
    let (root, outside, icons) = (base.join("work"), base.join("outside"), base.join("icons"));
    for (path, bytes) in [
        (root.join("index.html"), b"<b>hi</b>".to_vec()),
        (root.join("docs/guide.html"), b"<b>guide</b>".to_vec()),
        (root.join("docs/style.css"), b"body{}".to_vec()),
        (root.join("docs/app.js"), b"1".to_vec()),
        (root.join("docs/.hidden.css"), b"x".to_vec()),
        (root.join("docs/notes.txt"), b"plain".to_vec()),
        (root.join("docs/data.json"), b"{}".to_vec()),
        (root.join("docs/img/logo.png"), PNG.to_vec()),
        (root.join("docs/font.woff2"), b"f".to_vec()),
        (root.join("shot.png"), PNG.to_vec()),
        (root.join("page.pdf"), b"%PDF".to_vec()),
        (root.join("vector.svg"), b"<svg/>".to_vec()),
        (root.join("sub/inner.html"), b"i".to_vec()),
        (root.join("sub/deep/x.css"), b"x".to_vec()),
        (root.join("..double.html"), b"d".to_vec()),
        (outside.join("secret.png"), PNG.to_vec()),
        (outside.join("page.html"), b"<b>out</b>".to_vec()),
        (outside.join("clip.mp4"), b"mp4".to_vec()),
        (outside.join("song.wav"), b"wav".to_vec()),
        (outside.join("notes.txt"), b"no".to_vec()),
        (outside.join("doc.pdf"), b"%PDF".to_vec()),
        (icons.join("public/favicon.svg"), b"<svg/>".to_vec()),
        (icons.join("assets/brand.png"), PNG.to_vec()),
        (icons.join("weird/icon.txt"), b"t".to_vec()),
        (icons.join("t3proj/t3.json"), json!({"iconPath":"brand/mark.png"}).to_string().into_bytes()),
        (icons.join("t3proj/brand/mark.png"), PNG.to_vec()),
        (icons.join("linked/placeholder"), b"p".to_vec()),
        (icons.join("srcfile/index.html"), br#"<link rel="icon" href="/img/logo.svg">"#.to_vec()),
        (icons.join("srcfile/public/img/logo.svg"), b"<svg/>".to_vec()),
        (icons.join("badsrc/index.html"), br#"<link rel="icon" href="../../../etc/passwd">"#.to_vec()),
        (icons.join("external/ext.png"), PNG.to_vec()),
    ] {
        put(&path, bytes);
    }
    symlink(outside.join("secret.png"), root.join("link-out.png")).unwrap();
    symlink(root.join("docs/guide.html"), root.join("link-in.html")).unwrap();
    symlink(&outside, root.join("dir-link")).unwrap();
    symlink(outside.join("page.html"), root.join("docs/escape.html")).unwrap();
    symlink(outside.join("secret.png"), icons.join("linked/favicon.png")).unwrap();
    symlink(&root, base.join("root-link")).unwrap();
}

fn substitute(text: &str, base: &Path) -> String {
    text.replace("{base}", base.to_str().unwrap())
        .replace("{root}", base.join("work").to_str().unwrap())
        .replace("{outside}", base.join("outside").to_str().unwrap())
        .replace("{icons}", base.join("icons").to_str().unwrap())
}
fn hide(text: &str, base: &Path) -> String {
    text.replace(base.join("work").to_str().unwrap(), "{root}")
        .replace(base.join("outside").to_str().unwrap(), "{outside}")
        .replace(base.join("icons").to_str().unwrap(), "{icons}")
        .replace(base.to_str().unwrap(), "{base}")
}
fn sub_value(value: &Value, base: &Path) -> Value {
    serde_json::from_str(&substitute(&value.to_string(), base)).unwrap()
}
fn hide_value(value: &Value, base: &Path) -> Value {
    serde_json::from_str(&hide(&value.to_string(), base)).unwrap()
}

fn token_of(url: &str) -> (String, String) {
    let suffix = url.strip_prefix("/api/assets/").unwrap();
    let (token, name) = suffix.split_once('/').unwrap();
    (token.to_owned(), name.to_owned())
}

#[test]
fn issuing_and_resolving_agree_with_the_original_over_the_same_tree() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/asset-workspace.json")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    build_tree(&base);
    let state = base.join("state");
    std::fs::create_dir_all(state.join("attachments")).unwrap();
    let files = AttachmentService::new(&state);
    let now = chrono::Utc::now().timestamp_millis();
    let mut checked = (0, 0, 0);
    for row in fixture["rows"].as_array().unwrap() {
        let label = row["resource"].to_string();
        let resource: AssetResource = serde_json::from_value(sub_value(&row["resource"], &base)).unwrap();
        let context = t3_server::asset_workspace::IssueContext {
            workspace_root: row["workspaceRoot"].as_str().map(|root| substitute(root, &base)),
            project_favicon_path: row["projectFaviconPath"].as_str().map(|path| substitute(path, &base)),
            project_checkout_pending: row["projectCheckoutPending"] == true,
        };
        let issued = files.issue_asset_url(&resource, &context, None, now);
        if let Some(expected) = row.get("error") {
            let actual = issued.expect_err(&label).wire();
            assert_eq!(hide_value(&actual, &base), *expected, "{label}");
            checked.0 += 1;
            continue;
        }
        let issued = issued.unwrap_or_else(|error| panic!("{label}: {}", error.wire()));
        let (token, name) = token_of(issued["relativeUrl"].as_str().unwrap());
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token.split('.').next().unwrap()).unwrap()).unwrap();
        let mut seen = hide_value(&claims, &base);
        let mut expected = row["issued"]["claims"].clone();
        for claims in [&mut seen, &mut expected] {
            for key in ["expiresAt", "device", "inode"] {
                claims.as_object_mut().unwrap().remove(key);
            }
        }
        assert_eq!(seen, expected, "{label}");
        let favicon = claims["kind"].as_str().unwrap().starts_with("project-favicon");
        if favicon {
            assert_eq!(claims["expiresAt"], favicon_expiry(now), "{label}");
        } else {
            assert!(claims["expiresAt"].as_i64().unwrap() > now + 3_000_000, "{label}");
        }
        assert_eq!(percent_decode(&name).unwrap(), row["issued"]["nameSegment"].as_str().unwrap(), "{label}");
        assert_eq!(issued.get("sourcePath").map(|path| hide(path.as_str().unwrap(), &base)), row["issued"]["sourcePath"].as_str().map(str::to_owned), "{label}");
        assert_eq!(issued.get("imageDimensions").cloned().unwrap_or(Value::Null), row["issued"]["imageDimensions"], "{label}");
        for variant in row["resolved"].as_array().unwrap() {
            let name = variant["variant"].as_str().unwrap();
            // The original is given the already-extracted name; a variant is the part after the token.
            let resolved = files.resolve_asset_request(&token, name, now, None);
            let seen = resolved.map(|asset| match asset {
                ResolvedAsset::File { path, mime_type, file, .. } => json!({"kind":"file","path":hide(path.to_str().unwrap(), &base),"mimeType":mime_type,"hasFile":file.is_some()}),
                ResolvedAsset::GithubMedia { .. } => json!({"kind":"github-media","path":null,"mimeType":null,"hasFile":false}),
                ResolvedAsset::Bytes { .. } => json!({"kind":"bytes","path":null,"mimeType":null,"hasFile":false}),
            });
            assert_eq!(seen.unwrap_or(Value::Null), variant["result"], "{label} / {name:?}");
            checked.2 += 1;
        }
        checked.1 += 1;
    }
    assert!(checked.0 >= 25 && checked.1 >= 50 && checked.2 >= 500, "fixture lost coverage: {checked:?}");
    // GitHub media narrowing, display names and host mime types.
    for row in fixture["githubMedia"].as_array().unwrap() {
        let url = row["url"].as_str().unwrap();
        let fetch = github_media_fetch_url(url);
        assert_eq!(fetch.as_deref(), row["fetchUrl"].as_str(), "{url}");
        if let Some(fetch) = fetch {
            assert_eq!(github_media_file_name(&fetch), row["fileName"].as_str().unwrap(), "{url}");
        }
    }
    for row in fixture["hostPreviewMimes"].as_array().unwrap() {
        assert_eq!(host_preview_mime_type(row["extension"].as_str().unwrap()), row["mime"].as_str(), "{}", row["extension"]);
    }
}

// ---- over the real transport -------------------------------------------------------------------

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}
impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

async fn raw(address: std::net::SocketAddr, method: &str, path: &str, extra: &[(&str, &str)]) -> Reply {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
    for (name, value) in extra {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    let split = bytes.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> = lines.filter_map(|line| line.split_once(": ")).map(|(name, value)| (name.to_owned(), value.to_owned())).collect();
    let mut body = bytes[split + 4..].to_vec();
    // The server may chunk the body; these are small and unchunked unless it says so.
    if headers.iter().any(|(name, value): &(String, String)| name.eq_ignore_ascii_case("transfer-encoding") && value.contains("chunked")) {
        let mut decoded = Vec::new();
        let mut rest = body.as_slice();
        while let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") {
            let size = usize::from_str_radix(std::str::from_utf8(&rest[..line_end]).unwrap().trim(), 16).unwrap();
            if size == 0 {
                break;
            }
            decoded.extend_from_slice(&rest[line_end + 2..line_end + 2 + size]);
            rest = &rest[line_end + 2 + size + 2..];
        }
        body = decoded;
    }
    Reply { status, headers, body }
}

/// A URL the real secret signs, so a test can mint a claim the server would never issue itself.
fn forge(state: &Path, claims: &Value) -> String {
    let secret = ServerSecretStore::open(state.join("secrets")).unwrap().get_or_create_random("asset-access-signing-key", 32).unwrap();
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret).unwrap();
    mac.update(payload.as_bytes());
    format!("{payload}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

async fn create_url(socket: &mut Socket, id: u64, resource: Value) -> Value {
    rpc(socket, id, "assets.createUrl", json!({"resource":resource})).await
}

#[test]
fn urls_serve_only_what_stays_inside_and_stay_what_they_were_signed_for() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(Box::pin(async {
        let h = harness().await;
        build_tree(&h.base);
        let (root, outside, icons) = (h.base.join("work"), h.base.join("outside"), h.base.join("icons"));
        let state = h.base.join("userdata");
        let mut socket = h.connect(&h.operator).await;
        let mut reader = h.connect(&h.reader).await;

        // A project and a thread give `workspace-file` and relative `media-file` their workspace.
        success(&rpc(&mut socket, 1, "projects.mutate", json!({"type":"project.create","commandId":"p","projectId":"project","title":"P","workspaceRoot":root})).await);
        success(&rpc(&mut socket, 2, "projects.mutate", json!({"type":"project.create","commandId":"p2","projectId":"icons","title":"Icons","workspaceRoot":icons})).await);
        t3_server::launch::ThreadLaunchService::new(h.store.clone())
            .launch(json!({"commandId":"launch","threadId":"thread","projectId":"project","title":"T","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), chrono::Utc::now())
            .unwrap();

        // A reader may ask for URLs (the method needs only read access) and fetch them.
        let workspace_file = |path: &str| json!({"_tag":"workspace-file","threadId":"thread","path":path});
        let guide = success(&create_url(&mut reader, 3, workspace_file("docs/guide.html")).await).clone();
        let (token, name) = token_of(guide["relativeUrl"].as_str().unwrap());
        assert_eq!(name, "guide.html");
        let base_path = format!("/api/assets/{token}");
        let page = raw(h.address, "GET", &format!("{base_path}/guide.html"), &[]).await;
        assert_eq!((page.status, page.body.as_slice()), (200, b"<b>guide</b>".as_slice()));
        assert!(page.header("content-security-policy").unwrap().starts_with("sandbox"));
        assert_eq!(page.header("x-content-type-options"), Some("nosniff"));
        // Siblings of the page (styles, scripts, fonts, images) are served under its directory.
        assert_eq!(raw(h.address, "GET", &format!("{base_path}/style.css"), &[]).await.body, b"body{}");
        assert_eq!(raw(h.address, "GET", &format!("{base_path}/img/logo.png"), &[]).await.status, 200);
        // HEAD answers without a body.
        let head = raw(h.address, "HEAD", &format!("{base_path}/style.css"), &[]).await;
        assert_eq!((head.status, head.body.len()), (200, 0));
        // Everything else under the token is nothing: other types, hidden files, climbing, links out.
        for refused in [
            "notes.txt", "data.json", ".hidden.css", "../index.html", "..%2Findex.html", "%2e%2e/index.html", "%2e%2e%2findex.html",
            "%252e%252e/index.html", "img/../../index.html", "escape.html", "missing.css", "style.css%00.png", "%00", "img//..//..//index.html",
            "sub/inner.html", "..", ".", "",
        ] {
            let reply = raw(h.address, "GET", &format!("{base_path}/{refused}"), &[]).await;
            assert_eq!(reply.status, 404, "{refused:?} was served");
        }

        // A symlink swapped in after signing, pointing out of the workspace, serves nothing.
        let css = root.join("docs/style.css");
        std::fs::remove_file(&css).unwrap();
        symlink(outside.join("page.html"), &css).unwrap();
        assert_eq!(raw(h.address, "GET", &format!("{base_path}/style.css"), &[]).await.status, 404);
        std::fs::remove_file(&css).unwrap();
        std::fs::write(&css, "body{}").unwrap();
        assert_eq!(raw(h.address, "GET", &format!("{base_path}/style.css"), &[]).await.status, 200);
        // So does a directory swapped for a link out.
        std::fs::rename(root.join("docs/img"), root.join("docs/img-moved")).unwrap();
        symlink(&outside, root.join("docs/img")).unwrap();
        assert_eq!(raw(h.address, "GET", &format!("{base_path}/img/secret.png"), &[]).await.status, 404);
        std::fs::remove_file(root.join("docs/img")).unwrap();
        std::fs::rename(root.join("docs/img-moved"), root.join("docs/img")).unwrap();

        // The issuing side refuses what the serving side would: traversal, links out, other types.
        for (path, tag) in [
            ("../outside/page.html", "AssetWorkspacePathValidationError"),
            ("link-out.png", "AssetWorkspaceAssetNotFoundError"),
            ("dir-link/page.html", "AssetWorkspaceAssetNotFoundError"),
            ("docs/escape.html", "AssetWorkspaceAssetNotFoundError"),
            ("docs/notes.txt", "AssetPreviewTypeValidationError"),
            ("missing.html", "AssetWorkspaceAssetNotFoundError"),
        ] {
            let exit = create_url(&mut socket, 10, workspace_file(path)).await;
            assert_eq!(failure(&exit)["_tag"], tag, "{path}");
        }
        let unknown = create_url(&mut socket, 11, json!({"_tag":"workspace-file","threadId":"nobody","path":"index.html"})).await;
        assert_eq!(failure(&unknown)["_tag"], "AssetWorkspaceContextResolutionError");

        // An exact image URL names one file; the name in the URL must be that file's.
        let shot = success(&create_url(&mut socket, 12, workspace_file("shot.png")).await).clone();
        assert_eq!(shot["imageDimensions"], json!({"width":3,"height":5}));
        let (shot_token, _) = token_of(shot["relativeUrl"].as_str().unwrap());
        assert_eq!(raw(h.address, "GET", &format!("/api/assets/{shot_token}/shot.png"), &[]).await.body, PNG);
        assert_eq!(raw(h.address, "GET", &format!("/api/assets/{shot_token}/other.png"), &[]).await.status, 404);

        // Expired and tampered URLs.
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(shot_token.split('.').next().unwrap()).unwrap()).unwrap();
        let expired = forge(&state, &{ let mut c = claims.clone(); c["expiresAt"] = json!(chrono::Utc::now().timestamp_millis() - 1); c });
        assert_eq!(raw(h.address, "GET", &format!("/api/assets/{expired}/shot.png"), &[]).await.status, 404);
        let live = forge(&state, &{ let mut c = claims.clone(); c["expiresAt"] = json!(chrono::Utc::now().timestamp_millis() + 60_000); c });
        assert_eq!(raw(h.address, "GET", &format!("/api/assets/{live}/shot.png"), &[]).await.status, 200, "the forging helper must sign what the server accepts");
        let (payload, signature) = shot_token.split_once('.').unwrap();
        let other_payload = URL_SAFE_NO_PAD.encode(json!({"version":1,"kind":"workspace-file-exact","workspaceRoot":outside,"relativePath":"secret.png","expiresAt":claims["expiresAt"]}).to_string());
        let mut flipped = signature.to_owned();
        flipped.replace_range(0..1, if signature.starts_with('A') { "B" } else { "A" });
        for forged in [format!("{other_payload}.{signature}"), format!("{payload}.{flipped}"), format!("{payload}x.{signature}"), payload.to_owned()] {
            let name = if forged.starts_with(&other_payload) { "secret.png" } else { "shot.png" };
            assert_eq!(raw(h.address, "GET", &format!("/api/assets/{forged}/{name}"), &[]).await.status, 404);
        }
        // A correctly signed claim whose root is not a root at all still cannot escape it.
        let climbing = forge(&state, &json!({"version":1,"kind":"workspace-file","workspaceRoot":root,"baseRelativePath":"..","expiresAt":chrono::Utc::now().timestamp_millis() + 60_000}));
        for name in ["outside/page.html", "page.html"] {
            assert_eq!(raw(h.address, "GET", &format!("/api/assets/{climbing}/{name}"), &[]).await.status, 404, "{name}");
        }

        // Media files: served in place from anywhere readable, bound to the file that was signed.
        let media = |path: &str| json!({"_tag":"media-file","threadId":"thread","path":path});
        let clip = success(&create_url(&mut socket, 20, media(outside.join("clip.mp4").to_str().unwrap())).await).clone();
        let (clip_token, clip_name) = token_of(clip["relativeUrl"].as_str().unwrap());
        assert_eq!(clip_name, "clip.mp4");
        let whole = raw(h.address, "GET", &format!("/api/assets/{clip_token}/clip.mp4"), &[]).await;
        assert_eq!((whole.status, whole.header("content-type"), whole.body.as_slice()), (200, Some("video/mp4"), b"mp4".as_slice()));
        assert_eq!(whole.header("cache-control"), Some("private, no-store"));
        let ranged = raw(h.address, "GET", &format!("/api/assets/{clip_token}/clip.mp4"), &[("Range", "bytes=1-2")]).await;
        assert_eq!((ranged.status, ranged.header("content-range"), ranged.body.as_slice()), (206, Some("bytes 1-2/3"), b"p4".as_slice()));
        assert_eq!(raw(h.address, "GET", &format!("/api/assets/{clip_token}/other.mp4"), &[]).await.status, 404);
        // The file replaced by another (a new inode) after signing: nothing is served.
        let replacement = outside.join("clip.mp4.new");
        std::fs::write(&replacement, "evil").unwrap();
        std::fs::rename(&replacement, outside.join("clip.mp4")).unwrap();
        assert_eq!(raw(h.address, "GET", &format!("/api/assets/{clip_token}/clip.mp4"), &[]).await.status, 404);
        // Replaced by a link to somewhere else: nothing either.
        let secret = success(&create_url(&mut socket, 21, media(outside.join("secret.png").to_str().unwrap())).await).clone();
        let (secret_token, _) = token_of(secret["relativeUrl"].as_str().unwrap());
        assert_eq!(raw(h.address, "GET", &format!("/api/assets/{secret_token}/secret.png"), &[]).await.status, 200);
        std::fs::remove_file(outside.join("secret.png")).unwrap();
        symlink(root.join("shot.png"), outside.join("secret.png")).unwrap();
        assert_eq!(raw(h.address, "GET", &format!("/api/assets/{secret_token}/secret.png"), &[]).await.status, 404);
        // A link given at issue time resolves to the file it names, and that is what is signed.
        let linked = success(&create_url(&mut socket, 22, media(root.join("link-in.html").to_str().unwrap())).await).clone();
        assert_eq!(linked["relativeUrl"].as_str().unwrap().rsplit('/').next(), Some("guide.html"));
        // A relative media path uses the thread's workspace; a path that leaves it may still be
        // media (it is host media), but a type the server does not preview never is.
        let relative = success(&create_url(&mut socket, 23, media("docs/img/logo.png")).await).clone();
        assert_eq!(relative["imageDimensions"], json!({"width":3,"height":5}));
        assert_eq!(failure(&create_url(&mut socket, 24, media("docs/notes.txt")).await)["_tag"], "AssetPreviewTypeValidationError");
        assert_eq!(failure(&create_url(&mut socket, 25, media(outside.join("notes.txt").to_str().unwrap())).await)["_tag"], "AssetPreviewTypeValidationError");
        assert_eq!(failure(&create_url(&mut socket, 26, media(outside.join("missing.png").to_str().unwrap())).await)["_tag"], "AssetWorkspaceAssetNotFoundError");

        // A draft names its workspace itself.
        let draft = success(&create_url(&mut socket, 30, json!({"_tag":"draft-workspace-file","cwd":root,"path":"index.html"})).await).clone();
        let (draft_token, _) = token_of(draft["relativeUrl"].as_str().unwrap());
        assert_eq!(raw(h.address, "GET", &format!("/api/assets/{draft_token}/index.html"), &[]).await.body, b"<b>hi</b>");
        assert_eq!(failure(&create_url(&mut socket, 31, json!({"_tag":"draft-workspace-file","cwd":root,"path":"../outside/page.html"})).await)["_tag"], "AssetWorkspacePathValidationError");

        // Project favicons: found by the project's root, with the revision in the name.
        let favicon = success(&create_url(&mut socket, 40, json!({"_tag":"project-favicon","cwd":icons})).await).clone();
        let (favicon_token, favicon_name) = token_of(favicon["relativeUrl"].as_str().unwrap());
        assert!(favicon_name.starts_with('v') && favicon_name.ends_with("-favicon.svg"));
        assert_eq!(favicon["sourcePath"], "public/favicon.svg");
        assert_eq!(raw(h.address, "GET", &format!("/api/assets/{favicon_token}/{favicon_name}"), &[]).await.body, b"<svg/>");
        let unknown_project = create_url(&mut socket, 41, json!({"_tag":"project-favicon","cwd":outside})).await;
        assert_eq!(failure(&unknown_project)["_tag"], "AssetWorkspaceContextNotFoundError");

        // Tool images: bytes the server decodes from the stored output.
        use base64::engine::general_purpose::STANDARD;
        let mut thread = h.store.projection("thread", "thread").unwrap().unwrap();
        thread["turnItems"] = json!([{"id":"item-1","type":"dynamic_tool","output":[{"type":"text","text":"x"},{"type":"image","mimeType":"image/png","data":STANDARD.encode(PNG)},{"type":"image","source":{"type":"base64","media_type":"image/svg+xml","data":STANDARD.encode("<svg/>")}}]},{"id":"item-2","type":"command_execution","output":[{"type":"image","mimeType":"image/png","data":STANDARD.encode(PNG)}]}]);
        h.store.transaction(|transaction| t3_server::persistence::write_projection(transaction, "thread", "thread", &thread)).unwrap();
        let image = success(&create_url(&mut socket, 50, json!({"_tag":"tool-output-image","threadId":"thread","itemId":"item-1","index":0})).await).clone();
        assert_eq!(image["imageDimensions"], json!({"width":3,"height":5}));
        let (image_token, image_name) = token_of(image["relativeUrl"].as_str().unwrap());
        assert_eq!(image_name, "image-1.png");
        let served = raw(h.address, "GET", &format!("/api/assets/{image_token}/{image_name}"), &[]).await;
        assert_eq!((served.status, served.header("content-type"), served.body.as_slice()), (200, Some("image/png"), PNG.as_slice()));
        // Only raster types count, so the SVG is not image 2; and only dynamic tool items have any.
        for (item, index) in [("item-1", 1), ("item-2", 0), ("missing", 0)] {
            let exit = create_url(&mut socket, 51, json!({"_tag":"tool-output-image","threadId":"thread","itemId":item,"index":index})).await;
            assert_eq!(failure(&exit)["_tag"], "AssetWorkspaceAssetNotFoundError", "{item}/{index}");
        }

        // An application icon and GitHub media sign their claims; GitHub media outside GitHub is refused.
        let icon = success(&create_url(&mut socket, 60, json!({"_tag":"native-app-icon","app":{"_tag":"app-id","appId":"com.example.App"}})).await).clone();
        assert!(icon["relativeUrl"].as_str().unwrap().ends_with("/native-app-icon.png"));
        let media_url = success(&create_url(&mut socket, 61, json!({"_tag":"github-media","cwd":root,"url":"https://github.com/user-attachments/assets/abc-1"})).await).clone();
        assert!(media_url["relativeUrl"].as_str().unwrap().ends_with("/abc-1"));
        let foreign = create_url(&mut socket, 62, json!({"_tag":"github-media","cwd":root,"url":"https://example.com/a.png"})).await;
        assert_eq!(failure(&foreign)["_tag"], "AssetGitHubMediaUrlValidationError");
        h.finish().await;
    }));
}
