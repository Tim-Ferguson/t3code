//! A viewer's queue and what a viewer reads (apps/server/src/preview/ServerBrowser.test.ts, `ServerBrowserStream.test.ts`).
use super::*;
use crate::test_support::HANG_BOUND;

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, future).await.expect("the awaited event happened")
}

#[tokio::test]
async fn the_queue_keeps_order_refuses_past_its_bound_and_ends_when_closed() {
    let queue = Arc::new(ViewerQueue::default());
    for width in 0..VIEWER_OUTPUT_LIMIT as u32 {
        assert!(queue.offer(ViewerOutput::Viewport { width, height: 1 }).is_ok());
    }
    // A full queue hands the output back, so the caller can release what it holds.
    let refused = queue.offer(ViewerOutput::Gone).unwrap_err();
    assert!(matches!(refused, ViewerOutput::Gone));
    assert!(matches!(within(queue.next()).await, Some(ViewerOutput::Viewport { width: 0, .. })));
    assert_eq!(queue.len(), VIEWER_OUTPUT_LIMIT - 1);
    let cleared = queue.clear();
    assert_eq!((cleared.len(), queue.is_empty()), (VIEWER_OUTPUT_LIMIT - 1, true));
    // A reader gets what was queued before the close, and then the end.
    let reader = queue.clone();
    let waiting = tokio::spawn(async move {
        let first = reader.next().await;
        let second = reader.next().await;
        (first.and_then(|output| output.to_json()), second.is_none())
    });
    let _ = queue.offer(ViewerOutput::Viewport { width: 7, height: 7 });
    queue.close();
    let (first, closed) = within(waiting).await.unwrap();
    assert_eq!(first, Some(json!({"type": "viewport", "width": 7, "height": 7})));
    assert!(closed);
    // A closed queue takes nothing more.
    assert!(queue.offer(ViewerOutput::Gone).is_err());
}

#[test]
fn everything_but_a_frame_is_json_a_viewer_reads() {
    let shown = |output: ViewerOutput| output.to_json();
    assert_eq!(shown(ViewerOutput::Viewport { width: 1280, height: 800 }), Some(json!({"type": "viewport", "width": 1280, "height": 800})));
    assert_eq!(
        shown(ViewerOutput::Control { can_operate: true, controller: "you", generation: 3, dialog: Some(json!({"type": "alert", "message": "m", "defaultValue": ""})) }),
        Some(json!({"type": "control", "canOperate": true, "controller": "you", "generation": 3, "dialog": {"type": "alert", "message": "m", "defaultValue": ""}}))
    );
    assert_eq!(shown(ViewerOutput::Probe { x: 1.5, y: 2.0, editable: true }), Some(json!({"type": "probe", "x": 1.5, "y": 2.0, "editable": true})));
    assert_eq!(shown(ViewerOutput::Clipboard { text: "t".into() }), Some(json!({"type": "clipboard", "text": "t"})));
    assert_eq!(shown(ViewerOutput::Pointer { phase: "click", x: 1.0, y: 2.0, sequence: 4 }), Some(json!({"type": "pointer", "phase": "click", "x": 1.0, "y": 2.0, "sequence": 4})));
    assert_eq!(shown(ViewerOutput::FileChooser { id: "c".into(), multiple: true, accept: "image/*".into() }), Some(json!({"type": "fileChooser", "id": "c", "multiple": true, "accept": "image/*"})));
    assert_eq!(shown(ViewerOutput::FileChooserClosed { id: "c".into() }), Some(json!({"type": "fileChooserClosed", "id": "c"})));
    assert_eq!(shown(ViewerOutput::Download { id: "d".into(), file_name: "a.csv".into(), size_bytes: 9 }), Some(json!({"type": "download", "id": "d", "fileName": "a.csv", "sizeBytes": 9})));
    assert_eq!((shown(ViewerOutput::Frame { data: vec![], ack: Ack::none() }), shown(ViewerOutput::Gone), shown(ViewerOutput::Reconnect)), (None, None, None));
}

#[tokio::test]
async fn an_ack_runs_when_released_and_never_before() {
    let (done, finished) = tokio::sync::oneshot::channel();
    let ack = Ack::of(Box::pin(async move {
        let _ = done.send(());
    }));
    ack.release();
    within(finished).await.unwrap();
    // Nothing to release is nothing to run.
    Ack::none().release();
    within(Ack::none().into_future()).await;
}
