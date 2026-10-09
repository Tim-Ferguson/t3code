//! Shared Rust TextMate policy with target-specific upstream Oniguruma scanners.
//! No Shiki/vscode-textmate JavaScript is used by this production engine.
pub mod scanner;
pub mod regex;
pub mod grammar;
pub mod catalog;
pub mod selector;
pub mod scope;
pub mod tokenizer;
pub mod theme;
pub mod streaming;
#[cfg(not(target_arch = "wasm32"))]
pub mod native;
#[cfg(target_arch = "wasm32")]
pub mod browser;
