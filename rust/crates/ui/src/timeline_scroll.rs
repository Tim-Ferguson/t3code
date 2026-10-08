//! The DOM bridge only measures rows and performs Rust-owned scroll writes.
//! Gesture policy, run observation, restoration and ownership stay in Rust.
use crate::{
    runtime::{UiModel, UiModelStoreExt},
    scroll_state::{ScrollMode, ScrollRequest, ScrollState, ScrollTarget},
};
use dioxus::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use t3_client::timeline_scroll::{
    CHAT_TIMELINE_ANCHOR_OFFSET, EndState, Measurements, PositionCache, RememberedPosition,
    RowAnchor, RunInput, RunObservation, resolve_at_end,
};

#[derive(Clone, Default, Debug)]
pub(crate) struct Positions(Rc<RefCell<PositionCache>>);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Frame {
    kind: String,
    intent_generation: u64,
    #[serde(default)]
    row_ids: Vec<String>,
    #[serde(default)]
    positions: Vec<Option<f64>>,
    #[serde(default)]
    sizes: Vec<Option<f64>>,
    #[serde(default)]
    messages: Vec<Option<String>>,
    #[serde(default)]
    has_geometry: bool,
    scroll: f64,
    viewport_height: f64,
    content_length: f64,
    #[serde(default)]
    delta_y: f64,
    #[serde(default)]
    ctrl_key: bool,
    #[serde(default)]
    timeline_target: bool,
    #[serde(default)]
    scrollbar: bool,
    #[serde(default)]
    direction: i8,
}
impl Frame {
    fn measurements(&self) -> Measurements {
        Measurements {
            row_ids: self.row_ids.clone(),
            positions: self.positions.clone(),
            sizes: self.sizes.clone(),
            scroll: self.scroll,
            viewport_height: self.viewport_height,
        }
    }
    fn at_end(&self) -> bool {
        resolve_at_end(Some(&EndState {
            content_length: Some(self.content_length),
            scroll: Some(self.scroll),
            scroll_length: Some(self.viewport_height),
            is_at_end: None,
        }))
        .unwrap_or(false)
    }
}
struct Session {
    policy: ScrollState,
    owner: String,
    positions: Positions,
    initial: Option<RememberedPosition>,
    initialized: bool,
    initial_cancelled: bool,
    hydrated: bool,
    pending: Option<ScrollRequest>,
    last_frame: Option<Measurements>,
    last_messages: Vec<Option<String>>,
    last_anchor: Option<RowAnchor>,
    eval: Option<dioxus::document::Eval>,
    last_send: Option<String>,
    pill_generation: u64,
    pill_scheduled: bool,
    show_pill: bool,
}
impl Session {
    fn new(owner: String, positions: Positions) -> Self {
        let initial = positions.0.borrow().read(&owner).cloned();
        Self {
            policy: ScrollState::new(owner.clone(), initial.as_ref()),
            owner,
            positions,
            initial,
            initialized: false,
            initial_cancelled: false,
            hydrated: false,
            pending: None,
            last_frame: None,
            last_messages: vec![],
            last_anchor: None,
            eval: None,
            last_send: None,
            pill_generation: 0,
            pill_scheduled: false,
            show_pill: false,
        }
    }
    fn request_measurement(&self) {
        if let Some(eval) = self.eval {
            let _ = eval.send(json!({"type":"measure"}));
        }
    }
    fn observe(&mut self, input: &RunInput, send: Option<String>, tool_activity: bool) {
        self.hydrated = input.observation.hydrated;
        if let Some(send) = send.filter(|send| self.last_send.as_ref() != Some(send)) {
            self.last_send = Some(send.clone());
            self.pending = self.policy.own_send(send, false);
        }
        if let Some(request) = self.policy.observe(input) {
            self.pending = Some(request);
        }
        if tool_activity
            && matches!(&self.policy.mode, ScrollMode::AnchoringNewTurn(message) if input.message_id.as_ref()==Some(message))
        {
            self.pending = Some(self.policy.follow_end());
        }
        self.request_measurement();
    }
    fn finished(&mut self, message: &str) {
        if matches!(&self.policy.mode,ScrollMode::AnchoringNewTurn(anchor) if anchor==message) {
            self.policy.anchor_settled(message);
            self.pending = Some(self.policy.follow_end());
            self.request_measurement();
        }
    }
    fn frame(&mut self, frame: &Frame) -> Option<Value> {
        let measurements = if frame.has_geometry {
            self.last_messages = frame.messages.clone();
            frame.measurements()
        } else {
            let mut measurements = self.last_frame.clone().unwrap_or_default();
            measurements.scroll = frame.scroll;
            measurements.viewport_height = frame.viewport_height;
            measurements
        };
        let at_end = frame.at_end();
        let overflow = measurements.content_overflows(0.0, CHAT_TIMELINE_ANCHOR_OFFSET);
        let restore_cancelled = !self.initialized
            && match frame.kind.as_str() {
                "wheel" => !frame.ctrl_key && frame.timeline_target,
                "touch" | "pointer" => true,
                "key" => frame.timeline_target,
                _ => false,
            };
        if restore_cancelled {
            self.policy.manual_navigation();
        }
        let manual = restore_cancelled
            || match frame.kind.as_str() {
                "wheel" => self.policy.wheel(
                    frame.delta_y,
                    frame.ctrl_key,
                    frame.timeline_target,
                    overflow,
                ),
                "touch" => self.policy.touch_move(!at_end),
                "pointer" => self.policy.pointer_down(frame.scrollbar, overflow, !at_end),
                "key"
                    if frame.timeline_target
                        && ((frame.direction < 0 && overflow)
                            || (frame.direction > 0 && !at_end)) =>
                {
                    self.policy.manual_navigation();
                    true
                }
                _ => false,
            };
        if manual {
            self.initial_cancelled = true;
            self.pending = None;
        }
        // Before hydration, loading rows must not overwrite a saved reading
        // position or exhaust the one initial restore.
        if !self.hydrated {
            self.last_frame = Some(measurements);
            return None;
        }
        if !self.initialized {
            if !self.initial_cancelled && self.pending.is_none() {
                self.pending = self.policy.initial_position(self.initial.as_ref(), false);
            }
            self.initialized = true;
        }
        if frame.kind == "scroll" {
            self.policy.at_end_changed(at_end);
        }
        let prepended = self.last_frame.as_ref().is_some_and(|old| {
            old.row_ids.first() != measurements.row_ids.first()
                && old
                    .row_ids
                    .first()
                    .is_some_and(|id| measurements.row_ids.contains(id))
        });
        if prepended && self.policy.mode == ScrollMode::FreeScrolling && self.pending.is_none() {
            if let Some(anchor) = self.last_anchor.clone() {
                self.pending = Some(self.policy.preserve_history(anchor));
            }
        }
        let request = self
            .pending
            .take()
            .filter(|request| self.policy.owns(request));
        let target = request
            .as_ref()
            .map(|request| request.target.clone())
            .or_else(|| match &self.policy.mode {
                ScrollMode::FollowingEnd if matches!(frame.kind.as_str(), "layout" | "measure") => {
                    Some(ScrollTarget::End)
                }
                ScrollMode::AnchoringNewTurn(id)
                    if matches!(frame.kind.as_str(), "layout" | "measure") =>
                {
                    Some(ScrollTarget::AuthoredMessage(id.clone()))
                }
                _ => None,
            });
        let mut spacer = 0.0;
        let offset = match target {
            Some(ScrollTarget::End) => {
                Some((frame.content_length - frame.viewport_height).max(0.0))
            }
            Some(ScrollTarget::Row(anchor)) => Some(measurements.restore_offset(
                &anchor,
                (frame.content_length - frame.viewport_height).max(0.0),
            )),
            Some(ScrollTarget::AuthoredMessage(message)) => {
                if let Some(index) = self
                    .last_messages
                    .iter()
                    .position(|id| id.as_ref() == Some(&message))
                {
                    measurements
                        .anchored_turn(index as f64, 0.0, CHAT_TIMELINE_ANCHOR_OFFSET)
                        .map(|metrics| {
                            spacer =
                                (metrics.usable_viewport_height - metrics.turn_height).max(0.0);
                            if metrics.overflows_usable_viewport {
                                metrics.target_scroll_to_reveal_end
                            } else {
                                (metrics.anchor_top - CHAT_TIMELINE_ANCHOR_OFFSET).max(0.0)
                            }
                        })
                } else {
                    self.pending = request;
                    None
                }
            }
            None => None,
        };
        if let Some(anchor) = measurements.reading_anchor() {
            // Imperative restores are recorded after the resulting scroll,
            // never as the transient top position before initial hydration.
            if offset.is_none() || frame.kind == "scroll" {
                self.positions.0.borrow_mut().remember(
                    self.owner.clone(),
                    RememberedPosition {
                        anchor: anchor.clone(),
                        at_end: self.policy.mode != ScrollMode::FreeScrolling,
                        disclosures: None,
                    },
                );
            }
            self.last_anchor = Some(anchor);
        }
        self.last_frame = Some(measurements);
        if manual {
            return Some(json!({"type":"cancel","intentGeneration":frame.intent_generation}));
        }
        if offset.is_none()
            && matches!(frame.kind.as_str(), "wheel" | "touch" | "pointer" | "key")
            && self.policy.mode != ScrollMode::FreeScrolling
        {
            // The adapter canceled its pending write before Rust classified
            // the gesture. A harmless gesture must remeasure and retain the
            // latest follow target, including a final streamed chunk.
            return Some(json!({"type":"measure"}));
        }
        offset.filter(|offset|offset.is_finite()).map(|offset|json!({"type":"scroll","offset":offset,"spacer":spacer,"intentGeneration":frame.intent_generation}))
    }
}

#[derive(Clone)]
pub(crate) struct Handle {
    session: Rc<RefCell<Session>>,
    pub id: String,
    pub show_latest: Signal<bool>,
    pub error: Signal<Option<String>>,
}
impl Handle {
    pub fn mount(&self) {
        let handle = self.clone();
        if handle.session.borrow().eval.is_some() {
            return;
        }
        let script = format!(
            "const timelineId={};\n{}",
            serde_json::to_string(&self.id).unwrap(),
            include_str!("timeline_dom.js")
        );
        let mut eval = dioxus::document::eval(&script);
        self.session.borrow_mut().eval = Some(eval);
        spawn(async move {
            // recv alone never reports a script initialization rejection. Race
            // its completion as well, so a broken adapter cannot fail silently.
            let completion = eval.join::<Value>();
            futures_util::pin_mut!(completion);
            loop {
                let value = {
                    let next = eval.recv::<Value>();
                    futures_util::pin_mut!(next);
                    match futures_util::future::select(next, completion.as_mut()).await {
                        futures_util::future::Either::Left((Ok(value), _)) => value,
                        futures_util::future::Either::Left((Err(error), _)) => {
                            handle.failed(format!("Scroll bridge receive failed: {error:?}"));
                            break;
                        }
                        futures_util::future::Either::Right((result, _)) => {
                            handle
                                .failed(format!("Scroll bridge initialization ended: {result:?}"));
                            break;
                        }
                    }
                };
                if value["kind"] == "error" {
                    handle.failed(
                        value["message"]
                            .as_str()
                            .unwrap_or("Scroll bridge could not initialize")
                            .to_owned(),
                    );
                    break;
                }
                let frame = match serde_json::from_value::<Frame>(value) {
                    Ok(frame) => frame,
                    Err(error) => {
                        handle.failed(format!("Scroll bridge measurement was invalid: {error}"));
                        break;
                    }
                };
                let (command, show) = {
                    let mut session = handle.session.borrow_mut();
                    let command = session.frame(&frame);
                    (
                        command,
                        session.initialized
                            && session.policy.mode == ScrollMode::FreeScrolling
                            && !frame.at_end(),
                    )
                };
                let ticket = {
                    let mut session = handle.session.borrow_mut();
                    session.show_pill = show;
                    if !show {
                        session.pill_generation += 1;
                        session.pill_scheduled = false;
                        None
                    } else if !*handle.show_latest.peek() && !session.pill_scheduled {
                        session.pill_scheduled = true;
                        Some(session.pill_generation)
                    } else {
                        None
                    }
                };
                if !show && *handle.show_latest.peek() {
                    let mut visible = handle.show_latest;
                    visible.set(false);
                }
                if let Some(ticket) = ticket {
                    let delayed = handle.clone();
                    spawn(async move {
                        // Source scroll-to-end pill shows after150ms, hides
                        // immediately, so settling layout never flashes it.
                        #[cfg(target_arch = "wasm32")]
                        gloo_timers::future::TimeoutFuture::new(150).await;
                        #[cfg(not(target_arch = "wasm32"))]
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        let mut session = delayed.session.borrow_mut();
                        if session.pill_generation == ticket && session.show_pill {
                            session.pill_scheduled = false;
                            let mut visible = delayed.show_latest;
                            visible.set(true);
                        }
                    });
                }
                if let Some(command) = command {
                    let _ = eval.send(command);
                }
            }
        });
    }
    fn failed(&self, message: String) {
        if let Some(eval) = self.session.borrow_mut().eval.take() {
            let _ = eval.send(json!({"type":"stop"}));
        }
        let mut error = self.error;
        error.set(Some(message));
    }
    pub fn latest(&self) {
        let mut visible = self.show_latest;
        visible.set(false);
        let mut session = self.session.borrow_mut();
        session.pill_generation += 1;
        session.pill_scheduled = false;
        session.show_pill = false;
        session.pending = Some(session.policy.follow_end());
        session.request_measurement();
    }
}

pub(crate) fn use_scroll(state: Store<UiModel>, owner: String) -> Handle {
    let positions = use_hook(|| try_consume_context::<Positions>().unwrap_or_default());
    let session = use_hook(|| {
        let mut session = Session::new(owner.clone(), positions);
        // Reopening establishes a send baseline; retained notifications must
        // never frame a previously authored message again.
        session.last_send = state
            .timeline_send()
            .peek()
            .as_ref()
            .filter(|(key, _)| key == &owner)
            .map(|(_, message)| message.clone());
        Rc::new(RefCell::new(session))
    });
    let id = use_hook(|| format!("timeline-{}", uuid::Uuid::new_v4()));
    let show_latest = use_signal(|| {
        session
            .borrow()
            .initial
            .as_ref()
            .is_some_and(|position| !position.at_end)
    });
    let error = use_signal(|| None);
    let handle = Handle {
        session: session.clone(),
        id,
        show_latest,
        error,
    };
    let observed = session.clone();
    use_effect(move || {
        let domain = state.thread();
        let domain = domain.read();
        let projection = domain.projection.as_ref();
        let run = projection
            .and_then(|projection| projection["runs"].as_array())
            .and_then(|runs| {
                runs.iter()
                    .filter(|run| {
                        matches!(
                            run["status"].as_str(),
                            Some("preparing" | "starting" | "running" | "waiting")
                        )
                    })
                    .max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0))
                    .or_else(|| {
                        runs.iter()
                            .filter(|run| run["queueHeld"] != true)
                            .max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0))
                    })
            });
        let run_id = run.and_then(|run| run["id"].as_str());
        let rows = projection.and_then(|projection| projection["visibleTurnItems"].as_array());
        let message = rows
            .and_then(|rows| {
                rows.iter().find(|row| {
                    row["item"]["type"] == "user_message" && row["item"]["runId"].as_str() == run_id
                })
            })
            .and_then(|row| row["item"]["messageId"].as_str());
        // activeRunningTurnId deliberately excludes queued/waiting/settled
        // runs, matching deriveThreadRuntime, even if a newer run is queued.
        let running = projection
            .and_then(|p| p["runs"].as_array())
            .and_then(|runs| {
                runs.iter()
                    .filter(|run| {
                        matches!(
                            run["status"].as_str(),
                            Some("preparing" | "starting" | "running")
                        )
                    })
                    .max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0))
            })
            .and_then(|run| run["id"].as_str());
        // session-logic projectedWorkEntry carries itemType for every work
        // row. Standalone events, progress and proposed plans are not work.
        let qualifying = rows.into_iter().flatten().find(|row| {
            let item = &row["item"];
            item["runId"].as_str() == running
                && running.is_some()
                && !matches!(
                    item["type"].as_str(),
                    Some(
                        "user_message"
                            | "assistant_message"
                            | "todo_list"
                            | "checkpoint"
                            | "proposed_plan"
                            | "fork"
                            | "handoff"
                            | "run_interrupt_request"
                            | "run_interrupt_result"
                            | "secret_request"
                            | "subagent"
                    )
                )
                && !(item["type"] == "command_execution" && item["input"] == "Preparing workspace")
                && !(item["type"] == "error"
                    && item["status"] == "cancelled"
                    && item["failure"]["code"] == "workspace_preparation_failed")
        });
        // Only the qualifying witness crosses the predicate boundary. Avoid
        // rebuilding another JSON timeline on every streaming text update.
        let entries:Vec<_>=qualifying.into_iter().map(|row|json!({"kind":"work","entry":{"runId":row["item"]["runId"],"itemType":row["item"]["type"]}})).collect();
        let tool = t3_client::timeline_scroll::release_anchor_for_tool_activity(
            &json!({"anchorMessageId":message,"liveFollowEnabled":true,"runningTurnId":running,"timelineEntries":entries}),
        );
        let sent = state
            .timeline_send()
            .read()
            .as_ref()
            .filter(|(key, _)| key == &owner)
            .map(|(_, message)| message.clone());
        let mut session = observed.borrow_mut();
        session.observe(
            &RunInput {
                observation: RunObservation {
                    thread_key: Some(owner.clone()),
                    hydrated: projection.is_some() && domain.synchronized,
                    run_id: run_id.map(str::to_owned),
                },
                queued: run.is_some_and(|run| run["status"] == "queued"),
                message_id: message.map(str::to_owned),
            },
            sent,
            tool,
        );
        // Source ChatView anchorRunSettled releases phantom filler once its
        // own run settles, even when the provider emitted only text.
        let settled = if let ScrollMode::AnchoringNewTurn(message) = &session.policy.mode {
            projection
                .and_then(|p| p["runs"].as_array())
                .and_then(|runs| {
                    runs.iter()
                        .find(|run| run["userMessageId"].as_str() == Some(message.as_str()))
                })
                .filter(|run| {
                    !matches!(
                        run["status"].as_str(),
                        Some("preparing" | "queued" | "starting" | "running" | "waiting")
                    )
                })
                .map(|_| message.clone())
        } else {
            None
        };
        if let Some(message) = settled {
            session.finished(&message);
        }
    });
    use_drop(move || {
        if let Some(eval) = session.borrow_mut().eval.take() {
            let _ = eval.send(json!({"type":"stop"}));
        }
    });
    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(
        kind: &str,
        scroll: f64,
        ids: &[&str],
        positions: &[f64],
        messages: &[Option<&str>],
    ) -> Frame {
        Frame {
            kind: kind.into(),
            intent_generation: 0,
            row_ids: ids.iter().map(|id| id.to_string()).collect(),
            positions: positions.iter().copied().map(Some).collect(),
            sizes: vec![Some(100.0); ids.len()],
            has_geometry: true,
            messages: messages.iter().map(|id| id.map(str::to_owned)).collect(),
            scroll,
            viewport_height: 400.0,
            content_length: positions.last().copied().unwrap_or(0.0) + 100.0,
            delta_y: 0.0,
            ctrl_key: false,
            timeline_target: true,
            scrollbar: false,
            direction: 0,
        }
    }
    fn input(run: &str, message: &str) -> RunInput {
        RunInput {
            observation: RunObservation {
                thread_key: Some("a:thread".into()),
                hydrated: true,
                run_id: Some(run.into()),
            },
            queued: false,
            message_id: Some(message.into()),
        }
    }
    #[test]
    fn reading_gesture_before_hydration_cancels_the_actual_session_initial_restore() {
        let cache = Positions::default();
        cache.0.borrow_mut().remember(
            "a:thread".into(),
            RememberedPosition {
                anchor: RowAnchor {
                    row_id: "saved".into(),
                    offset_within_row: 32.0,
                    scroll_offset: 832.0,
                },
                at_end: false,
                disclosures: None,
            },
        );
        let mut session = Session::new("a:thread".into(), cache);
        let mut wheel = frame("wheel", 0.0, &["loading"], &[0.0], &[None]);
        wheel.delta_y = -30.0;
        assert!(session.frame(&wheel).is_none());
        session.observe(&input("old", "old-message"), None, false);
        let hydrated = frame(
            "layout",
            0.0,
            &["first", "saved", "end"],
            &[0.0, 800.0, 1500.0],
            &[None; 3],
        );
        assert!(
            session.frame(&hydrated).is_none(),
            "a delayed snapshot must not restore over user navigation"
        );
        assert_eq!(session.policy.mode, ScrollMode::FreeScrolling);
    }
    #[test]
    fn own_send_waits_for_its_row_even_while_old_projected_tools_remain() {
        let mut session = Session::new("a:thread".into(), Positions::default());
        session.observe(&input("old", "old-message"), None, false);
        let initial = frame("layout", 0.0, &["old"], &[0.0], &[Some("old-message")]);
        session.frame(&initial);
        session.observe(
            &input("old", "old-message"),
            Some("new-message".into()),
            true,
        );
        assert_eq!(
            session.policy.mode,
            ScrollMode::AnchoringNewTurn("new-message".into())
        );
        assert!(
            session.frame(&initial).is_none(),
            "do not anchor or follow an old tool row while the new send is in flight"
        );
        session.observe(
            &input("new", "new-message"),
            Some("new-message".into()),
            false,
        );
        let fresh = frame(
            "layout",
            0.0,
            &["old", "new"],
            &[0.0, 600.0],
            &[Some("old-message"), Some("new-message")],
        );
        let command = session.frame(&fresh).unwrap();
        assert_eq!(command["offset"], 576.0);
        assert_eq!(command["spacer"], 276.0);
        session.observe(
            &input("new", "new-message"),
            Some("new-message".into()),
            true,
        );
        assert_eq!(
            session.policy.mode,
            ScrollMode::FollowingEnd,
            "only work belonging to the new running turn releases framing"
        );
    }
    #[test]
    fn session_holds_reading_through_streaming_and_preserves_row_when_history_is_prepended() {
        let mut session = Session::new("a:thread".into(), Positions::default());
        session.observe(&input("old", "old-message"), None, false);
        let mut original = frame(
            "layout",
            0.0,
            &["first", "reading", "end"],
            &[0.0, 500.0, 1500.0],
            &[None; 3],
        );
        assert_eq!(session.frame(&original).unwrap()["offset"], 1200.0);
        original.kind = "wheel".into();
        original.delta_y = -30.0;
        original.scroll = 620.0;
        session.frame(&original);
        original.kind = "scroll".into();
        session.frame(&original);
        session.observe(&input("remote", "remote-message"), None, false);
        original.kind = "layout".into();
        original.content_length = 2200.0;
        assert!(
            session.frame(&original).is_none(),
            "remote output does not pull a reader to the end"
        );
        let mut prepended = frame(
            "layout",
            620.0,
            &["history", "first", "reading", "end"],
            &[0.0, 900.0, 1400.0, 2400.0],
            &[None; 4],
        );
        prepended.content_length = 3000.0;
        assert_eq!(session.frame(&prepended).unwrap()["offset"], 1520.0);
    }
    #[test]
    fn final_geometry_dirty_frame_follows_after_a_same_frame_scroll_notification() {
        for kind in ["layout", "measure"] {
            let mut session = Session::new("a:thread".into(), Positions::default());
            session.observe(&input("old", "old-message"), None, false);
            let mut geometry = frame("layout", 0.0, &["first", "end"], &[0.0, 600.0], &[None; 2]);
            session.frame(&geometry);
            geometry.kind = "scroll".into();
            geometry.scroll = 300.0;
            session.frame(&geometry);
            geometry.kind = kind.into();
            geometry.content_length = 1000.0;
            geometry.positions[1] = Some(900.0);
            assert_eq!(session.frame(&geometry).unwrap()["offset"], 600.0);
        }
    }
    #[test]
    fn harmless_gesture_remeasures_and_cached_scroll_frames_keep_reading_geometry() {
        let mut session = Session::new("a:thread".into(), Positions::default());
        session.observe(&input("old", "old-message"), None, false);
        let mut initial = frame(
            "layout",
            0.0,
            &["first", "middle", "end"],
            &[0.0, 500.0, 1000.0],
            &[None; 3],
        );
        session.frame(&initial);
        initial.kind = "wheel".into();
        initial.delta_y = 30.0;
        assert_eq!(
            session.frame(&initial).unwrap()["type"],
            "measure",
            "a downward wheel cannot strand a canceled pending follow write"
        );
        initial.delta_y = -30.0;
        initial.scroll = 620.0;
        session.frame(&initial);
        let mut cached = frame("scroll", 650.0, &[], &[], &[]);
        cached.has_geometry = false;
        cached.content_length = 1100.0;
        assert!(session.frame(&cached).is_none());
        assert_eq!(session.last_anchor.as_ref().unwrap().row_id, "middle");
        assert_eq!(
            session.last_anchor.as_ref().unwrap().offset_within_row,
            150.0
        );
        cached.kind = "measure".into();
        cached.has_geometry = true;
        cached.row_ids = vec!["first".into(), "middle".into(), "end".into()];
        cached.positions = vec![Some(0.0), Some(500.0), Some(1000.0)];
        cached.sizes = vec![Some(100.0); 3];
        cached.messages = vec![None; 3];
        assert!(
            session.frame(&cached).is_none(),
            "remeasure does not override actual manual reading"
        );
    }
    #[test]
    fn text_only_completion_removes_phantom_anchor_space_without_overriding_reading() {
        let mut session = Session::new("a:thread".into(), Positions::default());
        session.observe(&input("old", "old-message"), None, false);
        let initial = frame("layout", 0.0, &["old"], &[0.0], &[Some("old-message")]);
        session.frame(&initial);
        session.observe(
            &input("new", "new-message"),
            Some("new-message".into()),
            false,
        );
        let mut fresh = frame(
            "layout",
            0.0,
            &["old", "new"],
            &[0.0, 600.0],
            &[Some("old-message"), Some("new-message")],
        );
        assert!(session.frame(&fresh).unwrap()["spacer"].as_f64().unwrap() > 0.0);
        session.finished("new-message");
        fresh.content_length = 976.0;
        assert_eq!(
            session.frame(&fresh).unwrap()["spacer"],
            0.0,
            "completion removes anchor filler before measuring the real end"
        );
        session.policy.manual_navigation();
        session.finished("new-message");
        assert!(
            session.frame(&fresh).is_none(),
            "settlement cannot pull a reader away from history"
        );
    }
}
