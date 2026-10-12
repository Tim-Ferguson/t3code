//! A project's icon end to end: the `faviconPath` and `projectIcon` a client sets through `projects.mutate`, the icon file the server
//! finds in the workspace when none is set (carried by a `Project`, never by a shell), what the shell and the events carry, and the
//! refusal of an icon the original's schema does not accept.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use t3_server::{
    project_enrichment::{FaviconResolver, ProjectEnrichment},
    repository_identity::{Probe, RepositoryIdentityResolver},
    test_support::HANG_BOUND,
};
use t3_vcs::ManualClock;

async fn within<T>(wait: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, wait).await.expect("nothing arrived before the hang bound")
}

fn no_git() -> RepositoryIdentityResolver {
    let probe: Probe = Arc::new(|_| Box::pin(async { None }));
    RepositoryIdentityResolver::new(probe, None)
}

/// A resolver that answers from a list and counts how often it is asked.
fn scripted(answers: Vec<Result<Option<String>, String>>) -> (FaviconResolver, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let (counted, answers) = (asked.clone(), std::sync::Mutex::new(answers));
    let resolver: FaviconResolver = Arc::new(move |_| {
        let index = counted.fetch_add(1, Ordering::SeqCst);
        let answers = answers.lock().unwrap();
        answers[index.min(answers.len() - 1)].clone()
    });
    (resolver, asked)
}

#[tokio::test]
async fn an_icon_is_looked_for_once_in_the_background_and_held_for_fifteen_minutes_and_a_failure_for_five_seconds() {
    let clock = Arc::new(ManualClock::default());
    let (resolver, asked) = scripted(vec![Ok(Some("/work/public/favicon.svg".into())), Ok(None), Err("Workspace root is not a directory: /work".into()), Ok(Some("/work/app/icon.png".into()))]);
    let enrichment = ProjectEnrichment::start_with(no_git(), resolver, clock.clone());
    let mut done = enrichment.subscribe_favicons();
    // Cold: nothing is waited for, and the lookup is queued once however often the root is asked for.
    assert_eq!(enrichment.get_available("/work").favicon_path, None);
    assert_eq!(enrichment.get_available("/work").favicon_path, None);
    assert_eq!(within(done.recv()).await.unwrap(), "/work");
    assert_eq!(enrichment.peek("/work").favicon_path.as_deref(), Some("/work/public/favicon.svg"));
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    for _ in 0..100 {
        enrichment.get_available("/work");
    }
    clock.advance(Duration::from_secs(14 * 60));
    assert_eq!(enrichment.get_available("/work").favicon_path.as_deref(), Some("/work/public/favicon.svg"));
    assert_eq!(asked.load(Ordering::SeqCst), 1, "held for fifteen minutes");
    clock.advance(Duration::from_secs(2 * 60));
    // Past that the old answer is gone at once and the root is looked at again.
    assert_eq!(enrichment.get_available("/work").favicon_path, None);
    assert_eq!(within(done.recv()).await.unwrap(), "/work");
    assert_eq!((asked.load(Ordering::SeqCst), enrichment.peek("/work").favicon_path), (2, None), "no icon is an answer too");

    // A root that could not be looked in is held for five seconds, then asked again; invalidating a root forgets it at once.
    enrichment.invalidate(["/work"]);
    enrichment.request("/work");
    assert_eq!(within(done.recv()).await.unwrap(), "/work");
    assert_eq!(enrichment.peek("/work").favicon_path, None, "a failure leaves no icon");
    clock.advance(Duration::from_secs(6));
    enrichment.request("/work");
    assert_eq!(within(done.recv()).await.unwrap(), "/work");
    assert_eq!(enrichment.peek("/work").favicon_path.as_deref(), Some("/work/app/icon.png"));
    enrichment.invalidate(["/work"]);
    assert_eq!(enrichment.peek("/work").favicon_path, None);
}

struct Client {
    http: reqwest::Client,
    address: std::net::SocketAddr,
}

impl Client {
    async fn get(&self, path: &str, token: &str) -> Value {
        self.http.get(format!("http://{}{path}", self.address)).bearer_auth(token).header("x-t3-orchestration-protocol", "2").send().await.unwrap().json().await.unwrap()
    }
    async fn mutate(&self, token: &str, body: Value) -> (u16, Value) {
        let response = self.http.post(format!("http://{}/api/projects/mutate", self.address)).bearer_auth(token).json(&body).send().await.unwrap();
        (response.status().as_u16(), response.json().await.unwrap_or(Value::Null))
    }
}

fn shell_project<'a>(shell: &'a Value, id: &str) -> &'a Value {
    shell["projects"].as_array().unwrap().iter().find(|project| project["id"] == id).unwrap()
}

#[test]
fn a_project_names_its_icon_or_falls_back_on_the_file_in_its_workspace() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(Box::pin(async {
        let h = harness().await;
        let client = Client { http: reqwest::Client::builder().no_proxy().build().unwrap(), address: h.address };
        let workspace = h.base.join("workspace");
        std::fs::create_dir_all(workspace.join("public")).unwrap();
        std::fs::write(workspace.join("public/favicon.svg"), "<svg/>").unwrap();
        std::fs::write(workspace.join("custom.png"), "png").unwrap();
        let picked = h.base.join("picked.webp");
        std::fs::write(&picked, "webp").unwrap();
        let favicon = workspace.join("public/favicon.svg").to_string_lossy().into_owned();
        let mut socket = h.connect(&h.operator).await;
        // The service this server runs, to know when the background look has finished.
        let enrichment = ProjectEnrichment::for_store(&h.store, || None);
        let mut done = enrichment.subscribe_favicons();
        // A project that names no icon: making it is the first look at its root, which queues the search for its icon without waiting
        // for it, so the project made has none yet; once the search is done, the next read carries the file found.
        let created = success(&rpc(&mut socket, 1, "projects.mutate", json!({"type":"project.create","commandId":"create","projectId":"p1","title":"One","workspaceRoot":workspace})).await).clone();
        assert!(created["faviconPath"].is_null(), "not waited for");
        assert_eq!(within(done.recv()).await.unwrap(), workspace.to_string_lossy());
        let warm = client.get("/api/projects", &h.reader).await;
        assert_eq!(warm["projects"][0]["faviconPath"], json!(favicon));
        // The shell carries what the project stored and nothing else: the file found is for a `Project`, not a shell.
        assert!(shell_project(&client.get("/api/orchestration/shell", &h.reader).await, "p1")["faviconPath"].is_null());

        // The user picks a file outside the workspace (`pickProjectFavicon`), then a workspace-relative one.
        let result = success(&rpc(&mut socket, 2, "projects.mutate", json!({"type":"project.update","commandId":"pick","projectId":"p1","faviconPath":picked})).await).clone();
        assert_eq!(result["faviconPath"], json!(picked));
        let shell = client.get("/api/orchestration/shell", &h.reader).await;
        assert_eq!(shell_project(&shell, "p1")["faviconPath"], json!(picked));
        // The asset route serves what the project names: the picked file from where it is, the relative one from the workspace, and the
        // file found in the workspace when it names none. The name of each carries the revision of its bytes.
        let served = |url: Value| {
            let (client, address) = (client.http.clone(), h.address);
            async move {
                let url = url["relativeUrl"].as_str().unwrap().to_owned();
                let bytes = client.get(format!("http://{address}{url}")).send().await.unwrap().bytes().await.unwrap();
                (url, bytes.to_vec())
            }
        };
        let (url, bytes) = served(success(&rpc(&mut socket, 4, "assets.createUrl", json!({"resource":{"_tag":"project-favicon","cwd":workspace}})).await).clone()).await;
        assert_eq!((bytes.as_slice(), url.ends_with("-picked.webp")), (b"webp".as_slice(), true), "{url}");
        let result = success(&rpc(&mut socket, 3, "projects.mutate", json!({"type":"project.update","commandId":"relative","projectId":"p1","faviconPath":"custom.png"})).await).clone();
        assert_eq!(result["faviconPath"], "custom.png");
        let (url, bytes) = served(success(&rpc(&mut socket, 5, "assets.createUrl", json!({"resource":{"_tag":"project-favicon","cwd":workspace}})).await).clone()).await;
        assert_eq!((bytes.as_slice(), url.ends_with("-custom.png")), (b"png".as_slice(), true), "{url}");

        // The icon the user chose, beside it: three kinds, each stored as sent and returned by the shell.
        for (index, icon) in [json!({"kind":"lucide","name":"git-branch","color":"rose"}), json!({"kind":"emoji","emoji":"🚀"}), json!({"kind":"monogram","text":"T3","color":"sky"})].into_iter().enumerate() {
            let result = success(&rpc(&mut socket, 10 + index as u64, "projects.mutate", json!({"type":"project.update","commandId":format!("icon-{index}"),"projectId":"p1","projectIcon":icon})).await).clone();
            assert_eq!(result["projectIcon"], icon);
            let shell = client.get("/api/orchestration/shell", &h.reader).await;
            assert_eq!(shell_project(&shell, "p1")["projectIcon"], icon);
            let typed: t3_contracts::ProjectShell = serde_json::from_value(shell_project(&shell, "p1").clone()).unwrap();
            assert_eq!(serde_json::to_value(typed.project_icon()).unwrap(), icon, "a client reads it back as the same icon");
            assert_eq!(typed.favicon_path(), Some("custom.png"));
        }

        // Neither is changed by an update that names neither, and `null` clears each; clearing the favicon brings the file found back.
        success(&rpc(&mut socket, 20, "projects.mutate", json!({"type":"project.update","commandId":"title","projectId":"p1","title":"Renamed"})).await);
        let shell = client.get("/api/orchestration/shell", &h.reader).await;
        assert_eq!((shell_project(&shell, "p1")["faviconPath"].clone(), shell_project(&shell, "p1")["projectIcon"]["kind"].clone()), (json!("custom.png"), json!("monogram")));
        let cleared = success(&rpc(&mut socket, 21, "projects.mutate", json!({"type":"project.update","commandId":"clear","projectId":"p1","faviconPath":null,"projectIcon":null})).await).clone();
        assert!(cleared["projectIcon"].is_null());
        let shell = client.get("/api/orchestration/shell", &h.reader).await;
        assert!(shell_project(&shell, "p1")["faviconPath"].is_null() && shell_project(&shell, "p1")["projectIcon"].is_null());
        let back = client.get("/api/projects", &h.reader).await;
        assert_eq!(back["projects"][0]["faviconPath"], json!(favicon), "the file found in the workspace answers again");

        // What the original's schema refuses is refused, over the socket and over HTTP, and nothing is stored.
        for (index, icon) in [
            json!({"kind":"lucide","name":"Not A Name","color":"gray"}),
            json!({"kind":"lucide","name":"folder","color":"mauve"}),
            json!({"kind":"emoji","emoji":""}),
            json!({"kind":"monogram","text":"","color":"gray"}),
            json!({"kind":"sticker"}),
            json!("folder"),
        ]
        .into_iter()
        .enumerate()
        {
            let payload = json!({"type":"project.update","commandId":format!("bad-{index}"),"projectId":"p1","projectIcon":icon});
            let refused = rpc(&mut socket, 30 + index as u64, "projects.mutate", payload.clone()).await;
            assert!(!defect(&refused).is_null(), "{icon}: refused as a payload that does not decode");
            assert_eq!(client.mutate(&h.operator, payload).await.0, 400, "{icon}");
        }
        // A monogram of more than two characters passes the schema and is refused by the command, as in the original.
        let long = rpc(&mut socket, 40, "projects.mutate", json!({"type":"project.update","commandId":"long","projectId":"p1","projectIcon":{"kind":"monogram","text":"ABC","color":"gray"}})).await;
        assert_eq!(failure(&long)["_tag"], "ProjectMutationError");
        assert!(client.get("/api/orchestration/shell", &h.reader).await["projects"][0]["projectIcon"].is_null());
        // The HTTP route decodes as the socket does: a legacy spelling is not an input, an unknown key is dropped.
        let (status, project) = client.mutate(&h.operator, json!({"type":"project.update","commandId":"http-icon","projectId":"p1","projectIcon":{"kind":"emoji","emoji":"📦","extra":1},"unknown":true})).await;
        assert_eq!((status, &project["projectIcon"]), (200, &json!({"kind":"emoji","emoji":"📦"})));
        h.finish().await;
    }));
}
