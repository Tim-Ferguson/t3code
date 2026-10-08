//! Original settings terminal preview local echo policy (UTF-16 line cursor).
#[derive(Default)]
pub struct Echo {
    line_length: usize,
}
impl Echo {
    pub fn write(&mut self, data: &str, prompt: &str) -> Option<String> {
        if data == "\r" {
            self.line_length = 0;
            return Some(format!("\r\n{prompt}"));
        }
        if data == "\u{7f}" || data == "\u{8}" {
            if self.line_length > 0 {
                self.line_length -= 1;
                return Some("\u{8} \u{8}".into());
            }
            return None;
        }
        if data.starts_with('\u{1b}') {
            return None;
        }
        let printable: String = data
            .chars()
            .filter(|&ch| ch >= ' ' && ch != '\u{7f}')
            .collect();
        if printable.is_empty() {
            return None;
        }
        self.line_length += printable.encode_utf16().count();
        Some(printable)
    }
}
