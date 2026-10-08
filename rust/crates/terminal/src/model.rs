use serde::{Deserialize, Serialize};
pub(crate) fn js_space(ch: char) -> bool {
    matches!(ch,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}
impl Color {
    pub fn css(self) -> String {
        format!("rgb({}, {}, {})", self.r, self.g, self.b)
    }
    pub fn faint(self, background: Self) -> Self {
        let blend =
            |front: u8, back: u8| ((u32::from(front) * 155 + u32::from(back) * 100) / 255) as u8;
        Self {
            r: blend(self.r, background.r),
            g: blend(self.g, background.g),
            b: blend(self.b, background.b),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Theme {
    pub foreground: Color,
    pub background: Color,
    pub cursor: Color,
    #[serde(default)]
    pub selection_background: Option<String>,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: Color {
                r: 229,
                g: 231,
                b: 235,
            },
            background: Color { r: 0, g: 0, b: 0 },
            cursor: Color {
                r: 229,
                g: 231,
                b: 235,
            },
            selection_background: None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cell {
    pub text: String,
    pub wide: u32,
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub italic: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: bool,
    pub selected: bool,
}
impl Cell {
    pub fn empty(foreground: Color, background: Color) -> Self {
        Self {
            text: String::new(),
            wide: 0,
            foreground,
            background,
            bold: false,
            italic: false,
            invisible: false,
            strikethrough: false,
            overline: false,
            underline: false,
            selected: false,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub cells: Vec<Cell>,
    pub text: String,
    pub is_wrap_continuation: bool,
    pub wraps_to_next: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    pub foreground: Color,
    pub background: Color,
    pub cursor: Color,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub cursor_visible: bool,
    pub cursor_blinking: bool,
    pub cursor_style: u32,
    pub dirty_rows: Vec<usize>,
}
