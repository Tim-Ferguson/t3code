use crate::{
    keyboard::KeyInput,
    runtime::{Handle, Result, Runtime},
};
use serde::Deserialize;
use std::rc::Rc;
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MouseInput {
    pub action: String,
    pub button: Option<u32>,
    pub mods: u32,
    pub x: f64,
    pub y: f64,
    pub screen_width: f64,
    pub screen_height: f64,
    pub cell_width: f64,
    pub cell_height: f64,
    pub padding_left: f64,
    pub padding_right: f64,
    pub padding_top: f64,
    pub padding_bottom: f64,
    pub any_button_pressed: bool,
}
pub struct Input {
    runtime: Rc<Runtime>,
    terminal: u32,
    key_event: Handle,
    key_encoder: Handle,
    mouse_event: Handle,
    mouse_encoder: Handle,
}
impl Input {
    pub fn new(runtime: Rc<Runtime>, terminal: u32) -> Result<Self> {
        Ok(Self {
            key_event: runtime.handle("ghostty_key_event_new", "ghostty_key_event_free")?,
            key_encoder: runtime.handle("ghostty_key_encoder_new", "ghostty_key_encoder_free")?,
            mouse_event: runtime.handle("ghostty_mouse_event_new", "ghostty_mouse_event_free")?,
            mouse_encoder: runtime
                .handle("ghostty_mouse_encoder_new", "ghostty_mouse_encoder_free")?,
            runtime,
            terminal,
        })
    }
    fn output(&self, mut encode: impl FnMut(u32, u32, u32) -> Result<i32>) -> Result<String> {
        let written = self.runtime.allocate(4)?;
        let result = encode(0, 0, written.pointer)?;
        let size = self.runtime.u32(written.pointer);
        if result != -3 || size == 0 {
            return Ok(String::new());
        }
        let output = self.runtime.allocate(size)?;
        let result = encode(output.pointer, size, written.pointer)?;
        if result != 0 {
            return Ok(String::new());
        }
        Ok(decode(
            &self
                .runtime
                .bytes(output.pointer, self.runtime.u32(written.pointer)),
        ))
    }
    pub fn key(&self, event: &KeyInput) -> Result<String> {
        let rt = &self.runtime;
        let encoder = self.key_encoder.value();
        let key = self.key_event.value();
        rt.call(
            "ghostty_key_encoder_setopt_from_terminal",
            &[encoder.into(), self.terminal.into()],
        )?;
        for (name, value) in [
            (
                "ghostty_key_event_set_action",
                if event.release {
                    0
                } else if event.repeat {
                    2
                } else {
                    1
                },
            ),
            (
                "ghostty_key_event_set_key",
                crate::keyboard::key_for_code(&event.code),
            ),
            ("ghostty_key_event_set_mods", event.mods()),
            ("ghostty_key_event_set_consumed_mods", event.consumed_mods()),
            (
                "ghostty_key_event_set_composing",
                u32::from(event.is_composing),
            ),
            (
                "ghostty_key_event_set_unshifted_codepoint",
                event.unshifted(),
            ),
        ] {
            rt.call(name, &[key.into(), value.into()])?;
        }
        // Browser key.length uses UTF16, so astral input belongs to composition/input.
        let text = if event.key.encode_utf16().count() == 1 {
            event.key.as_bytes()
        } else {
            &[]
        };
        let storage = if text.is_empty() {
            None
        } else {
            let p = rt.allocate(text.len() as u32)?;
            rt.write(p.pointer, text);
            Some(p)
        };
        rt.call(
            "ghostty_key_event_set_utf8",
            &[
                key.into(),
                storage.as_ref().map_or(0, |p| p.pointer).into(),
                (text.len() as u32).into(),
            ],
        )?;
        self.output(|pointer, size, written| {
            rt.call(
                "ghostty_key_encoder_encode",
                &[
                    encoder.into(),
                    key.into(),
                    pointer.into(),
                    size.into(),
                    written.into(),
                ],
            )
        })
    }
    pub fn paste(&self, data: &str) -> Result<String> {
        if data.is_empty() {
            return Ok(String::new());
        }
        let rt = &self.runtime;
        let input = rt.allocate(data.len() as u32)?;
        rt.write(input.pointer, data.as_bytes());
        let mode = rt.allocate(1)?;
        let bracketed = rt.call(
            "ghostty_terminal_mode_get",
            &[self.terminal.into(), 2004u32.into(), mode.pointer.into()],
        )? == 0
            && rt.bytes(mode.pointer, 1)[0] != 0;
        self.output(|pointer, size, written| {
            rt.call(
                "ghostty_paste_encode",
                &[
                    input.pointer.into(),
                    (data.len() as u32).into(),
                    u32::from(bracketed).into(),
                    pointer.into(),
                    size.into(),
                    written.into(),
                ],
            )
        })
    }
    pub fn mouse(&self, input: &MouseInput) -> Result<String> {
        let rt = &self.runtime;
        let encoder = self.mouse_encoder.value();
        let event = self.mouse_event.value();
        rt.call(
            "ghostty_mouse_encoder_setopt_from_terminal",
            &[encoder.into(), self.terminal.into()],
        )?;
        let layout = rt.layout("GhosttyMouseEncoderSize")?;
        let size = rt.allocate(layout.size)?;
        rt.set_field(
            size.pointer,
            "GhosttyMouseEncoderSize",
            "size",
            layout.size as u64,
        )?;
        for (field, value) in [
            ("screen_width", input.screen_width),
            ("screen_height", input.screen_height),
            ("cell_width", input.cell_width),
            ("cell_height", input.cell_height),
            ("padding_top", input.padding_top),
            ("padding_bottom", input.padding_bottom),
            ("padding_right", input.padding_right),
            ("padding_left", input.padding_left),
        ] {
            rt.set_field(
                size.pointer,
                "GhosttyMouseEncoderSize",
                field,
                (value + 0.5).floor().max(0.0) as u64,
            )?;
        }
        rt.call(
            "ghostty_mouse_encoder_setopt",
            &[encoder.into(), 2u32.into(), size.pointer.into()],
        )?;
        let flag = rt.allocate(1)?;
        for (option, value) in [(3u32, u8::from(input.any_button_pressed)), (4, 1)] {
            rt.write(flag.pointer, &[value]);
            rt.call(
                "ghostty_mouse_encoder_setopt",
                &[encoder.into(), option.into(), flag.pointer.into()],
            )?;
        }
        let action = match input.action.as_str() {
            "press" => 0u32,
            "release" => 1,
            _ => 2,
        };
        rt.call(
            "ghostty_mouse_event_set_action",
            &[event.into(), action.into()],
        )?;
        if let Some(button) = input.button {
            rt.call(
                "ghostty_mouse_event_set_button",
                &[event.into(), button.into()],
            )?;
        } else {
            rt.call("ghostty_mouse_event_clear_button", &[event.into()])?;
        }
        rt.call(
            "ghostty_mouse_event_set_mods",
            &[event.into(), input.mods.into()],
        )?;
        let position = rt.allocate(rt.layout("GhosttyMousePosition")?.size)?;
        for (field, value) in [("x", input.x), ("y", input.y)] {
            let offset = rt.field("GhosttyMousePosition", field)?.offset;
            rt.write(position.pointer + offset, &(value as f32).to_le_bytes());
        }
        rt.call(
            "ghostty_mouse_event_set_position",
            &[event.into(), position.pointer.into()],
        )?;
        self.output(|pointer, size, written| {
            rt.call(
                "ghostty_mouse_encoder_encode",
                &[
                    encoder.into(),
                    event.into(),
                    pointer.into(),
                    size.into(),
                    written.into(),
                ],
            )
        })
    }
}
/// Match default TextDecoder, which suppresses a leading UTF8 BOM.
pub fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes)).into_owned()
}
