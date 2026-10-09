use crate::scanner::{Engine, Error, FindOptions, Input, Match, Scanner};
use std::{ffi::{c_char, c_int, c_void, CStr}, ptr::NonNull, sync::OnceLock};

unsafe extern "C" {
    fn t3_onig_initialize() -> c_int;
    fn t3_onig_compile(patterns: *const *const u8, lengths: *const usize, count: c_int,
        error: *mut c_char) -> *mut c_void;
    fn t3_onig_free(scanner: *mut c_void);
    fn t3_onig_find(scanner: *mut c_void, text: *const u8, length: usize, start: usize,
        options: u32, pattern: *mut c_int, count: *mut c_int, beg: *mut *const c_int,
        end: *mut *const c_int, error: *mut c_char) -> c_int;
}

#[derive(Default,Clone,Copy)]
pub struct NativeEngine;
pub struct NativeScanner(NonNull<c_void>);
impl Drop for NativeScanner {
    fn drop(&mut self) { unsafe { t3_onig_free(self.0.as_ptr()) } }
}
fn message(buffer: &[c_char; 256]) -> Error {
    // The C bridge writes a NUL-terminated upstream error (maximum 90 bytes).
    Error(unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned())
}
impl Engine for NativeEngine {
    type Scanner = NativeScanner;
    fn compile(&mut self, patterns: &[Input]) -> Result<Self::Scanner, Error> {
        static INITIALIZED: OnceLock<c_int> = OnceLock::new();
        if *INITIALIZED.get_or_init(|| unsafe { t3_onig_initialize() }) != 0 {
            return Err(Error("Oniguruma initialization failed".into()));
        }
        let count = c_int::try_from(patterns.len()).map_err(|_| Error("too many patterns".into()))?;
        if patterns.iter().any(|p| p.bytes().len() > c_int::MAX as usize) {
            return Err(Error("pattern exceeds Oniguruma's position range".into()));
        }
        let pointers: Vec<_> = patterns.iter().map(|p| p.bytes().as_ptr()).collect();
        let lengths: Vec<_> = patterns.iter().map(|p| p.bytes().len()).collect();
        let mut error = [0; 256];
        // The bridge copies patterns while compiling. No Rust pointers escape.
        let scanner = unsafe { t3_onig_compile(pointers.as_ptr(), lengths.as_ptr(), count, error.as_mut_ptr()) };
        NonNull::new(scanner).map(NativeScanner).ok_or_else(|| message(&error))
    }
}
impl Scanner for NativeScanner {
    fn find(&mut self, text: &Input, start: u64, options: FindOptions) -> Result<Option<Match>, Error> {
        // Offset conversion deliberately preserves unmapped ASCII capture sentinels.
        // Search positions are separately validated before any FFI pointer arithmetic.
        let Some(start) = text.search_byte_offset(start) else { return Ok(None); };
        if text.bytes().len() > c_int::MAX as usize {
            return Err(Error("input exceeds Oniguruma's position range".into()));
        }
        let mut error = [0; 256];
        let (mut pattern, mut count) = (0, 0);
        let (mut beg, mut end) = (std::ptr::null(), std::ptr::null());
        let status = unsafe { t3_onig_find(self.0.as_ptr(), text.bytes().as_ptr(), text.bytes().len(),
            start, options.0, &mut pattern, &mut count, &mut beg, &mut end, error.as_mut_ptr()) };
        match status {
            0 => Ok(None),
            1 => {
                let count = usize::try_from(count).map_err(|_| Error("invalid capture count".into()))?;
                let starts = unsafe { std::slice::from_raw_parts(beg, count) };
                let ends = unsafe { std::slice::from_raw_parts(end, count) };
                let captures = starts.iter().zip(ends).map(|(&a, &b)| {
                    // JavaScript reads the upstream signed offsets through Uint32Array.
                    text.unit_offset(a as u32 as usize)..text.unit_offset(b as u32 as usize)
                }).collect();
                Ok(Some(Match { pattern: pattern as usize, captures }))
            }
            _ => Err(message(&error)),
        }
    }
}
