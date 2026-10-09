//! TextMate's regex source transformations, independent of the regex backend.
use crate::scanner::Input;
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    units: Vec<u16>,
    pub has_anchor: bool,
    pub has_backreferences: bool,
}
impl Source {
    pub fn new(pattern: &Input) -> Self {
        let input = pattern.units();
        let mut units = Vec::with_capacity(input.len());
        let mut has_anchor = false;
        let mut index = 0;
        while index < input.len() {
            if input[index] == b'\\' as u16 && index + 1 < input.len() {
                match input[index + 1] {
                    0x7A => units.extend("$(?!\\n)(?<!\\n)".encode_utf16()),
                    unit => {
                        has_anchor |= unit == b'A' as u16 || unit == b'G' as u16;
                        units.extend_from_slice(&input[index..index + 2]);
                    }
                }
                index += 2;
            } else {
                units.push(input[index]);
                index += 1;
            }
        }
        let has_backreferences = units.windows(2).any(|pair| {
            pair[0] == b'\\' as u16 && ascii_digit(pair[1])
        });
        Self { units, has_anchor, has_backreferences }
    }
    pub fn units(&self) -> &[u16] { &self.units }
    /// Like upstream setSource, replacements retain the original anchor and
    /// backreference flags. Captured end patterns contain escaped literal data.
    pub fn set_source(&mut self, units: Vec<u16>) { self.units = units; }
    pub fn anchors(&self, allow_begin: bool, allow_position: bool) -> Input {
        let mut units = self.units.clone();
        if self.has_anchor {
            let mut index = 0;
            while index + 1 < units.len() {
                if units[index] == b'\\' as u16 {
                    if (units[index + 1] == b'A' as u16 && !allow_begin)
                        || (units[index + 1] == b'G' as u16 && !allow_position) {
                        // Retain the backslash. The source disables anchors with
                        // \\U+FFFF, not by dropping/rewriting the entire pattern.
                        units[index + 1] = 0xFFFF;
                    }
                    index += 2;
                } else { index += 1; }
            }
        }
        Input::from_units(units)
    }
    pub fn backreferences(&self, line: &Input, captures: &[Range<usize>]) -> Input {
        let mut output = Vec::with_capacity(self.units.len());
        let mut offset = 0;
        while offset < self.units.len() {
            if self.units[offset] == b'\\' as u16
                && self.units.get(offset + 1).is_some_and(|unit| ascii_digit(*unit)) {
                let mut end = offset + 1;
                let mut capture_index = Some(0usize);
                while end < self.units.len() && ascii_digit(self.units[end]) {
                    capture_index = capture_index.and_then(|index| index.checked_mul(10))
                        .and_then(|index| index.checked_add(usize::from(self.units[end] - b'0' as u16)));
                    end += 1;
                }
                if let Some(capture) = capture_index.and_then(|index| captures.get(index)) {
                    let start = capture.start.min(line.len());
                    let end = capture.end.min(line.len());
                    for unit in &line.units()[start.min(end)..start.max(end)] {
                        if special(*unit) { output.push(b'\\' as u16); }
                        output.push(*unit);
                    }
                }
                offset = end;
            } else {
                output.push(self.units[offset]);offset += 1;
            }
        }
        Input::from_units(output)
    }
}
fn ascii_digit(unit: u16) -> bool { (b'0' as u16..=b'9' as u16).contains(&unit) }
fn special(unit: u16) -> bool {
    if unit < 128 && b"-\\{}*+?|^$.,[]()#".contains(&(unit as u8)) { return true; }
    // ECMAScript \s, including BOM and excluding NEL.
    matches!(unit, 0x09..=0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A
        | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF)
}
