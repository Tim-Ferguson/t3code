//! The original dirty-row canvas policy, including style runs that do not
//! split at selection boundaries and the previous cursor row repaint.
use crate::model::{Cell, Row, Snapshot};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub width: f64,
    pub height: f64,
    pub baseline: f64,
}
pub fn measured_metrics(font_size: f64, width: f64, ascent: f64, descent: f64) -> Metrics {
    let ascent = if ascent == 0.0 { font_size } else { ascent };
    let glyph_height = ascent + descent;
    let height = 1.0_f64
        .max((font_size * 1.35).round())
        .max(glyph_height.ceil());
    Metrics {
        width: width.max(1.0),
        height,
        baseline: ((height - glyph_height) / 2.0 + ascent).round(),
    }
}
pub fn grid_size(width: f64, height: f64, metrics: Metrics, padding: f64) -> (f64, f64) {
    (
        ((width - padding * 2.0) / metrics.width).floor().max(1.0),
        ((height - padding * 2.0) / metrics.height).floor().max(1.0),
    )
}
fn same_style(a: &Cell, b: &Cell) -> bool {
    a.foreground == b.foreground
        && a.bold == b.bold
        && a.italic == b.italic
        && a.invisible == b.invisible
}
pub fn text_run_end(cells: &[Cell], start: usize) -> usize {
    let mut end = start + 1;
    while end < cells.len() {
        let next = &cells[end];
        if next.wide == 2 {
            end += 1;
            continue;
        }
        if next.text.is_empty() || !same_style(&cells[start], next) {
            break;
        }
        end += 1;
    }
    end
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "camelCase")]
pub enum Op {
    Save,
    Restore,
    ResetTransform,
    BeginPath,
    Clip,
    FillStyle(String),
    StrokeStyle(String),
    TextBaseline(String),
    Font(String),
    FillRect(f64, f64, f64, f64),
    StrokeRect(f64, f64, f64, f64),
    Rect(f64, f64, f64, f64),
    FillText(String, f64, f64, f64),
}
pub trait Canvas {
    fn dimensions(&self) -> (f64, f64);
    fn draw(&mut self, op: Op);
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: usize,
    pub y: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Range {
    pub start: Point,
    pub end: Point,
}
pub struct Paint<'a> {
    pub metrics: Metrics,
    pub font_size: f64,
    pub font_family: &'a str,
    pub padding: f64,
    pub force_full: bool,
    pub cursor_on: bool,
    pub previous_cursor_y: Option<i32>,
    pub focused: bool,
    pub selection_background: Option<&'a str>,
    pub hovered_link: Option<Range>,
    pub origin_y: Option<f64>,
}
fn font(cell: &Cell, size: f64, family: &str) -> String {
    format!(
        "{} {} {size}px {family}",
        if cell.italic { "italic" } else { "normal" },
        if cell.bold { "700" } else { "400" }
    )
}
pub fn paint(canvas: &mut impl Canvas, snapshot: &Snapshot, rows: &[Row], options: Paint<'_>) {
    let Metrics {
        width,
        height,
        baseline,
    } = options.metrics;
    let mut draw_rows = if options.force_full {
        (0..snapshot.rows as usize).collect()
    } else {
        snapshot.dirty_rows.clone()
    };
    if let Some(previous) = options.previous_cursor_y.filter(|row| *row >= 0) {
        if !draw_rows.contains(&(previous as usize)) {
            draw_rows.push(previous as usize)
        }
    }
    if snapshot.cursor_visible
        && snapshot.cursor_y >= 0
        && !draw_rows.contains(&(snapshot.cursor_y as usize))
    {
        draw_rows.push(snapshot.cursor_y as usize)
    }
    if options.force_full {
        canvas.draw(Op::Save);
        canvas.draw(Op::ResetTransform);
        canvas.draw(Op::FillStyle(snapshot.background.css()));
        let (w, h) = canvas.dimensions();
        canvas.draw(Op::FillRect(0.0, 0.0, w, h));
        canvas.draw(Op::Restore);
    }
    canvas.draw(Op::TextBaseline("alphabetic".into()));
    let origin_y = options.origin_y.unwrap_or(options.padding);
    for row_index in draw_rows {
        let Some(row) = rows.get(row_index) else {
            continue;
        };
        let top = origin_y + row_index as f64 * height;
        canvas.draw(Op::FillStyle(snapshot.background.css()));
        canvas.draw(Op::FillRect(
            options.padding,
            top,
            snapshot.cols as f64 * width,
            height,
        ));
        let mut start = 0;
        while start < row.cells.len() {
            let cell = &row.cells[start];
            let mut end = start + 1;
            while end < row.cells.len()
                && row.cells[end].selected == cell.selected
                && row.cells[end].background == cell.background
            {
                end += 1;
            }
            if cell.selected || cell.background != snapshot.background {
                let left = options.padding + start as f64 * width;
                let run_width = (end - start) as f64 * width;
                if cell.background != snapshot.background {
                    canvas.draw(Op::FillStyle(cell.background.css()));
                    canvas.draw(Op::FillRect(left, top, run_width, height));
                }
                if cell.selected {
                    canvas.draw(Op::FillStyle(
                        options
                            .selection_background
                            .unwrap_or("rgba(72, 122, 191, 0.35)")
                            .into(),
                    ));
                    canvas.draw(Op::FillRect(left, top, run_width, height));
                }
            }
            start = end;
        }
        let mut start = 0;
        while start < row.cells.len() {
            let cell = &row.cells[start];
            if cell.text.is_empty() {
                start += 1;
                continue;
            }
            let end = text_run_end(&row.cells, start);
            let text: String = row.cells[start..end]
                .iter()
                .map(|cell| cell.text.as_str())
                .collect();
            if !cell.invisible && !text.trim_matches(crate::model::js_space).is_empty() {
                let left = options.padding + start as f64 * width;
                let run_width = (end - start) as f64 * width;
                canvas.draw(Op::Save);
                canvas.draw(Op::BeginPath);
                canvas.draw(Op::Rect(left, top, run_width, height));
                canvas.draw(Op::Clip);
                canvas.draw(Op::Font(font(cell, options.font_size, options.font_family)));
                canvas.draw(Op::FillStyle(cell.foreground.css()));
                canvas.draw(Op::FillText(text, left, top + baseline, run_width));
                canvas.draw(Op::Restore);
            }
            start = end;
        }
        for (column, cell) in row.cells.iter().enumerate() {
            let hovered = options.hovered_link.is_some_and(|range| {
                row_index >= range.start.y
                    && row_index <= range.end.y
                    && (row_index > range.start.y || column >= range.start.x)
                    && (row_index < range.end.y || column <= range.end.x)
            });
            if !cell.underline && !cell.strikethrough && !cell.overline && !hovered {
                continue;
            }
            canvas.draw(Op::FillStyle(cell.foreground.css()));
            let left = options.padding + column as f64 * width;
            if cell.underline || hovered {
                canvas.draw(Op::FillRect(left, top + height - 2.0, width, 1.0));
            }
            if cell.strikethrough {
                canvas.draw(Op::FillRect(
                    left,
                    top + (height * 0.55).floor(),
                    width,
                    1.0,
                ));
            }
            if cell.overline {
                canvas.draw(Op::FillRect(left, top + 1.0, width, 1.0));
            }
        }
    }
    if options.cursor_on
        && snapshot.cursor_visible
        && snapshot.cursor_x >= 0
        && snapshot.cursor_y >= 0
    {
        let left = options.padding + snapshot.cursor_x as f64 * width;
        let top = origin_y + snapshot.cursor_y as f64 * height;
        canvas.draw(Op::FillStyle(snapshot.cursor.css()));
        if !options.focused || snapshot.cursor_style == 3 {
            canvas.draw(Op::StrokeStyle(snapshot.cursor.css()));
            canvas.draw(Op::StrokeRect(
                left + 0.5,
                top + 0.5,
                width - 1.0,
                height - 1.0,
            ));
        } else if snapshot.cursor_style == 0 {
            canvas.draw(Op::FillRect(left, top, 2.0, height));
        } else if snapshot.cursor_style == 2 {
            canvas.draw(Op::FillRect(left, top + height - 2.0, width, 2.0));
        } else {
            canvas.draw(Op::FillRect(left, top, width, height));
            if let Some(cell) = rows
                .get(snapshot.cursor_y as usize)
                .and_then(|row| row.cells.get(snapshot.cursor_x as usize))
                .filter(|cell| !cell.text.is_empty())
            {
                canvas.draw(Op::Font(font(cell, options.font_size, options.font_family)));
                canvas.draw(Op::FillStyle(snapshot.background.css()));
                canvas.draw(Op::FillText(cell.text.clone(), left, top + baseline, width));
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub struct WebCanvas(pub web_sys::CanvasRenderingContext2d);
#[cfg(target_arch = "wasm32")]
impl Canvas for WebCanvas {
    fn dimensions(&self) -> (f64, f64) {
        self.0
            .canvas()
            .map(|canvas| (canvas.width() as f64, canvas.height() as f64))
            .unwrap_or_default()
    }
    fn draw(&mut self, op: Op) {
        let context = &self.0;
        match op {
            Op::Save => context.save(),
            Op::Restore => context.restore(),
            Op::ResetTransform => {
                let _ = context.reset_transform();
            }
            Op::BeginPath => context.begin_path(),
            Op::Clip => context.clip(),
            Op::FillStyle(value) => context.set_fill_style_str(&value),
            Op::StrokeStyle(value) => context.set_stroke_style_str(&value),
            Op::TextBaseline(value) => context.set_text_baseline(&value),
            Op::Font(value) => context.set_font(&value),
            Op::FillRect(x, y, w, h) => context.fill_rect(x, y, w, h),
            Op::StrokeRect(x, y, w, h) => context.stroke_rect(x, y, w, h),
            Op::Rect(x, y, w, h) => context.rect(x, y, w, h),
            Op::FillText(text, x, y, w) => {
                let _ = context.fill_text_with_max_width(&text, x, y, w);
            }
        }
    }
}
