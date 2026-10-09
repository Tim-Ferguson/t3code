//! `server.getConfig` and `subscribeServerConfig` carry the folder a new named project gets and the
//! Scratch ("No project") folder, as the original's `loadServerConfig` does: Scratch only while
//! the T3 home is not inside a Git work tree.
mod rpc_support;
use rpc_support::*;
use serde_json::json;

#[test]
fn the_config_names_the_new_projects_root_and_offers_scratch_outside_a_work_tree() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(Box::pin(async {
        let h = harness_with_config().await;
        let mut socket = h.connect(&h.operator).await;
        let config = success(&rpc(&mut socket, 1, "server.getConfig", json!({})).await).clone();
        assert_eq!(config["newProjectsRoot"], h.base.join("projects").to_str().unwrap());
        assert_eq!(config["scratchWorkspaceRoot"], h.base.join("scratch").to_str().unwrap());
        // The live config a client subscribes to starts from the same snapshot.
        send(&mut socket, json!({"_tag":"Request","id":2,"tag":"subscribeServerConfig","payload":{},"headers":[]})).await;
        let first = loop {
            let message = next(&mut socket).await;
            if message["_tag"] == "Chunk" {
                break message;
            }
        };
        let snapshot = &first["values"][0];
        assert_eq!(snapshot["type"], "snapshot");
        assert_eq!((snapshot["config"]["newProjectsRoot"].clone(), snapshot["config"]["scratchWorkspaceRoot"].clone()), (config["newProjectsRoot"].clone(), config["scratchWorkspaceRoot"].clone()));
        h.finish().await;

        // Inside a work tree Scratch is withheld, the projects folder is still named.
        let h = harness_with_config_in_checkout().await;
        let mut socket = h.connect(&h.operator).await;
        let config = success(&rpc(&mut socket, 1, "server.getConfig", json!({})).await).clone();
        assert_eq!(config["newProjectsRoot"], h.base.join("projects").to_str().unwrap());
        assert!(config.get("scratchWorkspaceRoot").is_none(), "{config}");
        h.finish().await;
    }));
}
