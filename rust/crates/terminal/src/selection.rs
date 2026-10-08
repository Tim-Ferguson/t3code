use super::TerminalCore;
use crate::runtime::{Allocation, Result, error};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Point {
    pub x: u16,
    pub y: u32,
    #[serde(default = "viewport")]
    pub tag: u32,
}
fn viewport() -> u32 {
    1
}
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Coordinate {
    pub x: u16,
    pub y: u32,
}
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Range {
    pub start: Coordinate,
    pub end: Coordinate,
}
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Selection {
    pub viewport: Range,
    pub screen: Range,
}
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Scrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}
impl TerminalCore {
    fn grid_ref(&self, point: Point) -> Result<Allocation> {
        let rt = &self.runtime;
        let layout = rt.layout("GhosttyPoint")?;
        let input = rt.allocate(layout.size)?;
        rt.set_field(input.pointer, "GhosttyPoint", "tag", point.tag as u64)?;
        let offset = layout.fields["value"].offset;
        rt.write(input.pointer + offset, &point.x.to_le_bytes());
        rt.write(input.pointer + offset + 4, &point.y.to_le_bytes());
        let layout = rt.layout("GhosttyGridRef")?;
        let reference = rt.allocate(layout.size)?;
        rt.set_field(
            reference.pointer,
            "GhosttyGridRef",
            "size",
            layout.size as u64,
        )?;
        rt.success(
            "ghostty_terminal_grid_ref",
            &[
                self.terminal.value().into(),
                input.pointer.into(),
                reference.pointer.into(),
            ],
        )?;
        Ok(reference)
    }
    fn point_from_ref(&self, reference: u32, tag: u32) -> Result<Option<Coordinate>> {
        let rt = &self.runtime;
        let output = rt.allocate(rt.layout("GhosttyPointCoordinate")?.size)?;
        if rt.call(
            "ghostty_terminal_point_from_grid_ref",
            &[
                self.terminal.value().into(),
                reference.into(),
                tag.into(),
                output.pointer.into(),
            ],
        )? != 0
        {
            return Ok(None);
        }
        Ok(Some(Coordinate {
            x: rt.get_field(output.pointer, "GhosttyPointCoordinate", "x")? as u16,
            y: rt.get_field(output.pointer, "GhosttyPointCoordinate", "y")? as u32,
        }))
    }
    pub fn convert_point(&self, point: Point, to_tag: u32) -> Result<Option<Coordinate>> {
        let reference = self.grid_ref(point)?;
        self.point_from_ref(reference.pointer, to_tag)
    }
    pub fn set_selection(&self, anchor: Point, end: Point) -> Result<()> {
        let rt = &self.runtime;
        let layout = rt.layout("GhosttySelection")?;
        let selection = rt.allocate(layout.size)?;
        rt.set_field(
            selection.pointer,
            "GhosttySelection",
            "size",
            layout.size as u64,
        )?;
        for (name, point) in [("start", anchor), ("end", end)] {
            let reference = self.grid_ref(point)?;
            let field = &layout.fields[name];
            rt.write(
                selection.pointer + field.offset,
                &rt.bytes(reference.pointer, field.size),
            );
        }
        rt.call(
            "ghostty_terminal_set",
            &[
                self.terminal.value().into(),
                21u32.into(),
                selection.pointer.into(),
            ],
        )?;
        Ok(())
    }
    pub fn select_at(&self, col: u16, row: u32, line: bool) -> Result<Option<Selection>> {
        let rt = &self.runtime;
        let (name, operation) = if line {
            (
                "GhosttyTerminalSelectLineOptions",
                "ghostty_terminal_select_line",
            )
        } else {
            (
                "GhosttyTerminalSelectWordOptions",
                "ghostty_terminal_select_word",
            )
        };
        let layout = rt.layout(name)?;
        let options = rt.allocate(layout.size)?;
        rt.set_field(options.pointer, name, "size", layout.size as u64)?;
        let reference = self.grid_ref(Point {
            x: col,
            y: row,
            tag: 1,
        })?;
        let field = &layout.fields["ref"];
        rt.write(
            options.pointer + field.offset,
            &rt.bytes(reference.pointer, field.size),
        );
        let layout = rt.layout("GhosttySelection")?;
        let selection = rt.allocate(layout.size)?;
        rt.set_field(
            selection.pointer,
            "GhosttySelection",
            "size",
            layout.size as u64,
        )?;
        if rt.call(
            operation,
            &[
                self.terminal.value().into(),
                options.pointer.into(),
                selection.pointer.into(),
            ],
        )? != 0
        {
            return Ok(None);
        }
        let start = selection.pointer + layout.fields["start"].offset;
        let end = selection.pointer + layout.fields["end"].offset;
        let range = match (
            self.point_from_ref(start, 1)?,
            self.point_from_ref(end, 1)?,
            self.point_from_ref(start, 2)?,
            self.point_from_ref(end, 2)?,
        ) {
            (Some(vs), Some(ve), Some(ss), Some(se)) => Some(Selection {
                viewport: Range { start: vs, end: ve },
                screen: Range { start: ss, end: se },
            }),
            _ => None,
        };
        rt.call(
            "ghostty_terminal_set",
            &[
                self.terminal.value().into(),
                21u32.into(),
                selection.pointer.into(),
            ],
        )?;
        Ok(range)
    }
    pub fn scrollbar(&self) -> Result<Option<Scrollbar>> {
        let rt = &self.runtime;
        let output = rt.allocate(rt.layout("GhosttyTerminalScrollbar")?.size)?;
        if rt.call(
            "ghostty_terminal_get",
            &[
                self.terminal.value().into(),
                9u32.into(),
                output.pointer.into(),
            ],
        )? != 0
        {
            return Ok(None);
        }
        Ok(Some(Scrollbar {
            total: rt.get_field(output.pointer, "GhosttyTerminalScrollbar", "total")?,
            offset: rt.get_field(output.pointer, "GhosttyTerminalScrollbar", "offset")?,
            len: rt.get_field(output.pointer, "GhosttyTerminalScrollbar", "len")?,
        }))
    }
    fn text_output(
        &self,
        mut operation: impl FnMut(u32, u32, u32) -> Result<i32>,
    ) -> Result<String> {
        let rt = &self.runtime;
        let written = rt.allocate(4)?;
        let result = operation(0, 0, written.pointer)?;
        let size = rt.u32(written.pointer);
        if result != -3 || size == 0 {
            return Ok(String::new());
        }
        let output = rt.allocate(size)?;
        if operation(output.pointer, size, written.pointer)? != 0 {
            return Ok(String::new());
        }
        Ok(crate::input::decode(
            &rt.bytes(output.pointer, rt.u32(written.pointer)),
        ))
    }
    pub fn terminal_bool(&self, tag: u32) -> Result<bool> {
        self.runtime.zero(self.scratch.pointer, 4);
        Ok(self.runtime.call(
            "ghostty_terminal_get",
            &[
                self.terminal.value().into(),
                tag.into(),
                self.scratch.pointer.into(),
            ],
        )? == 0
            && self.runtime.bytes(self.scratch.pointer, 1)[0] != 0)
    }
}
#[wasm_bindgen]
impl TerminalCore {
    pub fn set_selection_json(&self, anchor: &str, end: &str) -> Result<()> {
        self.set_selection(
            serde_json::from_str(anchor).map_err(|e| error(e.to_string()))?,
            serde_json::from_str(end).map_err(|e| error(e.to_string()))?,
        )
    }
    pub fn clear_selection(&self) -> Result<()> {
        self.runtime.call(
            "ghostty_terminal_set",
            &[self.terminal.value().into(), 21u32.into(), 0u32.into()],
        )?;
        Ok(())
    }
    pub fn select_all(&self) -> Result<()> {
        let rt = &self.runtime;
        let layout = rt.layout("GhosttySelection")?;
        let output = rt.allocate(layout.size)?;
        rt.set_field(
            output.pointer,
            "GhosttySelection",
            "size",
            layout.size as u64,
        )?;
        if rt.call(
            "ghostty_terminal_select_all",
            &[self.terminal.value().into(), output.pointer.into()],
        )? == 0
        {
            rt.call(
                "ghostty_terminal_set",
                &[
                    self.terminal.value().into(),
                    21u32.into(),
                    output.pointer.into(),
                ],
            )?;
        }
        Ok(())
    }
    pub fn select_word_json(&self, col: u16, row: u32) -> Result<String> {
        serde_json::to_string(&self.select_at(col, row, false)?).map_err(|e| error(e.to_string()))
    }
    pub fn select_line_json(&self, col: u16, row: u32) -> Result<String> {
        serde_json::to_string(&self.select_at(col, row, true)?).map_err(|e| error(e.to_string()))
    }
    pub fn selection_text(&self) -> Result<String> {
        let rt = &self.runtime;
        let options = rt.allocate(16)?;
        rt.write(options.pointer, &16u32.to_le_bytes());
        rt.write(options.pointer + 8, &[1, 1]);
        self.text_output(|pointer, size, written| {
            rt.call(
                "ghostty_terminal_selection_format_buf",
                &[
                    self.terminal.value().into(),
                    options.pointer.into(),
                    pointer.into(),
                    size.into(),
                    written.into(),
                ],
            )
        })
    }
    pub fn hyperlink_at(&self, col: u16, row: u32) -> Result<Option<String>> {
        let reference = self.grid_ref(Point {
            x: col,
            y: row,
            tag: 1,
        })?;
        let text = self.text_output(|pointer, size, written| {
            self.runtime.call(
                "ghostty_grid_ref_hyperlink_uri",
                &[
                    reference.pointer.into(),
                    pointer.into(),
                    size.into(),
                    written.into(),
                ],
            )
        })?;
        Ok((!text.is_empty()).then_some(text))
    }
    pub fn scrollbar_json(&self) -> Result<String> {
        serde_json::to_string(&self.scrollbar()?).map_err(|e| error(e.to_string()))
    }
    pub fn point_json(&self, col: u16, row: u32, from: u32, to: u32) -> Result<String> {
        serde_json::to_string(&self.convert_point(
            Point {
                x: col,
                y: row,
                tag: from,
            },
            to,
        )?)
        .map_err(|e| error(e.to_string()))
    }
    pub fn is_mouse_tracking(&self) -> Result<bool> {
        self.terminal_bool(11)
    }
    pub fn is_viewport_active(&self) -> Result<bool> {
        self.terminal_bool(32)
    }
    pub fn is_alternate_screen(&self) -> Result<bool> {
        self.runtime.zero(self.scratch.pointer, 4);
        Ok(self.runtime.call(
            "ghostty_terminal_get",
            &[
                self.terminal.value().into(),
                6u32.into(),
                self.scratch.pointer.into(),
            ],
        )? == 0
            && self.runtime.u32(self.scratch.pointer) == 1)
    }
}
