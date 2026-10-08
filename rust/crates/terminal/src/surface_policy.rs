//! Pure event policies mirror the original terminal surface helpers.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClickSequence {
    pub count: u32,
    pub time: f64,
    pub x: f64,
    pub y: f64,
}
pub fn advance_click(previous: Option<ClickSequence>, time: f64, x: f64, y: f64) -> ClickSequence {
    let repeat = previous
        .is_some_and(|last| time - last.time <= 500.0 && (x - last.x).hypot(y - last.y) <= 4.0);
    let count = if repeat {
        previous
            .map(|last| if last.count >= 3 { 1 } else { last.count + 1 })
            .unwrap()
    } else {
        1
    };
    ClickSequence { count, time, x, y }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MouseDecision {
    pub send: bool,
    pub next_motion_data: String,
}
pub fn mouse_data(action: &str, data: &str, previous: &str) -> MouseDecision {
    MouseDecision {
        send: !data.is_empty() && (action != "motion" || data != previous),
        next_motion_data: if action == "motion" {
            data.to_owned()
        } else {
            String::new()
        },
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackingDecision {
    pub tracking: bool,
    pub motion_data: String,
}
pub fn mouse_tracking(previous: bool, tracking: bool, data: &str) -> TrackingDecision {
    TrackingDecision {
        tracking,
        motion_data: if previous == tracking {
            data.to_owned()
        } else {
            String::new()
        },
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Wheel {
    pub rows: i32,
    pub remainder: f64,
}
pub fn wheel(delta: f64, mode: u32, height: f64, viewport_rows: u16, remainder: f64) -> Wheel {
    let pixels = match mode {
        1 => delta * height,
        2 => delta * viewport_rows as f64 * height,
        _ => delta,
    };
    let total = remainder + pixels / height;
    let rows = total.trunc() as i32;
    Wheel {
        rows,
        remainder: total - rows as f64,
    }
}
pub fn wheel_arrows(rows: i32, application: bool) -> String {
    let sequence = match (rows < 0, application) {
        (true, true) => "\x1bOA",
        (false, true) => "\x1bOB",
        (true, false) => "\x1b[A",
        (false, false) => "\x1b[B",
    };
    sequence.repeat(rows.unsigned_abs() as usize)
}
