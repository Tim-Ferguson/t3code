use serde::Deserialize;
use t3_terminal::{
    model::{Row, Snapshot},
    renderer::{self, Canvas, Metrics, Op, Paint, Range},
};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct View {
    #[serde(flatten)]
    header: Snapshot,
    row_data: Vec<Row>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    snapshot: View,
    metrics: Metrics,
    font_size: f64,
    font_family: String,
    padding: f64,
    force_full: bool,
    cursor_on: bool,
    previous_cursor_y: Option<i32>,
    focused: bool,
    selection_background: Option<String>,
    hovered_link_range: Option<Range>,
    origin_y: Option<f64>,
    canvas: (f64, f64),
    expected: Vec<Op>,
}
struct Recording {
    size: (f64, f64),
    commands: Vec<Op>,
}
impl Canvas for Recording {
    fn dimensions(&self) -> (f64, f64) {
        self.size
    }
    fn draw(&mut self, command: Op) {
        self.commands.push(command)
    }
}
#[test]
fn original_canvas_commands_match_rust_renderer() {
    for (index, line) in include_str!("fixtures/renderer.jsonl").lines().enumerate() {
        let fixture: Fixture = serde_json::from_str(line).unwrap();
        let mut canvas = Recording {
            size: fixture.canvas,
            commands: vec![],
        };
        renderer::paint(
            &mut canvas,
            &fixture.snapshot.header,
            &fixture.snapshot.row_data,
            Paint {
                metrics: fixture.metrics,
                font_size: fixture.font_size,
                font_family: &fixture.font_family,
                padding: fixture.padding,
                force_full: fixture.force_full,
                cursor_on: fixture.cursor_on,
                previous_cursor_y: fixture.previous_cursor_y,
                focused: fixture.focused,
                selection_background: fixture.selection_background.as_deref(),
                hovered_link: fixture.hovered_link_range,
                origin_y: fixture.origin_y,
            },
        );
        assert_eq!(
            canvas.commands, fixture.expected,
            "original canvas fixture {index}"
        );
    }
}
#[test]
fn positive_cell_metrics_and_zero_sized_host_produce_usable_grid() {
    let metrics = renderer::measured_metrics(12.0, 0.0, 0.0, 0.0);
    assert_eq!(
        metrics,
        Metrics {
            width: 1.0,
            height: 16.0,
            baseline: 14.0
        }
    );
    assert_eq!(renderer::grid_size(0.0, 0.0, metrics, 4.0), (1.0, 1.0));
}
