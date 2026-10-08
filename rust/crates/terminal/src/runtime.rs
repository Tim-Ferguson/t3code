//! Memory views are always recreated after ABI calls: Ghostty may grow its
//! separate WASM memory. No upstream application JavaScript is executed.
use futures_util::FutureExt;
use js_sys::{Array, Function, Object, Reflect, Uint8Array, WebAssembly};
use serde::Deserialize;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;

pub type Result<T> = std::result::Result<T, JsValue>;
pub fn error(message: impl AsRef<str>) -> JsValue {
    js_sys::Error::new(message.as_ref()).into()
}
#[derive(Debug, Clone, Deserialize)]
pub struct Field {
    pub offset: u32,
    pub size: u32,
    #[serde(rename = "type")]
    pub kind: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Layout {
    pub size: u32,
    pub fields: BTreeMap<String, Field>,
}
use crate::callbacks::{Writer, Writers};
pub struct Runtime {
    exports: Object,
    memory: WebAssembly::Memory,
    layouts: BTreeMap<String, Layout>,
    writers: Rc<Writers>,
    callback_index: u32,
    _log: Closure<dyn FnMut(u32, u32)>,
    _pty: Closure<dyn Fn(u32, u32, u32, u32)>,
}
#[derive(Clone, Copy)]
pub enum Arg {
    Number(u32),
    Signed(i32),
    Wide(u64),
}
impl From<u32> for Arg {
    fn from(value: u32) -> Self {
        Self::Number(value)
    }
}
impl From<i32> for Arg {
    fn from(value: i32) -> Self {
        Self::Signed(value)
    }
}
impl From<u64> for Arg {
    fn from(value: u64) -> Self {
        Self::Wide(value)
    }
}
impl Runtime {
    pub fn callback_index(&self) -> u32 {
        self.callback_index
    }
    pub async fn shared() -> Result<Rc<Self>> {
        type Loading = futures_util::future::Shared<
            futures_util::future::LocalBoxFuture<'static, Result<Rc<Runtime>>>,
        >;
        thread_local! {static LOADING:RefCell<Option<Loading>>=const{RefCell::new(None)};}
        let load = LOADING.with(|slot| {
            slot.borrow_mut()
                .get_or_insert_with(|| Self::load().boxed_local().shared())
                .clone()
        });
        let result = load.await;
        if result.is_err() {
            LOADING.with(|slot| {
                slot.borrow_mut().take();
            });
        }
        result
    }
    pub async fn load() -> Result<Rc<Self>> {
        let memory_slot: Rc<RefCell<Option<WebAssembly::Memory>>> = Rc::default();
        let log_memory = memory_slot.clone();
        let log = Closure::wrap(Box::new(move |pointer: u32, length: u32| {
            if let Some(memory) = log_memory.borrow().as_ref() {
                let bytes = Uint8Array::new(&memory.buffer())
                    .subarray(pointer, pointer.saturating_add(length))
                    .to_vec();
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    web_sys::console::debug_1(&JsValue::from_str(text));
                }
            }
        }) as Box<dyn FnMut(u32, u32)>);
        let imports = Object::new();
        let env = Object::new();
        Reflect::set(&env, &"log".into(), log.as_ref())?;
        Reflect::set(&imports, &"env".into(), &env)?;
        let loaded = JsFuture::from(WebAssembly::instantiate_buffer(
            include_bytes!("../vendor/ghostty-vt.wasm"),
            &imports,
        ))
        .await?;
        let instance: WebAssembly::Instance =
            Reflect::get(&loaded, &"instance".into())?.dyn_into()?;
        let exports = instance.exports();
        let memory: WebAssembly::Memory = Reflect::get(&exports, &"memory".into())?.dyn_into()?;
        *memory_slot.borrow_mut() = Some(memory.clone());
        let type_json: Function =
            Reflect::get(&exports, &"ghostty_type_json".into())?.dyn_into()?;
        let pointer = type_json
            .call0(&JsValue::UNDEFINED)?
            .as_f64()
            .ok_or_else(|| error("Missing Ghostty type JSON"))? as u32;
        let bytes = Uint8Array::new(&memory.buffer());
        let mut end = pointer;
        while end < bytes.length() && bytes.get_index(end) != 0 {
            end += 1;
        }
        let layouts = serde_json::from_slice(&bytes.subarray(pointer, end).to_vec())
            .map_err(|cause| error(format!("Invalid Ghostty ABI layouts: {cause}")))?;
        let writers: Rc<Writers> = Rc::default();
        let callback_writers = writers.clone();
        let callback_memory = memory.clone();
        let pty = Closure::wrap(Box::new(
            move |_terminal: u32, userdata: u32, pointer: u32, length: u32| {
                if length == 0 {
                    return;
                }
                let bytes = Uint8Array::new(&callback_memory.buffer())
                    .subarray(pointer, pointer.saturating_add(length))
                    .to_vec();
                callback_writers.deliver(userdata, crate::input::decode(&bytes));
            },
        ) as Box<dyn Fn(u32, u32, u32, u32)>);
        let imports = Object::new();
        let env = Object::new();
        Reflect::set(&env, &"t3_write_pty".into(), pty.as_ref())?;
        Reflect::set(&imports, &"env".into(), &env)?;
        let loaded = JsFuture::from(WebAssembly::instantiate_buffer(
            include_bytes!("../vendor/ghostty-write-pty.wasm"),
            &imports,
        ))
        .await?;
        let trampoline: WebAssembly::Instance =
            Reflect::get(&loaded, &"instance".into())?.dyn_into()?;
        let callback: Function =
            Reflect::get(&trampoline.exports(), &"ghostty_write_pty".into())?.dyn_into()?;
        let table: WebAssembly::Table =
            Reflect::get(&exports, &"__indirect_function_table".into())?.dyn_into()?;
        let callback_index = table.length();
        // Separate grow/set preserves callback function type under WebKit.
        table.grow(1)?;
        table.set(callback_index, &callback)?;
        Ok(Rc::new(Self {
            exports,
            memory,
            layouts,
            writers,
            callback_index,
            _log: log,
            _pty: pty,
        }))
    }
    pub fn call(&self, name: &str, args: &[Arg]) -> Result<i32> {
        let function: Function = Reflect::get(&self.exports, &name.into())?
            .dyn_into()
            .map_err(|_| error(format!("Ghostty export unavailable: {name}")))?;
        let values = Array::new();
        for arg in args {
            values.push(&match arg {
                Arg::Number(value) => JsValue::from(*value),
                Arg::Signed(value) => JsValue::from(*value),
                Arg::Wide(value) => js_sys::BigInt::from(*value).into(),
            });
        }
        let result = function.apply(&JsValue::UNDEFINED, &values)?;
        Ok(result.as_f64().unwrap_or(0.0) as i32)
    }
    pub fn success(&self, name: &str, args: &[Arg]) -> Result<()> {
        let code = self.call(name, args)?;
        if code == 0 {
            Ok(())
        } else {
            Err(error(format!("{name} failed with {code}")))
        }
    }
    pub fn layout(&self, name: &str) -> Result<&Layout> {
        self.layouts
            .get(name)
            .ok_or_else(|| error(format!("Ghostty layout unavailable: {name}")))
    }
    pub fn field(&self, name: &str, field: &str) -> Result<&Field> {
        self.layout(name)?
            .fields
            .get(field)
            .ok_or_else(|| error(format!("Ghostty field unavailable: {name}.{field}")))
    }
    pub fn bytes(&self, pointer: u32, size: u32) -> Vec<u8> {
        Uint8Array::new(&self.memory.buffer())
            .subarray(pointer, pointer.saturating_add(size))
            .to_vec()
    }
    pub fn write(&self, pointer: u32, bytes: &[u8]) {
        Uint8Array::new(&self.memory.buffer())
            .subarray(pointer, pointer + bytes.len() as u32)
            .copy_from(bytes);
    }
    pub fn zero(&self, pointer: u32, size: u32) {
        Uint8Array::new(&self.memory.buffer())
            .subarray(pointer, pointer + size)
            .fill(0, 0, size);
    }
    pub fn u32(&self, pointer: u32) -> u32 {
        u32::from_le_bytes(self.bytes(pointer, 4).try_into().unwrap())
    }
    pub fn u64(&self, pointer: u32) -> u64 {
        u64::from_le_bytes(self.bytes(pointer, 8).try_into().unwrap())
    }
    pub fn get_field(&self, pointer: u32, name: &str, field: &str) -> Result<u64> {
        let field = self.field(name, field)?;
        let bytes = self.bytes(pointer + field.offset, field.size);
        Ok(match field.kind.as_str() {
            "bool" | "u8" => bytes[0] as u64,
            "u16" => u16::from_le_bytes(bytes.try_into().unwrap()) as u64,
            "u32" | "enum" | "i32" => u32::from_le_bytes(bytes.try_into().unwrap()) as u64,
            "u64" => u64::from_le_bytes(bytes.try_into().unwrap()),
            _ => return Err(error("Unsupported scalar field")),
        })
    }
    pub fn set_field(&self, pointer: u32, name: &str, field: &str, value: u64) -> Result<()> {
        let field = self.field(name, field)?;
        let bytes = value.to_le_bytes();
        if !matches!(
            field.kind.as_str(),
            "bool" | "u8" | "u16" | "u32" | "i32" | "enum" | "u64"
        ) {
            return Err(error("Unsupported scalar field"));
        }
        self.write(pointer + field.offset, &bytes[..field.size as usize]);
        Ok(())
    }
    pub fn allocate(self: &Rc<Self>, size: u32) -> Result<Allocation> {
        let pointer = self.call("ghostty_wasm_alloc_u8_array", &[size.into()])? as u32;
        if pointer == 0 {
            return Err(error("Ghostty allocation failed"));
        }
        self.zero(pointer, size);
        Ok(Allocation {
            runtime: self.clone(),
            pointer,
            size,
        })
    }
    pub fn handle(self: &Rc<Self>, new: &'static str, free: &'static str) -> Result<Handle> {
        self.handle_with(new, free, &[])
    }
    pub fn handle_with(
        self: &Rc<Self>,
        new: &'static str,
        free: &'static str,
        extra: &[Arg],
    ) -> Result<Handle> {
        let slot = self.call("ghostty_wasm_alloc_opaque", &[])? as u32;
        if slot == 0 {
            return Err(error("Ghostty opaque allocation failed"));
        }
        self.write(slot, &0u32.to_le_bytes());
        let handle = Handle {
            runtime: self.clone(),
            slot,
            free,
        };
        let mut args = vec![0u32.into(), slot.into()];
        args.extend_from_slice(extra);
        self.success(new, &args)?;
        Ok(handle)
    }
    pub fn attach_writer(&self, terminal: u32, writer: Writer) -> Result<u32> {
        self.writers
            .attach(writer, self.callback_index, |option, value| {
                self.success(
                    "ghostty_terminal_set",
                    &[terminal.into(), option.into(), value.into()],
                )
            })
    }

    pub fn detach_writer(&self, terminal: u32, id: u32) {
        let _ = self.call(
            "ghostty_terminal_set",
            &[terminal.into(), 1u32.into(), 0u32.into()],
        );
        let _ = self.call(
            "ghostty_terminal_set",
            &[terminal.into(), 0u32.into(), 0u32.into()],
        );
        self.writers.remove(id);
    }
}
pub struct Allocation {
    pub runtime: Rc<Runtime>,
    pub pointer: u32,
    pub size: u32,
}
impl Drop for Allocation {
    fn drop(&mut self) {
        let _ = self.runtime.call(
            "ghostty_wasm_free_u8_array",
            &[self.pointer.into(), self.size.into()],
        );
    }
}
pub struct Handle {
    pub runtime: Rc<Runtime>,
    pub slot: u32,
    pub free: &'static str,
}
impl Handle {
    pub fn value(&self) -> u32 {
        self.runtime.u32(self.slot)
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        let value = self.value();
        if value != 0 {
            let _ = self.runtime.call(self.free, &[value.into()]);
        }
        let _ = self
            .runtime
            .call("ghostty_wasm_free_opaque", &[self.slot.into()]);
    }
}
