//! Only the upstream regex scanner executes in WASM. Grammar and theme policy
//! remain in Rust, shared with the native scanner.
use crate::scanner::{Engine, Error, FindOptions, Input, Match, Scanner};
use js_sys::{Array, Function, Object, Reflect, Uint8Array, Uint32Array, WebAssembly};
use std::{cell::{Cell, RefCell}, rc::Rc};
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

fn js_error(value: JsValue) -> Error {
    Error(value.as_string().unwrap_or_else(|| format!("Oniguruma WASM: {value:?}")))
}
fn property(object: &JsValue, key: &str) -> Result<JsValue, Error> {
    Reflect::get(object, &key.into()).map_err(js_error)
}
fn function(object: &JsValue, key: &str) -> Result<Function, Error> {
    property(object, key)?.dyn_into().map_err(js_error)
}
fn set(object: &Object, key: &str, value: &JsValue) -> Result<(), Error> {
    Reflect::set(object, &key.into(), value).map_err(js_error)?;
    Ok(())
}

struct Runtime {
    exports: Object,
    memory: WebAssembly::Memory,
    string_id: Cell<u32>,
    _copy: Closure<dyn Fn(u32, u32, u32)>,
    _resize: Closure<dyn Fn(u32) -> u32>,
    _clock: Closure<dyn Fn() -> f64>,
    _write: Closure<dyn Fn(u32, u32, u32, u32) -> u32>,
}
impl Runtime {
    fn call(&self, name: &str, args: &[u32]) -> Result<u32, Error> {
        let arguments = Array::new();
        for &argument in args { arguments.push(&JsValue::from_f64(f64::from(argument))); }
        let result = function(&self.exports, name)?.apply(&JsValue::UNDEFINED, &arguments).map_err(js_error)?;
        result.as_f64().filter(|value| value.is_finite() && *value >= 0.0 && *value <= u32::MAX as f64)
            .map(|value| value as u32).ok_or_else(|| Error(format!("Invalid {name} result")))
    }
    fn error(&self) -> Error {
        let result = (|| {
            let start = self.call("getLastOnigError", &[])?;
            let heap = Uint8Array::new(&self.memory.buffer());
            let mut end = start;
            while end < heap.length() && end - start < 1024 && heap.get_index(end) != 0 { end += 1; }
            Ok::<_, Error>(String::from_utf8_lossy(&heap.subarray(start, end).to_vec()).into_owned())
        })();
        Error(result.unwrap_or_else(|error| error.0))
    }
    fn alloc(self: &Rc<Self>, bytes: &[u8]) -> Result<Allocation, Error> {
        let length = u32::try_from(bytes.len()).map_err(|_| Error("scanner input too large".into()))?;
        let pointer = self.call("omalloc", &[length.max(1)])?;
        if pointer == 0 { return Err(Error("scanner allocation failed".into())); }
        let allocation = Allocation { runtime: self.clone(), pointer };
        let heap = Uint8Array::new(&self.memory.buffer());
        let end = pointer.checked_add(length).filter(|end| *end <= heap.length())
            .ok_or_else(|| Error("scanner allocation outside memory".into()))?;
        heap.subarray(pointer, end).copy_from(bytes);
        Ok(allocation)
    }
}
struct Allocation { runtime: Rc<Runtime>, pointer: u32 }
impl Drop for Allocation {
    fn drop(&mut self) { let _ = self.runtime.call("ofree", &[self.pointer]); }
}

#[derive(Clone)]
pub struct BrowserEngine(Rc<Runtime>);
impl BrowserEngine {
    pub async fn load() -> Result<Self, Error> {
        let slot: Rc<RefCell<Option<WebAssembly::Memory>>> = Rc::default();
        let copy_slot = slot.clone();
        let copy = Closure::wrap(Box::new(move |destination: u32, source: u32, count: u32| {
            if let Some(memory) = copy_slot.borrow().as_ref() {
                // Upstream's memcpy import has memmove semantics (copyWithin).
                let heap = Uint8Array::new(&memory.buffer());
                heap.copy_within(destination as i32, source as i32, source.wrapping_add(count) as i32);
            }
        }) as Box<dyn Fn(u32, u32, u32)>);
        let resize_slot = slot.clone();
        let resize = Closure::wrap(Box::new(move |requested: u32| -> u32 {
            let Some(memory) = resize_slot.borrow().clone() else { return 0; };
            let max = 2_147_483_648u64;
            if u64::from(requested) > max { return 0; }
            let old = Uint8Array::new(&memory.buffer()).length() as f64;
            for cut in [1.0, 2.0, 4.0] {
                let grown = (old * (1.0 + 0.2 / cut)).min(f64::from(requested) + 100_663_296.0);
                let size = ((grown.max(f64::from(requested)) / 65536.0).ceil() as u64 * 65536).min(max);
                let current = Uint8Array::new(&memory.buffer()).length() as u64;
                let pages = size.saturating_sub(current).div_ceil(65536) as u32;
                // The direct js-sys grow binding throws. Dynamic call catches
                // allocation failures and follows the upstream smaller retries.
                let result = function(memory.as_ref(), "grow").and_then(|grow| {
                    grow.call1(memory.as_ref(), &JsValue::from_f64(f64::from(pages))).map_err(js_error)
                });
                if result.is_ok() { return 1; }
            }
            0
        }) as Box<dyn Fn(u32) -> u32>);
        let clock = Closure::wrap(Box::new(js_sys::Date::now) as Box<dyn Fn() -> f64>);
        let write = Closure::wrap(Box::new(|_: u32, _: u32, _: u32, _: u32| 0u32) as Box<dyn Fn(u32, u32, u32, u32) -> u32>);
        let env = Object::new();
        set(&env, "emscripten_memcpy_big", copy.as_ref())?;
        set(&env, "_emscripten_memcpy_js", copy.as_ref())?;
        set(&env, "emscripten_resize_heap", resize.as_ref())?;
        set(&env, "emscripten_get_now", clock.as_ref())?;
        set(&env, "fd_write", write.as_ref())?;
        let imports = Object::new();
        set(&imports, "env", &env)?;
        set(&imports, "wasi_snapshot_preview1", &env)?;
        let result = JsFuture::from(WebAssembly::instantiate_buffer(include_bytes!("../assets/onig.wasm"), &imports)).await.map_err(js_error)?;
        let instance: WebAssembly::Instance = property(&result, "instance")?.dyn_into().map_err(js_error)?;
        let exports = instance.exports();
        let memory: WebAssembly::Memory = property(&exports, "memory")?.dyn_into().map_err(js_error)?;
        *slot.borrow_mut() = Some(memory.clone());
        // Emscripten initialization precedes every allocator/scanner call.
        function(&exports, "__wasm_call_ctors")?.call0(&JsValue::UNDEFINED).map_err(js_error)?;
        Ok(Self(Rc::new(Runtime { exports, memory, string_id: Cell::new(0), _copy: copy,
            _resize: resize, _clock: clock, _write: write })))
    }
}
pub struct BrowserScanner { runtime: Rc<Runtime>, pointer: u32 }
impl Drop for BrowserScanner {
    fn drop(&mut self) { let _ = self.runtime.call("freeOnigScanner", &[self.pointer]); }
}
impl Engine for BrowserEngine {
    type Scanner = BrowserScanner;
    fn compile(&mut self, patterns: &[Input]) -> Result<Self::Scanner, Error> {
        let strings = patterns.iter().map(|pattern| self.0.alloc(pattern.bytes())).collect::<Result<Vec<_>, _>>()?;
        let pointers: Vec<u8> = strings.iter().flat_map(|string| string.pointer.to_le_bytes()).collect();
        let lengths: Vec<u8> = patterns.iter().flat_map(|pattern| (pattern.bytes().len() as u32).to_le_bytes()).collect();
        let pointers = self.0.alloc(&pointers)?;
        let lengths = self.0.alloc(&lengths)?;
        let count = u32::try_from(patterns.len()).map_err(|_| Error("too many patterns".into()))?;
        let pointer = self.0.call("createOnigScanner", &[pointers.pointer, lengths.pointer, count])?;
        if pointer == 0 { return Err(self.0.error()); }
        Ok(BrowserScanner { runtime: self.0.clone(), pointer })
    }
}
impl Scanner for BrowserScanner {
    fn find(&mut self, text: &Input, start: u64, options: FindOptions) -> Result<Option<Match>, Error> {
        let Some(start) = text.search_byte_offset(start) else { return Ok(None); };
        let length = u32::try_from(text.bytes().len()).map_err(|_| Error("scanner input too large".into()))?;
        let string = self.runtime.alloc(text.bytes())?;
        let id = self.runtime.string_id.get().wrapping_add(1);
        self.runtime.string_id.set(id);
        let pointer = self.runtime.call("findNextOnigScannerMatch", &[self.pointer, id, string.pointer,
            length, start as u32, options.0])?;
        if pointer == 0 { return Ok(None); }
        let heap = Uint32Array::new(&self.runtime.memory.buffer());
        let offset = pointer / 4;
        if pointer % 4 != 0 || offset.checked_add(2).is_none_or(|end| end > heap.length()) {
            return Err(Error("invalid scanner capture header".into()));
        }
        let pattern = heap.get_index(offset) as usize;
        let count = heap.get_index(offset + 1);
        if count > 1000 || offset.checked_add(2 + count * 2).is_none_or(|end| end > heap.length()) {
            return Err(Error("invalid scanner captures".into()));
        }
        let captures = (0..count).map(|index| {
            text.unit_offset(heap.get_index(offset + 2 + index * 2) as usize)
                ..text.unit_offset(heap.get_index(offset + 3 + index * 2) as usize)
        }).collect();
        Ok(Some(Match { pattern, captures }))
    }
}
