//! Azure DevOps pull requests over a real socket, through the whole stack: the project's remote
//! says Azure DevOps, the service picks the Azure provider, and the provider drives a fake `az`.
//! Every response is checked against the original's schema at the transport. Each call is checked
//! for what it sends, for what it refuses before sending, and for what it makes the server forget.
//! No network, no real `az`, no real credentials. This binary installs one process-wide host before
//! the first request, so everything runs in one test.
#![recursion_limit = "512"]
mod pull_request_support;
mod source_control_support;

use chrono::Utc;
use pull_request_support::*;
use serde_json::{Value, json};
use source_control_support::*;
use t3_server::{project::ProjectService, source_control_host::Host};

fn identity(name: &str, unique: &str) -> Value {
    json!({"displayName":name,"uniqueName":unique,"imageUrl":format!("https://img.example/{name}.png")})
}

/// 7 is an ordinary pull request, 8 a second row for listings, 55 one that is merged.
fn pull_request(number: u64) -> Value {
    let merged = number == 55;
    json!({
        "pullRequestId":number,"title":format!("Fix {number}"),"description":"Body text","status":if merged { "completed" } else { "active" },
        "isDraft":false,"mergeStatus":"succeeded","createdBy":identity("Alice","alice@acme.io"),"reviewers":[identity("Bob","bob@acme.io")],
        "sourceRefName":"refs/heads/feature","targetRefName":"refs/heads/main",
        "creationDate":if number == 8 { "2026-01-03T00:00:00Z" } else { "2026-01-02T00:00:00Z" },
        "closedDate":if merged { json!("2026-01-04T00:00:00Z") } else { Value::Null },
        "url":format!("https://dev.azure.com/acme/_apis/git/repositories/web/pullRequests/{number}"),
        "repository":{"name":"web","webUrl":"https://dev.azure.com/acme/shop/_git/web","project":{"name":"shop"}},
        "_links":{"web":{"href":format!("https://dev.azure.com/acme/shop/_git/web/pullrequest/{number}")}}
    })
}

fn item(content: &str) -> String {
    json!({"content":content,"contentMetadata":{"isBinary":false}}).to_string()
}

fn routes() -> Vec<Value> {
    let comment = |id: u64, content: &str, at: &str| json!({"id":id,"content":content,"author":identity("Bob","bob@acme.io"),"publishedDate":at});
    vec![
        json!({"match":"--version","stdout":"azure-cli 2.60.0\n"}),
        json!({"match":"account show --query user.name","stdout":"carol@acme.io\n"}),
        json!({"match":"account show --query user","stdout":"{\"name\":\"carol@acme.io\",\"type\":\"user\"}"}),
        json!({"match":"--id 99","stdout":pull_request(99).to_string()}),
        json!({"match":"--id 55","stdout":pull_request(55).to_string()}),
        json!({"match":"--id 8","stdout":pull_request(8).to_string()}),
        json!({"match":"--id 7 --only","stdout":pull_request(7).to_string()}),
        json!({"match":"repos pr list","stdout":json!([pull_request(8), pull_request(7)]).to_string()}),
        json!({"match":"--resource pullRequestThreads","stdout":json!({"value":[
            {"id":1,"comments":[comment(1,"Looks fine","2026-01-02T00:00:00Z")]},
            {"id":2,"threadContext":{"filePath":"/src/a.ts"},"comments":[comment(1,"Rename this","2026-01-02T00:01:00Z")]},
            {"id":3,"comments":[{"id":1,"content":"Policy passed","commentType":"system","publishedDate":"2026-01-02T00:02:00Z"}]}
        ]}).to_string()}),
        json!({"match":"--resource pullRequestIterations","stdout":json!({"value":[{"id":1,"sourceRefCommit":{"commitId":"head1"},"commonRefCommit":{"commitId":"base1"}},{"id":2,"sourceRefCommit":{"commitId":"head2"},"commonRefCommit":{"commitId":"base2"}}]}).to_string()}),
        json!({"match":"--resource pullRequestIterationChanges","stdout":json!({"changeEntries":[
            {"changeType":"edit","item":{"path":"/src/a.ts","objectId":"blob-a","originalObjectId":"blob-0","gitObjectType":"blob"}},
            {"changeType":"add","item":{"path":"/src/b.ts","objectId":"blob-b","gitObjectType":"blob"}},
            {"changeType":"edit","item":{"path":"/src","isFolder":true,"gitObjectType":"tree"}}
        ]}).to_string()}),
        json!({"match":"path=/src/a.ts versionDescriptor.versionType=commit versionDescriptor.version=base2","stdout":item("a\nb\nc\n")}),
        json!({"match":"path=/src/a.ts versionDescriptor.versionType=commit versionDescriptor.version=head2","stdout":item("a\nB\nc\n")}),
        json!({"match":"path=/src/b.ts versionDescriptor.versionType=commit versionDescriptor.version=head2","stdout":item("new\n")}),
        json!({"match":"repos pr update","stdout":"{}"}),
        json!({"match":"repos pr reviewer","stdout":"[]"}),
    ]
}

/// What the requests were, as the command and the REST resource they went to.
fn kinds(calls: &[Value]) -> Vec<String> {
    calls
        .iter()
        .map(|call| {
            let args: Vec<&str> = call["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
            match args[0] {
                "repos" if args[2] == "reviewer" => format!("reviewer {}", args[3]),
                "repos" => format!("{} {}", args[1], args[2]),
                "devops" => format!("invoke {}", args[args.iter().position(|arg| *arg == "--resource").map_or(0, |at| at + 1)]),
                other => format!("{other} {}", args[1]),
            }
        })
        .collect()
}

/// The value after a flag.
fn after<'a>(call: &'a Value, flag: &str) -> &'a str {
    let args = call["args"].as_array().unwrap();
    let at = args.iter().position(|arg| arg == flag).unwrap_or_else(|| panic!("{flag} in {args:?}"));
    args[at + 1].as_str().unwrap()
}

#[test]
fn azure_pull_requests_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let cli = FakeCliDir::new();
    let routes = routes();
    install_az(&cli, &routes);
    let test = test_host(&[], Some(cli.path()), None, &base);
    let clock = test.clock.clone();
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");

    let (_web, web_path) = repo_with_remotes(&[("origin", "https://dev.azure.com/acme/shop/_git/web")]);
    let harness = Harness::start(&base, "paz", None).await;
    ProjectService::new(harness.store.clone())
        .mutate(json!({"type":"project.create","commandId":"create-web","projectId":"web","title":"Project web","workspaceRoot":web_path.to_string_lossy()}), Utc::now())
        .unwrap();
    let mut socket = harness.connect(&harness.writer).await;
    let mut reader_socket = harness.connect(&harness.reader).await;
    let reference = |number: u64| json!({"projectId":"web","repository":"web","number":number});
    let fresh = |number: u64| json!({"projectId":"web","repository":"web","number":number,"allowStale":false});
    let with = |mut base: Value, extra: Value| {
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        base
    };
    let mut id = 100;
    macro_rules! call {
        ($socket:expr, $tag:expr, $payload:expr) => {{
            id += 1;
            unary(&mut $socket, id, $tag, $payload).await
        }};
    }

    // -- reads ---------------------------------------------------------------------------------
    let exit = call!(socket, "pullRequests.summary", fresh(7));
    let summary = value_of(&exit);
    assert_eq!(summary["provider"], "azure-devops");
    assert_eq!((summary["number"].clone(), summary["state"].clone(), summary["isDraft"].clone()), (json!(7), json!("open"), json!(false)));
    assert_eq!(summary["author"], json!({"login":"alice@acme.io","name":"Alice","avatarUrl":"https://img.example/Alice.png"}));
    assert_eq!(summary["mergeability"], "mergeable");
    for absent in ["additions", "deletions", "changedFiles", "reviewDecision", "checksState", "stack"] {
        assert!(summary.get(absent).is_none(), "the original's summary carries no {absent}: {summary}");
    }
    assert_eq!(kinds(&take_calls(&cli)), vec!["pr show"], "a summary costs the one read, not the iterations");

    let exit = call!(socket, "pullRequests.detail", fresh(7));
    let detail = value_of(&exit);
    assert_eq!(detail["provider"], "azure-devops");
    assert_eq!(detail["viewer"], "carol@acme.io");
    assert_eq!(detail["changedFiles"], 2, "the file count comes from the latest push, folders left out");
    assert_eq!(detail["mergeCapabilities"], json!({"merge":true,"squash":true,"rebase":false}));
    assert_eq!(detail["checks"], json!([]));
    assert_eq!(detail["reviewers"][0]["login"], "bob@acme.io");
    assert_eq!(detail["labels"], json!([]));
    assert_eq!(detail["autoMergeEnabled"], false);
    assert_eq!(detail["capabilities"]["viewedFiles"], "environment");
    assert_eq!(detail["capabilities"]["mergeMethods"], json!(["merge", "squash"]));
    assert_eq!(detail["capabilities"]["comment"], false);
    assert_eq!(detail["capabilities"]["search"], false);
    assert_eq!(detail["capabilities"]["reactions"], false);
    assert_eq!(detail["capabilities"]["review"], json!({"inlineComment":false,"reply":false,"resolve":false,"verdicts":[]}));
    assert_eq!(detail["capabilities"]["reviewers"], json!({"request":true,"listCandidates":false}));
    assert_eq!(detail["capabilities"]["edit"], json!({"changeRequest":true,"comment":false}));
    for absent in ["labels", "stacks", "stackActions", "updateMethods"] {
        assert!(detail["capabilities"].get(absent).is_none(), "{absent}");
    }
    assert_eq!(detail["viewerPermissions"], json!({"actions":["merge","ready","draft","close","reopen","enable-auto-merge","disable-auto-merge"],"comment":false,"resolve":false,"verdicts":[],"requestReviewers":true}));
    assert!(detail.get("baseComparison").is_none() && detail.get("behindBy").is_none());
    let mut sent = kinds(&take_calls(&cli));
    sent.sort();
    assert_eq!(sent, vec!["account show", "invoke pullRequestIterationChanges", "invoke pullRequestIterations", "pr show"], "the account is read beside the pull request");

    let exit = call!(socket, "pullRequests.checks", fresh(7));
    assert_eq!(value_of(&exit), &Value::Null, "Azure has no checks to read");
    let exit = call!(socket, "pullRequests.preview", reference(7));
    assert_eq!(value_of(&exit)["title"], "Fix 7");
    take_calls(&cli);

    let exit = call!(socket, "pullRequests.activity", fresh(7));
    let activity = value_of(&exit);
    assert_eq!(activity["commentCount"], 2);
    assert_eq!(activity["commentsTruncated"], false);
    assert_eq!(activity["comments"][0]["body"], "Looks fine");
    assert_eq!(activity["comments"][0]["kind"], "issue-comment");
    assert_eq!(activity["comments"][1]["kind"], "review-comment");
    assert_eq!(activity["comments"][1]["path"], "src/a.ts".replace("src", "/src"));
    assert_eq!((activity["reviewThreads"].clone(), activity["commits"].clone()), (json!([]), json!([])));
    assert!(activity.get("reactions").is_none());
    assert_eq!(kinds(&take_calls(&cli)), vec!["pr show", "invoke pullRequestThreads"]);

    // the patch travels over HTTP, where it compresses and is flow-controlled
    let http = reqwest::Client::new();
    let diff = |body: Value| http.post(format!("http://{}/api/pull-requests/diff", harness.address)).bearer_auth(&harness.reader).json(&body).send();
    let response = diff(reference(7)).await.unwrap();
    assert_eq!(response.status(), 200);
    let patch: Value = response.json().await.unwrap();
    let text = patch["patch"].as_str().unwrap();
    assert!(text.starts_with("diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"), "{text}");
    assert!(text.contains("diff --git a/src/b.ts b/src/b.ts\nnew file mode 100644\n--- /dev/null\n+++ b/src/b.ts\n@@ -0,0 +1 @@\n+new\n"), "{text}");
    assert_eq!((patch["truncated"].clone(), patch["nextCursor"].clone()), (json!(false), Value::Null));
    let sent = kinds(&take_calls(&cli));
    assert_eq!(sent.iter().filter(|kind| *kind == "invoke items").count(), 3, "{sent:?}: both sides of the edit, the new side of the add");
    // a cursor from nowhere starts the read again; a named commit is not something Azure can scope to
    let response = diff(with(reference(7), json!({"cursor":"abc","commit":"abc1234"}))).await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.json::<Value>().await.unwrap()["patch"].as_str().unwrap().contains("diff --git a/src/a.ts"));
    // and one that names the second file continues there
    take_calls(&cli);
    let response = diff(with(reference(7), json!({"cursor":"2:1"}))).await.unwrap();
    let continued: Value = response.json().await.unwrap();
    assert!(!continued["patch"].as_str().unwrap().contains("src/a.ts") && continued["patch"].as_str().unwrap().contains("src/b.ts"));
    take_calls(&cli);
    let exit = call!(socket, "pullRequests.diffFileContents", with(reference(7), json!({"changeType":"change","oldPath":"src/a.ts","newPath":"src/a.ts"})));
    assert_eq!(value_of(&exit), &json!({"oldContents":"a\nb\nc\n","newContents":"a\nB\nc\n"}));
    let sent = take_calls(&cli);
    assert_eq!(after(&sent[sent.len() - 1], "--query-parameters"), "path=/src/a.ts", "Azure's own spelling of a path");
    let exit = call!(socket, "pullRequests.threadComments", with(reference(7), json!({"threadId":"1","cursor":"x"})));
    assert_eq!(error_of(&exit)["detail"], "This host does not page review thread comments.");
    let exit = call!(socket, "pullRequests.stack", reference(7));
    assert_eq!(value_of(&exit), &Value::Null, "Azure has no stacks");
    let exit = call!(socket, "pullRequests.routingIdentity", json!({"host":"dev.azure.com"}));
    assert_eq!(error_of(&exit), &json!({"_tag":"PullRequestUnavailableError","reason":"provider-unsupported"}));
    take_calls(&cli);

    // -- the listing: one read per repository, carried on by the number of rows already sent ----------
    let exit = call!(socket, "pullRequests.list", json!({"state":"open"}));
    let page = value_of(&exit).clone();
    assert_eq!(page["viewers"], json!({"dev.azure.com":"carol@acme.io"}));
    assert_eq!(page["entries"].as_array().unwrap().iter().map(|entry| (entry["number"].as_u64().unwrap(), entry["repository"].as_str().unwrap())).collect::<Vec<_>>(), vec![(8, "web"), (7, "web")]);
    assert_eq!(page["entries"][0]["provider"], "azure-devops");
    assert_eq!(page["entries"][0]["viewerReviewRequested"], false);
    assert_eq!(page["entries"][1]["reviewDecision"], Value::Null);
    let providers = page["providers"].as_array().unwrap();
    assert!(providers.iter().any(|provider| provider["host"] == "dev.azure.com" && provider["kind"] == "azure-devops" && provider["configured"] == true && provider["searchesOnHost"] == false), "{providers:?}");
    assert_eq!(page["errors"], json!([]));
    let listing = take_calls(&cli);
    let list = listing.iter().find(|call| call["args"][2] == "list").unwrap();
    let args: Vec<&str> = list["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
    assert_eq!(args[..8], ["repos", "pr", "list", "--detect", "true", "--repository", "web", "--status"]);
    assert_eq!(args[8], "active");
    assert!(args.contains(&"--include-links") && args.contains(&"--top") && !args.contains(&"--skip"), "{args:?}");
    let exit = call!(socket, "pullRequests.list", json!({"state":"open","involvement":"reviewing","limit":1,"query":"fix & more","projectId":"web"}));
    let page = value_of(&exit);
    assert_eq!(page["truncated"], true);
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    let listing = take_calls(&cli);
    let list = listing.iter().find(|call| call["args"][2] == "list").unwrap();
    assert_eq!(after(list, "--reviewer"), "carol@acme.io");
    assert_eq!(after(list, "--top"), "2");
    assert!(!list["args"].to_string().contains("fix"), "Azure has nothing to search with, so the text is not sent");
    let exit = call!(socket, "pullRequests.listStats", json!({"refs":[reference(7)]}));
    assert_eq!(value_of(&exit), &json!({"stats":[]}), "a listing that carries no counts has nothing to be asked for");
    assert!(take_calls(&cli).is_empty());

    // -- viewed files are this environment's own --------------------------------------------------
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(value_of(&exit), &json!({"files":[],"truncated":false}));
    take_calls(&cli);
    let exit = call!(socket, "pullRequests.setFilesViewed", with(reference(7), json!({"files":[{"path":"src/a.ts","viewed":true}]})));
    value_of(&exit);
    let sent = kinds(&take_calls(&cli));
    assert!(sent.contains(&"invoke pullRequestIterationChanges".to_string()), "{sent:?}: the head's blob is stored with the tick");
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(value_of(&exit), &json!({"files":[{"path":"src/a.ts","state":"viewed"}],"truncated":false}));
    let exit = call!(socket, "pullRequests.setFilesViewed", with(reference(7), json!({"files":[{"path":"src/a.ts","viewed":false}]})));
    value_of(&exit);
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(value_of(&exit), &json!({"files":[],"truncated":false}), "unticking deletes the mark");
    take_calls(&cli);

    // -- a read-only connection is refused before anything is decided -----------------------------
    for (tag, extra) in [
        ("pullRequests.comment", json!({"body":"hi"})),
        ("pullRequests.runAction", json!({"action":"close"})),
        ("pullRequests.update", json!({"title":"x"})),
        ("pullRequests.requestReviewers", json!({"reviewers":[{"id":"bob@acme.io","kind":"user"}],"requested":true})),
        ("pullRequests.setReaction", json!({"content":"heart","reacted":true})),
        ("pullRequests.replyToThread", json!({"threadId":"1","body":"x"})),
        ("pullRequests.submitReview", json!({"verdict":"comment","body":"x","comments":[]})),
    ] {
        let exit = call!(reader_socket, tag, with(reference(7), extra));
        assert_eq!(error_of(&exit)["_tag"], "EnvironmentAuthorizationError", "{tag}");
        assert_eq!(error_of(&exit)["requiredPermission"], "source-control:write");
    }
    assert!(take_calls(&cli).is_empty(), "a refused write reaches nothing");

    // -- writes: exactly the request, never a read of the account's standing first -----------------
    let exit = call!(socket, "pullRequests.update", with(reference(7), json!({"title":"New title","body":"- a bullet\nsecond line"})));
    value_of(&exit);
    let sent = take_calls(&cli);
    assert_eq!(kinds(&sent), vec!["pr update"], "Azure states no standing, so the host is left to refuse");
    let args: Vec<&str> = sent[0]["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
    assert_eq!(args[..7], ["repos", "pr", "update", "--detect", "true", "--id", "7"]);
    assert_eq!(args[7..9], ["--title=New title", "--description=- a bullet\nsecond line"], "each value travels as one argument");
    let exit = call!(socket, "pullRequests.update", reference(7));
    assert_eq!(error_of(&exit)["detail"], "Nothing was changed.");
    // the sentences the page shows for what the host cannot do, with no request made
    for (tag, extra, sentence) in [
        ("pullRequests.comment", json!({"body":"hello"}), "This host cannot post a comment on a change request."),
        ("pullRequests.updateComment", json!({"commentId":"1:1","kind":"issue-comment","body":"edited"}), "This host cannot rewrite a comment."),
        ("pullRequests.submitReview", json!({"verdict":"comment","body":"x","comments":[]}), "This host cannot review a change request."),
        ("pullRequests.replyToThread", json!({"threadId":"1","body":"done"}), "This host cannot reply to a review conversation."),
        ("pullRequests.setThreadResolution", json!({"threadId":"1","resolved":true}), "This host cannot resolve a review conversation."),
        ("pullRequests.setReaction", json!({"content":"heart","reacted":true}), "This host has no reactions."),
        ("pullRequests.reviewerCandidates", json!({}), "This host cannot say who may review a change request."),
        ("pullRequests.labelCandidates", json!({}), "This host cannot change the labels on a change request."),
        ("pullRequests.setLabels", json!({"labels":["bug"],"applied":true}), "This host cannot change the labels on a change request."),
    ] {
        let exit = call!(socket, tag, with(reference(7), extra));
        let error = error_of(&exit);
        assert!(error["detail"].as_str().is_some_and(|detail| !detail.is_empty()), "{tag}: {error}");
        assert_eq!(error["detail"], sentence, "{tag}");
    }
    assert!(take_calls(&cli).is_empty(), "what the capabilities hide costs no request");

    // reviewers are asked for by name, and the request is the one command Azure has for it
    let exit = call!(socket, "pullRequests.requestReviewers", with(reference(7), json!({"reviewers":[{"id":"bob@acme.io","kind":"user"},{"id":"8f5a-guid","kind":"user"}],"requested":true})));
    value_of(&exit);
    let sent = take_calls(&cli);
    assert_eq!(kinds(&sent), vec!["reviewer add"]);
    let args: Vec<&str> = sent[0]["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
    assert_eq!(args[args.iter().position(|arg| *arg == "--reviewers").unwrap() + 1..args.len() - 3], ["bob@acme.io", "8f5a-guid"]);
    let exit = call!(socket, "pullRequests.requestReviewers", with(reference(7), json!({"reviewers":[{"id":"bob@acme.io","kind":"user"}],"requested":false})));
    value_of(&exit);
    assert_eq!(kinds(&take_calls(&cli)), vec!["reviewer remove"]);
    let exit = call!(socket, "pullRequests.requestReviewers", with(reference(7), json!({"reviewers":[{"id":"-rf","kind":"user"}],"requested":true})));
    assert_eq!(error_of(&exit)["detail"], "A reviewer is named by an email address or an identity id.");
    assert!(take_calls(&cli).is_empty(), "a name az would read as a flag is refused unsent");

    // -- actions ---------------------------------------------------------------------------------------------
    for (action, extra, expected) in [
        ("close", json!({}), vec!["--status", "abandoned"]),
        ("reopen", json!({}), vec!["--status", "active"]),
        ("ready", json!({}), vec!["--draft", "false"]),
        ("draft", json!({}), vec!["--draft", "true"]),
        ("disable-auto-merge", json!({}), vec!["--auto-complete", "false"]),
        ("enable-auto-merge", json!({"mergeMethod":"squash"}), vec!["--auto-complete", "true", "--squash", "true"]),
        ("enable-auto-merge", json!({"mergeMethod":"merge"}), vec!["--auto-complete", "true", "--squash", "false"]),
        ("merge", json!({"mergeMethod":"squash"}), vec!["--status", "completed", "--squash", "true"]),
        ("merge", json!({"mergeMethod":"merge"}), vec!["--status", "completed", "--squash", "false"]),
    ] {
        let exit = call!(socket, "pullRequests.runAction", with(reference(7), with(json!({"action":action}), extra)));
        value_of(&exit);
        let sent = take_calls(&cli);
        let args: Vec<&str> = sent[0]["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
        assert_eq!(args[7..7 + expected.len()], expected[..], "{action}");
        assert!(kinds(&sent).iter().all(|kind| kind == "pr update" || kind == "pr show" || kind.starts_with("invoke")), "{action}: {:?}", kinds(&sent));
    }
    // refused without a request: what the host does not offer
    for (extra, sentence) in [
        (json!({"action":"revert"}), "This host cannot revert a change request."),
        (json!({"action":"approve-workflows"}), "This host cannot approve-workflows a change request."),
        (json!({"action":"update-branch","updateMethod":"merge"}), "This host cannot update-branch a change request."),
        (json!({"action":"merge","mergeMethod":"rebase"}), "This host cannot merge with the rebase strategy."),
    ] {
        let exit = call!(socket, "pullRequests.runAction", with(reference(7), extra.clone()));
        assert_eq!(error_of(&exit)["detail"], sentence, "{extra}");
    }
    assert!(take_calls(&cli).is_empty(), "a refusal from the host's capabilities costs no request");

    // -- failures of the CLI, told as the original tells them ---------------------------------------------------
    std::fs::write(cli.dir.path().join("mode"), "auth\n").unwrap();
    let exit = call!(socket, "pullRequests.summary", fresh(8));
    assert_eq!(error_of(&exit), &json!({"_tag":"PullRequestUnavailableError","reason":"cli-unauthenticated","provider":"azure-devops"}));
    std::fs::write(cli.dir.path().join("mode"), "notfound\n").unwrap();
    let exit = call!(socket, "pullRequests.summary", fresh(9));
    let error = error_of(&exit).clone();
    assert_eq!((error["_tag"].clone(), error["operation"].clone()), (json!("PullRequestOperationError"), json!("summary")), "{error}");
    take_calls(&cli);
    // a rate limit pauses the host: reads are refused unsent, a press is let through
    std::fs::write(cli.dir.path().join("mode"), "rate\n").unwrap();
    let exit = call!(socket, "pullRequests.summary", fresh(10));
    assert_eq!(error_of(&exit)["detail"], "Azure DevOps API rate limit exceeded.");
    std::fs::write(cli.dir.path().join("mode"), "ok\n").unwrap();
    take_calls(&cli);
    let exit = call!(socket, "pullRequests.summary", fresh(11));
    assert_eq!(error_of(&exit)["detail"], "azure-devops requests to dev.azure.com are paused until the rate limit resets.");
    assert!(take_calls(&cli).is_empty(), "a paused host is not asked");
    let exit = call!(socket, "pullRequests.runAction", with(reference(7), json!({"action":"close"})));
    value_of(&exit);
    assert_eq!(kinds(&take_calls(&cli)), vec!["pr update"], "an interactive call goes through the pause");
    clock.advance(std::time::Duration::from_secs(31));
    value_of(&call!(socket, "pullRequests.summary", fresh(8)));
    take_calls(&cli);
    // an az that is not installed
    std::fs::rename(cli.dir.path().join("az"), cli.dir.path().join("az.away")).unwrap();
    let exit = call!(socket, "pullRequests.summary", fresh(55));
    assert_eq!(error_of(&exit), &json!({"_tag":"PullRequestUnavailableError","reason":"cli-missing","provider":"azure-devops"}));
    std::fs::rename(cli.dir.path().join("az.away"), cli.dir.path().join("az")).unwrap();

    // -- a write makes the server forget what it held ---------------------------------------------------------------
    value_of(&call!(socket, "pullRequests.detail", fresh(7)));
    take_calls(&cli);
    value_of(&call!(socket, "pullRequests.detail", reference(7)));
    assert!(take_calls(&cli).is_empty(), "a detail read is held");
    value_of(&call!(socket, "pullRequests.runAction", with(reference(7), json!({"action":"ready"}))));
    take_calls(&cli);
    value_of(&call!(socket, "pullRequests.detail", fresh(7)));
    assert!(!take_calls(&cli).is_empty(), "the pull request changed, so the detail is read again");

    drop(socket);
    drop(reader_socket);
    harness.finish().await;
}
