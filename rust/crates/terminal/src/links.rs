//! Which link of the terminal's screen lies under a cell: the screen's rows read through
//! `t3_client::terminal_links`, with the cells the link covers as the range the renderer underlines.
use crate::{
    model::Row,
    renderer::{Point, Range},
};
use t3_client::terminal_links::{self as links, Cells, Hit};

struct Screen<'a>(&'a [Row]);
impl Cells for Screen<'_> {
    fn rows(&self) -> usize {
        self.0.len()
    }
    fn cells(&self, row: usize) -> Vec<&str> {
        self.0[row].cells.iter().map(|cell| cell.text.as_str()).collect()
    }
    fn continues_previous(&self, row: usize) -> bool {
        self.0[row].is_wrap_continuation
    }
    fn continues_next(&self, row: usize) -> bool {
        self.0[row].wraps_to_next
    }
}

/// A link under a cell: its text and the cells it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct Under {
    pub text: String,
    pub range: Range,
}
pub fn under(rows: &[Row], column: usize, row: usize) -> Option<Under> {
    let Hit { text, start, end, .. } = links::link_at(&Screen(rows), column, row)?;
    Some(Under { text, range: Range { start: Point { x: start.0, y: start.1 }, end: Point { x: end.0, y: end.1 } } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cell, Color};

    fn blank() -> Cell {
        Cell::empty(Color { r: 0, g: 0, b: 0 }, Color { r: 0, g: 0, b: 0 })
    }

    fn row(text: &str, width: usize, continuation: bool, wraps: bool) -> Row {
        let mut cells: Vec<Cell> = text.chars().map(|character| Cell { text: character.to_string(), ..blank() }).collect();
        cells.resize(width, blank());
        Row { cells, text: text.to_owned(), is_wrap_continuation: continuation, wraps_to_next: wraps }
    }

    #[test]
    fn the_cells_a_link_covers_become_the_range_the_renderer_underlines() {
        let rows = vec![row("run tests at https://exa", 24, false, true), row("mple.com/x for more", 20, true, false)];
        let hit = under(&rows, 14, 0).unwrap();
        assert_eq!(hit.text, "https://example.com/x");
        assert_eq!((hit.range.start, hit.range.end), (Point { x: 13, y: 0 }, Point { x: 9, y: 1 }));
        assert_eq!(under(&rows, 2, 1).map(|hit| hit.text), Some("https://example.com/x".into()));
        assert!(under(&rows, 2, 0).is_none() && under(&rows, 15, 1).is_none());
    }

    #[test]
    fn a_path_with_a_position_is_one_link() {
        let rows = vec![row("  --> src/lib.rs:10:5", 30, false, false)];
        assert_eq!(under(&rows, 8, 0).map(|hit| hit.text), Some("src/lib.rs:10:5".into()));
    }
}
