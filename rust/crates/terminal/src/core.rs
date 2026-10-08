//! Cells and dirty rows stay in Rust. JSON snapshots are a fixture interface,
//! not the canvas live path.
use crate::{
    model::*,
    runtime::{Allocation, Arg, Handle, Result, Runtime, error},
};
use js_sys::Function;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
#[wasm_bindgen]
pub struct TerminalCore {
    runtime: Rc<Runtime>,
    cells: Handle,
    iterator: Handle,
    render: Handle,
    input: crate::input::Input,
    terminal: Handle,
    scratch: Allocation,
    style: Allocation,
    graphemes: Option<Allocation>,
    rows: Vec<Row>,
    writer_id: u32,
}
#[wasm_bindgen]
pub async fn create_terminal(cols: u16, rows: u16, theme_json: String) -> Result<TerminalCore> {
    let theme = if theme_json.is_empty() {
        Theme::default()
    } else {
        serde_json::from_str(&theme_json).map_err(|cause| error(cause.to_string()))?
    };
    TerminalCore::create(cols, rows, &theme).await
}
impl TerminalCore {
    pub async fn create(cols: u16, rows: u16, theme: &Theme) -> Result<Self> {
        let runtime = Runtime::shared().await?;
        let options = runtime.allocate(runtime.layout("GhosttyTerminalOptions")?.size)?;
        runtime.set_field(
            options.pointer,
            "GhosttyTerminalOptions",
            "cols",
            cols.max(1) as u64,
        )?;
        runtime.set_field(
            options.pointer,
            "GhosttyTerminalOptions",
            "rows",
            rows.max(1) as u64,
        )?;
        runtime.set_field(
            options.pointer,
            "GhosttyTerminalOptions",
            "max_scrollback",
            10000,
        )?;
        let terminal = runtime.handle_with(
            "ghostty_terminal_new",
            "ghostty_terminal_free",
            &[options.pointer.into()],
        )?;
        let render = runtime.handle("ghostty_render_state_new", "ghostty_render_state_free")?;
        let iterator = runtime.handle(
            "ghostty_render_state_row_iterator_new",
            "ghostty_render_state_row_iterator_free",
        )?;
        let cells = runtime.handle(
            "ghostty_render_state_row_cells_new",
            "ghostty_render_state_row_cells_free",
        )?;
        let scratch = runtime.allocate(16)?;
        let style = runtime.allocate(runtime.layout("GhosttyStyle")?.size)?;
        let input = crate::input::Input::new(runtime.clone(), terminal.value())?;
        let core = Self {
            runtime,
            cells,
            iterator,
            render,
            input,
            terminal,
            scratch,
            style,
            graphemes: None,
            rows: vec![],
            writer_id: 0,
        };
        core.blink_default()?;
        core.apply_theme(theme)?;
        core.resize(cols, rows, 1, 1)?;
        Ok(core)
    }
    fn blink_default(&self) -> Result<()> {
        self.runtime.write(self.scratch.pointer, &[1]);
        self.runtime.success(
            "ghostty_terminal_set",
            &[
                self.terminal.value().into(),
                23u32.into(),
                self.scratch.pointer.into(),
            ],
        )
    }
    pub fn apply_theme(&self, theme: &Theme) -> Result<()> {
        for (option, color) in [
            (11u32, theme.foreground),
            (12, theme.background),
            (13, theme.cursor),
        ] {
            self.runtime
                .write(self.scratch.pointer, &[color.r, color.g, color.b]);
            self.runtime.success(
                "ghostty_terminal_set",
                &[
                    self.terminal.value().into(),
                    option.into(),
                    self.scratch.pointer.into(),
                ],
            )?;
        }
        Ok(())
    }
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }
    fn render_value(&self, tag: u32, size: u32) -> Result<Vec<u8>> {
        self.runtime.zero(self.scratch.pointer, size);
        self.runtime.success(
            "ghostty_render_state_get",
            &[
                self.render.value().into(),
                tag.into(),
                self.scratch.pointer.into(),
            ],
        )?;
        Ok(self.runtime.bytes(self.scratch.pointer, size))
    }
    fn render_bool(&self, tag: u32) -> Result<bool> {
        Ok(self.render_value(tag, 1)?[0] != 0)
    }
    fn render_u16(&self, tag: u32) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.render_value(tag, 2)?.try_into().unwrap(),
        ))
    }
    fn render_u32(&self, tag: u32) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.render_value(tag, 4)?.try_into().unwrap(),
        ))
    }
    fn color(bytes: &[u8]) -> Color {
        Color {
            r: bytes[0],
            g: bytes[1],
            b: bytes[2],
        }
    }
    fn cell_value(&self, cells: u32, tag: u32, size: u32) -> Result<Vec<u8>> {
        self.runtime.zero(self.scratch.pointer, size);
        self.runtime.success(
            "ghostty_render_state_row_cells_get",
            &[cells.into(), tag.into(), self.scratch.pointer.into()],
        )?;
        Ok(self.runtime.bytes(self.scratch.pointer, size))
    }
    fn cell_color(&self, cells: u32, tag: u32, fallback: Color) -> Color {
        self.cell_value(cells, tag, 3)
            .map(|value| Self::color(&value))
            .unwrap_or(fallback)
    }
    fn read_row(
        &mut self,
        iterator: u32,
        cols: u16,
        foreground: Color,
        background: Color,
    ) -> Result<Row> {
        self.runtime.success(
            "ghostty_render_state_row_get",
            &[iterator.into(), 2u32.into(), self.scratch.pointer.into()],
        )?;
        let row = self.runtime.u64(self.scratch.pointer);
        self.runtime.zero(self.scratch.pointer + 8, 1);
        self.runtime.success(
            "ghostty_row_get",
            &[
                Arg::Wide(row),
                2u32.into(),
                (self.scratch.pointer + 8).into(),
            ],
        )?;
        let is_wrap_continuation = self.runtime.bytes(self.scratch.pointer + 8, 1)[0] != 0;
        self.runtime.zero(self.scratch.pointer + 8, 1);
        self.runtime.success(
            "ghostty_row_get",
            &[
                Arg::Wide(row),
                1u32.into(),
                (self.scratch.pointer + 8).into(),
            ],
        )?;
        let wraps_to_next = self.runtime.bytes(self.scratch.pointer + 8, 1)[0] != 0;
        self.runtime.success(
            "ghostty_render_state_row_get",
            &[iterator.into(), 3u32.into(), self.cells.slot.into()],
        )?;
        let cells = self.cells.value();
        let mut output: Vec<Cell> = vec![];
        while output.len() < cols as usize
            && self
                .runtime
                .call("ghostty_render_state_row_cells_next", &[cells.into()])?
                != 0
        {
            let mut front = self.cell_color(cells, 6, foreground);
            let mut back = self.cell_color(cells, 5, background);
            self.runtime.zero(self.style.pointer, self.style.size);
            self.runtime.set_field(
                self.style.pointer,
                "GhosttyStyle",
                "size",
                self.style.size as u64,
            )?;
            self.runtime.call(
                "ghostty_render_state_row_cells_get",
                &[cells.into(), 2u32.into(), self.style.pointer.into()],
            )?;
            let length = u32::from_le_bytes(self.cell_value(cells, 3, 4)?.try_into().unwrap());
            let mut text = String::new();
            if length > 0 {
                let needed = length
                    .checked_mul(4)
                    .ok_or_else(|| error("Ghostty grapheme exceeds address space"))?;
                if self
                    .graphemes
                    .as_ref()
                    .is_none_or(|buffer| buffer.size < needed)
                {
                    let size = needed.max(
                        self.graphemes
                            .as_ref()
                            .map_or(0, |buffer| buffer.size.saturating_mul(2)),
                    );
                    self.graphemes = Some(self.runtime.allocate(size)?);
                }
                let buffer = self.graphemes.as_ref().unwrap();
                if self.runtime.call(
                    "ghostty_render_state_row_cells_get",
                    &[cells.into(), 4u32.into(), buffer.pointer.into()],
                )? == 0
                {
                    for bytes in self.runtime.bytes(buffer.pointer, needed).chunks_exact(4) {
                        text.push(
                            char::from_u32(u32::from_le_bytes(bytes.try_into().unwrap()))
                                .unwrap_or(char::REPLACEMENT_CHARACTER),
                        );
                    }
                }
            }
            let mut wide = 0;
            if text.is_empty() && output.last().is_some_and(|cell| !cell.text.is_empty()) {
                self.runtime.success(
                    "ghostty_render_state_row_cells_get",
                    &[cells.into(), 1u32.into(), self.scratch.pointer.into()],
                )?;
                let raw = self.runtime.u64(self.scratch.pointer);
                self.runtime.zero(self.scratch.pointer + 8, 4);
                self.runtime.success(
                    "ghostty_cell_get",
                    &[
                        Arg::Wide(raw),
                        3u32.into(),
                        (self.scratch.pointer + 8).into(),
                    ],
                )?;
                wide = self.runtime.u32(self.scratch.pointer + 8);
            }
            let selected = self.cell_value(cells, 7, 1)?[0] != 0;
            let field = |name| {
                self.runtime
                    .get_field(self.style.pointer, "GhosttyStyle", name)
                    .map(|value| value != 0)
            };
            if field("inverse")? {
                std::mem::swap(&mut front, &mut back)
            }
            if field("faint")? {
                front = front.faint(back)
            }
            output.push(Cell {
                text,
                wide,
                foreground: front,
                background: back,
                bold: field("bold")?,
                italic: field("italic")?,
                invisible: field("invisible")?,
                strikethrough: field("strikethrough")?,
                overline: field("overline")?,
                underline: field("underline")?,
                selected,
            });
        }
        while output.len() < cols as usize {
            output.push(Cell::empty(foreground, background));
        }
        let text: String = output
            .iter()
            .map(|cell| {
                if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                }
            })
            .collect();
        Ok(Row {
            cells: output,
            text: text.trim_end_matches(crate::model::js_space).to_owned(),
            is_wrap_continuation,
            wraps_to_next,
        })
    }
    pub fn update(&mut self) -> Result<Snapshot> {
        self.runtime.success(
            "ghostty_render_state_update",
            &[self.render.value().into(), self.terminal.value().into()],
        )?;
        let cols = self.render_u16(1)?;
        let rows = self.render_u16(2)?;
        let dirty = self.render_u32(3)?;
        let foreground = self
            .render_value(6, 3)
            .map(|bytes| Self::color(&bytes))
            .unwrap_or(Theme::default().foreground);
        let background = self
            .render_value(5, 3)
            .map(|bytes| Self::color(&bytes))
            .unwrap_or(Theme::default().background);
        let cursor = if self.render_bool(8)? {
            self.render_value(7, 3)
                .map(|bytes| Self::color(&bytes))
                .unwrap_or(foreground)
        } else {
            foreground
        };
        let in_viewport = self.render_bool(14)?;
        let cursor_visible = self.render_bool(11)? && in_viewport;
        let cursor_x = if in_viewport {
            self.render_u16(15)? as i32
        } else {
            -1
        };
        let cursor_y = if in_viewport {
            self.render_u16(16)? as i32
        } else {
            -1
        };
        if self.rows.len() != rows as usize
            || self.rows.iter().any(|row| row.cells.len() != cols as usize)
        {
            self.rows = (0..rows)
                .map(|_| Row {
                    cells: vec![Cell::empty(foreground, background); cols as usize],
                    text: String::new(),
                    is_wrap_continuation: false,
                    wraps_to_next: false,
                })
                .collect();
        }
        let mut dirty_rows = vec![];
        if dirty != 0 {
            self.runtime.success(
                "ghostty_render_state_get",
                &[
                    self.render.value().into(),
                    4u32.into(),
                    self.iterator.slot.into(),
                ],
            )?;
            let iterator = self.iterator.value();
            let mut row = 0;
            while row < rows as usize
                && self
                    .runtime
                    .call("ghostty_render_state_row_iterator_next", &[iterator.into()])?
                    != 0
            {
                self.runtime.zero(self.scratch.pointer, 1);
                self.runtime.success(
                    "ghostty_render_state_row_get",
                    &[iterator.into(), 1u32.into(), self.scratch.pointer.into()],
                )?;
                if dirty == 2 || self.runtime.bytes(self.scratch.pointer, 1)[0] != 0 {
                    self.rows[row] = self.read_row(iterator, cols, foreground, background)?;
                    dirty_rows.push(row);
                    self.runtime.zero(self.scratch.pointer, 1);
                    self.runtime.call(
                        "ghostty_render_state_row_set",
                        &[iterator.into(), 0u32.into(), self.scratch.pointer.into()],
                    )?;
                }
                row += 1;
            }
            self.runtime.zero(self.scratch.pointer, 4);
            self.runtime.call(
                "ghostty_render_state_set",
                &[
                    self.render.value().into(),
                    0u32.into(),
                    self.scratch.pointer.into(),
                ],
            )?;
        }
        Ok(Snapshot {
            cols,
            rows,
            foreground,
            background,
            cursor,
            cursor_x,
            cursor_y,
            cursor_visible,
            cursor_blinking: self.render_bool(12)?,
            cursor_style: self.render_u32(10)?,
            dirty_rows,
        })
    }
}
#[wasm_bindgen]
impl TerminalCore {
    pub fn encode_key(&self, event_json: &str) -> Result<String> {
        let event = serde_json::from_str(event_json)
            .map_err(|cause| error(format!("Invalid keyboard event: {cause}")))?;
        self.input.key(&event)
    }
    pub fn encode_mouse(&self, event_json: &str) -> Result<String> {
        let event = serde_json::from_str(event_json)
            .map_err(|cause| error(format!("Invalid mouse event: {cause}")))?;
        self.input.mouse(&event)
    }
    pub fn encode_paste(&self, data: &str) -> Result<String> {
        self.input.paste(data)
    }
    pub fn write(&self, data: &str) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let bytes = self.runtime.allocate(data.len() as u32)?;
        self.runtime.write(bytes.pointer, data.as_bytes());
        self.runtime.call(
            "ghostty_terminal_vt_write",
            &[
                self.terminal.value().into(),
                bytes.pointer.into(),
                bytes.size.into(),
            ],
        )?;
        Ok(())
    }
    pub fn reset_and_write(&mut self, data: &str) -> Result<()> {
        self.runtime
            .call("ghostty_terminal_reset", &[self.terminal.value().into()])?;
        self.blink_default()?;
        self.rows.clear();
        // Suppress terminal protocol replies from historical replay.
        if self.writer_id != 0 {
            self.runtime.call(
                "ghostty_terminal_set",
                &[self.terminal.value().into(), 1u32.into(), 0u32.into()],
            )?;
        }
        let result = self.write(data);
        if self.writer_id != 0 {
            self.runtime.call(
                "ghostty_terminal_set",
                &[
                    self.terminal.value().into(),
                    1u32.into(),
                    self.runtime.callback_index().into(),
                ],
            )?;
        }
        result
    }
    pub fn set_writer(&mut self, callback: Function) -> Result<()> {
        if self.writer_id != 0 {
            self.runtime
                .detach_writer(self.terminal.value(), self.writer_id);
            self.writer_id = 0;
        }
        self.writer_id = self.runtime.attach_writer(
            self.terminal.value(),
            std::rc::Rc::new(move |data| {
                let _ = callback.call1(&JsValue::UNDEFINED, &data.into());
            }),
        )?;
        Ok(())
    }
    pub fn resize(&self, cols: u16, rows: u16, cell_width: u32, cell_height: u32) -> Result<()> {
        self.runtime.success(
            "ghostty_terminal_resize",
            &[
                self.terminal.value().into(),
                (cols.max(1) as u32).into(),
                (rows.max(1) as u32).into(),
                cell_width.max(1).into(),
                cell_height.max(1).into(),
            ],
        )
    }
    pub fn snapshot_json(&mut self) -> Result<String> {
        let snapshot = self.update()?;
        let mut value = serde_json::to_value(snapshot).map_err(|cause| error(cause.to_string()))?;
        value["rowData"] =
            serde_json::to_value(&self.rows).map_err(|cause| error(cause.to_string()))?;
        serde_json::to_string(&value).map_err(|cause| error(cause.to_string()))
    }
    pub fn scroll(&self, delta: i32) -> Result<()> {
        let options = self
            .runtime
            .allocate(self.runtime.layout("GhosttyTerminalScrollViewport")?.size)?;
        self.runtime
            .set_field(options.pointer, "GhosttyTerminalScrollViewport", "tag", 2)?;
        let offset = self
            .runtime
            .field("GhosttyTerminalScrollViewport", "value")?
            .offset;
        self.runtime
            .write(options.pointer + offset, &delta.to_le_bytes());
        self.runtime.call(
            "ghostty_terminal_scroll_viewport",
            &[self.terminal.value().into(), options.pointer.into()],
        )?;
        Ok(())
    }
    pub fn scroll_to_bottom(&self) -> Result<()> {
        let options = self
            .runtime
            .allocate(self.runtime.layout("GhosttyTerminalScrollViewport")?.size)?;
        self.runtime
            .set_field(options.pointer, "GhosttyTerminalScrollViewport", "tag", 1)?;
        self.runtime.call(
            "ghostty_terminal_scroll_viewport",
            &[self.terminal.value().into(), options.pointer.into()],
        )?;
        Ok(())
    }
    pub fn mode(&self, mode: u32) -> Result<bool> {
        self.runtime.zero(self.scratch.pointer, 1);
        self.runtime.success(
            "ghostty_terminal_mode_get",
            &[
                self.terminal.value().into(),
                mode.into(),
                self.scratch.pointer.into(),
            ],
        )?;
        Ok(self.runtime.bytes(self.scratch.pointer, 1)[0] != 0)
    }
}
impl Drop for TerminalCore {
    fn drop(&mut self) {
        if self.writer_id != 0 {
            self.runtime
                .detach_writer(self.terminal.value(), self.writer_id)
        }
    }
}

#[path = "selection.rs"]
pub mod selection;
