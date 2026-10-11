//! What the file panel shows a file as, besides its text: a table for delimited text, a player for
//! audio and video, a frame for a PDF. Port of `packages/shared/src/delimitedPreview.ts`,
//! `filePreview.ts` (`isWorkspaceVideoPreviewPath`, `isWorkspaceAudioPreviewPath`), `video.ts` and
//! `BrowserDocumentFrame.tsx` (`isPdfPreviewFile`, the viewer fragment).

pub const MAX_ROWS: usize = 100;
pub const MAX_COLUMNS: usize = 30;
pub const MAX_CELL: usize = 2000;
/// Chromium's viewer opens with its own toolbar and a thumbnail rail; the panel header is the only chrome.
pub const PDF_VIEWER_FRAGMENT: &str = "#toolbar=0&view=FitH";
/// Where the choice between the table and the source is remembered (`RENDER_TABLE_STORAGE_KEY`).
pub const RENDER_TABLE_KEY: &str = "t3code.renderTable";

/// `filePreviewDelimiter` for a name alone: CSV and TSV files.
pub fn delimiter(name: &str) -> Option<char> {
    let lower = name.to_lowercase();
    if lower.ends_with(".csv") {
        Some(',')
    } else if lower.ends_with(".tsv") {
        Some('\t')
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub rows: Vec<Vec<String>>,
    /// More than the table holds: rows, columns or a cell cut short.
    pub truncated: bool,
}

/// `parseDelimitedPreview`: quoted delimiters, escaped quotes and multiline cells are kept; the table is bounded
/// to 100 rows, 30 columns and 2000 characters a cell, and the source view keeps every byte.
pub fn parse_table(text: &str, delimiter: char) -> Table {
    let chars: Vec<char> = text.chars().collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut cell_length = 0usize;
    let mut quoted = false;
    let mut truncated = false;
    let mut row_start = usize::from(chars.first() == Some(&'\u{feff}'));
    let mut index = row_start;
    // The original counts UTF-16 units; characters are the same for what people put in a cell.
    macro_rules! end_cell {
        () => {{
            if row.len() < MAX_COLUMNS {
                row.push(std::mem::take(&mut cell));
            } else {
                truncated = true;
                cell.clear();
            }
            cell_length = 0;
        }};
    }
    while index < chars.len() {
        let character = chars[index];
        if character == '"' {
            if quoted && chars.get(index + 1) == Some(&'"') {
                if cell_length < MAX_CELL {
                    cell.push('"');
                    cell_length += 1;
                } else {
                    truncated = true;
                }
                index += 2;
                continue;
            }
            if quoted || cell.is_empty() {
                quoted = !quoted;
                index += 1;
                continue;
            }
        }
        if !quoted && (character == delimiter || character == '\n' || character == '\r') {
            end_cell!();
            if character != delimiter {
                rows.push(std::mem::take(&mut row));
                if character == '\r' && chars.get(index + 1) == Some(&'\n') {
                    index += 1;
                }
                row_start = index + 1;
                if rows.len() == MAX_ROWS {
                    return Table { rows, truncated: truncated || index + 1 < chars.len() };
                }
            }
        } else if cell_length < MAX_CELL {
            cell.push(character);
            cell_length += 1;
        } else {
            truncated = true;
        }
        index += 1;
    }
    if row_start < chars.len() {
        end_cell!();
        rows.push(row);
    }
    Table { rows, truncated: truncated || quoted }
}

pub const TABLE_NOTICE: &str = "Table limited to the first 100 rows and 30 columns. Switch to source for the rest.";

fn extension(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.rfind('.').map(|at| name[at + 1..].to_lowercase())
}
/// `isWorkspaceVideoPreviewPath`.
pub fn is_video_path(path: &str) -> bool {
    matches!(extension(path).as_deref(), Some("avi" | "m4v" | "mkv" | "mov" | "mp4" | "ogv" | "webm"))
}
/// `isWorkspaceAudioPreviewPath`.
pub fn is_audio_path(path: &str) -> bool {
    matches!(extension(path).as_deref(), Some("mp3" | "wav" | "ogg" | "oga" | "flac" | "aac" | "m4a" | "opus" | "aiff"))
}
/// `isPdfPreviewFile`.
pub fn is_pdf_path(path: &str) -> bool {
    path.split(['?', '#']).next().is_some_and(|path| path.to_lowercase().ends_with(".pdf"))
}

/// A workspace file's place on the server's disk, the path a media URL is asked for (`absolutePath`).
pub fn absolute_path(cwd: &str, relative: &str) -> String {
    format!("{}/{}", cwd.trim_end_matches(['/', '\\']), relative)
}

/// The notice over a file the server read only the start of (`FilePreviewPanel.tsx`).
pub fn limit_notice(byte_length: u64) -> String {
    let digits = byte_length.to_string();
    let mut grouped = String::new();
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("Preview limited to the first 1 MB of a {grouped} byte file.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(table: &Table) -> Vec<Vec<&str>> {
        table.rows.iter().map(|row| row.iter().map(String::as_str).collect()).collect()
    }

    #[test]
    fn quoted_delimiters_escaped_quotes_and_multiline_cells_stay_in_one_cell() {
        let table = parse_table("name,note\n\"Doe, Jane\",\"said \"\"hi\"\"\nthen left\"\r\nx,y", ',');
        assert_eq!(cells(&table), vec![vec!["name", "note"], vec!["Doe, Jane", "said \"hi\"\nthen left"], vec!["x", "y"]]);
        assert!(!table.truncated);
    }

    #[test]
    fn a_byte_order_mark_and_the_final_newline_leave_no_cell_behind() {
        assert_eq!(cells(&parse_table("\u{feff}a\tb\n1\t2\n", '\t')), vec![vec!["a", "b"], vec!["1", "2"]]);
    }

    #[test]
    fn rows_columns_and_cells_are_bounded_and_say_so() {
        let wide = (0..40).map(|column| column.to_string()).collect::<Vec<_>>().join(",");
        let table = parse_table(&wide, ',');
        assert_eq!((table.rows[0].len(), table.truncated), (30, true));
        let long = format!("{}\n", "x".repeat(2500));
        let table = parse_table(&long, ',');
        assert_eq!((table.rows[0][0].len(), table.truncated), (2000, true));
        let many = (0..150).map(|row| row.to_string()).collect::<Vec<_>>().join("\n");
        let table = parse_table(&many, ',');
        assert_eq!((table.rows.len(), table.truncated), (100, true));
        let exact = (0..100).map(|row| row.to_string()).collect::<Vec<_>>().join("\n");
        assert_eq!(parse_table(&exact, ',').truncated, false, "a hundred rows and nothing after is not cut");
        assert!(parse_table("a,\"open", ',').truncated, "an unclosed quote is cut");
    }

    #[test]
    fn files_are_told_by_their_extension() {
        assert_eq!((delimiter("a/data.CSV"), delimiter("x.tsv"), delimiter("x.txt")), (Some(','), Some('\t'), None));
        assert!(is_video_path("clips/a.MP4") && is_video_path("b.mkv") && !is_video_path("c.mp3"));
        assert!(is_audio_path("a.flac") && is_audio_path("dir.d/a.m4a") && !is_audio_path("a.mp4") && !is_audio_path("flac"));
        assert!(is_pdf_path("docs/a.PDF") && is_pdf_path("a.pdf#page=2") && !is_pdf_path("a.pdf.txt"));
        assert_eq!(absolute_path("/work/app/", "docs/a.pdf"), "/work/app/docs/a.pdf");
    }

    #[test]
    fn the_limit_notice_groups_the_size_as_the_original_does() {
        assert_eq!(limit_notice(5_242_880), "Preview limited to the first 1 MB of a 5,242,880 byte file.");
        assert_eq!(limit_notice(1_048_577), "Preview limited to the first 1 MB of a 1,048,577 byte file.");
    }
}
