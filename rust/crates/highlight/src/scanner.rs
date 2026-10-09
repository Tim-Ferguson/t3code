//! The TextMate engine uses UTF-16 offsets on both targets. Upstream Oniguruma
//! consumes UTF-8 (including JavaScript's preserved lone-surrogate encoding).
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    units: Vec<u16>,
    bytes: Vec<u8>,
    unit_to_byte: Vec<usize>,
    byte_to_unit: Vec<usize>,
}
impl Input {
    pub fn from_str(text: &str) -> Self {
        Self::from_units(text.encode_utf16().collect())
    }
    /// Match Shiki's UtfString: paired surrogates share the scalar's start,
    /// unmatched surrogates retain three UTF-8 bytes rather than U+FFFD.
    pub fn from_units(units: Vec<u16>) -> Self {
        let mut bytes = Vec::with_capacity(units.len());
        let mut unit_to_byte = vec![0; units.len() + 1];
        let mut byte_to_unit = Vec::with_capacity(units.len() + 1);
        let mut index = 0;
        while index < units.len() {
            let first = units[index];
            let pair = (0xD800..=0xDBFF).contains(&first)
                && units.get(index + 1).is_some_and(|unit| (0xDC00..=0xDFFF).contains(unit));
            let code = if pair {
                0x10000 + ((u32::from(first) - 0xD800) << 10)
                    + u32::from(units[index + 1]) - 0xDC00
            } else {
                u32::from(first)
            };
            unit_to_byte[index] = bytes.len();
            if pair {
                unit_to_byte[index + 1] = bytes.len();
            }
            let start = bytes.len();
            match code {
                0..=0x7F => bytes.push(code as u8),
                0x80..=0x7FF => {
                    bytes.push(0xC0 | (code >> 6) as u8);
                    bytes.push(0x80 | (code & 0x3F) as u8);
                }
                0x800..=0xFFFF => {
                    bytes.push(0xE0 | (code >> 12) as u8);
                    bytes.push(0x80 | ((code >> 6) & 0x3F) as u8);
                    bytes.push(0x80 | (code & 0x3F) as u8);
                }
                _ => {
                    bytes.push(0xF0 | (code >> 18) as u8);
                    bytes.push(0x80 | ((code >> 12) & 0x3F) as u8);
                    bytes.push(0x80 | ((code >> 6) & 0x3F) as u8);
                    bytes.push(0x80 | (code & 0x3F) as u8);
                }
            }
            byte_to_unit.extend(std::iter::repeat_n(index, bytes.len() - start));
            index += if pair { 2 } else { 1 };
        }
        unit_to_byte[units.len()] = bytes.len();
        byte_to_unit.push(units.len());
        Self { units, bytes, unit_to_byte, byte_to_unit }
    }
    pub fn units(&self) -> &[u16] { &self.units }
    pub fn bytes(&self) -> &[u8] { &self.bytes }
    pub fn len(&self) -> usize { self.units.len() }
    pub fn is_empty(&self) -> bool { self.units.is_empty() }
    pub fn byte_offset(&self, offset: usize) -> usize {
        if self.bytes.len() == self.units.len() { offset } else {
            self.unit_to_byte[offset.min(self.units.len())]
        }
    }
    pub fn unit_offset(&self, offset: usize) -> usize {
        if self.bytes.len() == self.units.len() { offset } else {
            self.byte_to_unit[offset.min(self.bytes.len())]
        }
    }
    pub fn unit_range(&self, bytes: Range<usize>) -> Range<usize> {
        self.unit_offset(bytes.start)..self.unit_offset(bytes.end)
    }
    /// Reproduce the defined JavaScript offset mapping followed by the WASM
    /// i32 argument conversion, while never constructing an out-of-range pointer.
    pub fn search_byte_offset(&self, start: u64) -> Option<usize> {
        if start > 9_007_199_254_740_991 { return None; }
        let byte = if self.bytes.len() == self.units.len() {
            let signed = start as u32 as i32;
            if signed < 0 { return None; }
            signed as usize
        } else {
            self.byte_offset(start.min(self.units.len() as u64) as usize)
        };
        (byte <= self.bytes.len()).then_some(byte)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FindOptions(pub u32);
impl FindOptions {
    pub const NONE: Self = Self(0);
    pub const NOT_BEGIN_STRING: Self = Self(1);
    pub const NOT_END_STRING: Self = Self(2);
    pub const NOT_BEGIN_POSITION: Self = Self(4);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub pattern: usize,
    /// UTF-16 ranges. Unmatched optional captures follow the original binding:
    /// unsigned -1 stays u32::MAX for ASCII inputs, and clamps to the
    /// input's end when a UTF-8/UTF-16 map exists.
    pub captures: Vec<Range<usize>>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);
impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for Error {}

pub trait Scanner {
    fn find(&mut self, text: &Input, start: u64, options: FindOptions)
        -> Result<Option<Match>, Error>;
}
pub trait Engine {
    type Scanner: Scanner;
    fn compile(&mut self, patterns: &[Input]) -> Result<Self::Scanner, Error>;
}
