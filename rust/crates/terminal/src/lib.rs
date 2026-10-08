//! Rust terminal policy and canvas surface over the application's pinned
//! upstream libghostty-vt engine. Native clients load this Rust WASM surface
//! in their WebView; the host keeps session/RPC ownership in native Rust.
#[cfg(any(target_arch = "wasm32", test))]
mod callbacks;
#[cfg(target_arch = "wasm32")]
pub mod core;
pub mod fonts;
pub mod model;
pub mod renderer;
#[cfg(target_arch = "wasm32")]
mod runtime;
pub const GHOSTTY_REVISION: &str = "9f62873bf195e4d8a762d768a1405a5f2f7b1697";

#[cfg(target_arch = "wasm32")]
mod input;
pub mod keyboard;

#[cfg(target_arch = "wasm32")]
pub mod surface;

pub mod surface_policy;

#[cfg(target_arch = "wasm32")]
pub mod appearance;

#[cfg(target_arch = "wasm32")]
pub mod inspector;
