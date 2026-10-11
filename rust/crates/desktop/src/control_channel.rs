//! The app's end of the channel the server it started talks to it over (`DesktopTelemetryPublisher.ts`,
//! `DesktopRemoteUpdates.ts`): one socket, newline-delimited JSON, the app writing `desktopTelemetryHello` and
//! `desktopUpdateStatus` frames and the server writing its requests (`requestDesktopUpdate`, `commitDesktopUpdate`,
//! `cancelDesktopUpdate`, `setDiagnosticsDemand`, `setHostPowerIntervals`).
//!
//! This app has no update feed, so every request for an update is answered as the original answers it with none
//! configured (`updates::unavailable_report`), and a commit is answered that nothing is prepared. The host power
//! and Electron process samples are not published; the server then reports that source as unavailable.
use crate::updates::{self, State};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// What the app answers the server's update requests with.
#[derive(Debug, Clone)]
pub struct Answers {
    pub state: State,
    /// Why updating is not possible (`updates::disabled_reason`).
    pub reason: Option<String>,
}

/// The hello that opens the channel (`DesktopHostTelemetryHello`).
pub fn hello() -> Value {
    json!({"version":1,"type":"desktopTelemetryHello","electronPid":std::process::id()})
}

/// The frame a control message is answered with, if it is answered.
pub fn answer(message: &Value, answers: &Answers) -> Option<Value> {
    let request_id = message["requestId"].as_str().filter(|id| !id.trim().is_empty())?;
    match message["type"].as_str()? {
        "requestDesktopUpdate" => Some(updates::unavailable_report(request_id, &answers.state, answers.reason.as_deref())),
        "commitDesktopUpdate" => Some(updates::not_prepared_report(request_id, &answers.state)),
        // A cancellation of a run that never started, and the sampling controls, need no answer.
        _ => None,
    }
}

/// Serves the channel until the server closes it. `stream` is this app's end.
#[cfg(unix)]
pub async fn serve(stream: tokio::net::UnixStream, answers: Answers) {
    let (reader, mut writer) = stream.into_split();
    let mut frames = vec![hello()];
    let mut lines = BufReader::new(reader).lines();
    loop {
        for frame in frames.drain(..) {
            let mut bytes = frame.to_string().into_bytes();
            bytes.push(b'\n');
            if writer.write_all(&bytes).await.is_err() {
                return;
            }
        }
        let Ok(Some(line)) = lines.next_line().await else { return };
        if let Some(frame) = serde_json::from_str::<Value>(&line).ok().and_then(|message| answer(&message, &answers)) {
            frames.push(frame);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::updates::{Channel, RuntimeInfo};

    fn answers() -> Answers {
        let runtime = RuntimeInfo { host_arch: "arm64".into(), app_arch: "arm64".into(), running_under_arm64_translation: false };
        Answers { state: updates::unavailable("1.0.0", &runtime, Channel::Latest), reason: Some("Automatic updates are not available because no update feed is configured.".into()) }
    }

    #[tokio::test]
    async fn a_request_for_an_update_is_answered_that_updates_are_unavailable_and_a_commit_that_nothing_is_prepared() {
        let (app, server) = tokio::net::UnixStream::pair().unwrap();
        let task = tokio::spawn(serve(app, answers()));
        let (reader, mut writer) = server.into_split();
        let mut lines = BufReader::new(reader).lines();
        let opening: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!((opening["type"].clone(), opening["version"].clone()), (json!("desktopTelemetryHello"), json!(1)));
        assert_eq!(opening["electronPid"], std::process::id());
        for message in [
            json!({"version":1,"type":"setHostPowerIntervals","activeIntervalMs":30000,"idleIntervalMs":120000}),
            json!({"version":1,"type":"requestDesktopUpdate","requestId":"update-1"}),
            json!({"version":1,"type":"cancelDesktopUpdate","requestId":"update-1"}),
            json!({"version":1,"type":"commitDesktopUpdate","requestId":"update-1"}),
        ] {
            writer.write_all(format!("{message}\n").as_bytes()).await.unwrap();
        }
        // Only the two requests that need an answer get one, in order.
        let request: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!((request["type"].clone(), request["requestId"].clone(), request["outcome"].clone()), (json!("desktopUpdateStatus"), json!("update-1"), json!("failed")));
        assert_eq!(request["reason"], "Automatic updates are not available because no update feed is configured.");
        assert_eq!((request["state"]["enabled"].clone(), request["state"]["status"].clone()), (json!(false), json!("disabled")));
        let commit: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!((commit["requestId"].clone(), commit["outcome"].clone(), commit["reason"].clone()), (json!("update-1"), json!("failed"), json!("This desktop update is no longer prepared.")));
        // The server going away ends the task.
        drop(writer);
        drop(lines);
        task.await.unwrap();
    }

    #[test]
    fn a_message_without_an_id_or_of_another_kind_is_not_answered() {
        assert!(answer(&json!({"type":"requestDesktopUpdate"}), &answers()).is_none());
        assert!(answer(&json!({"type":"requestDesktopUpdate","requestId":"  "}), &answers()).is_none());
        assert!(answer(&json!({"type":"setDiagnosticsDemand","enabled":true}), &answers()).is_none());
    }
}
