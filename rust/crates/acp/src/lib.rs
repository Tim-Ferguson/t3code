//! ACP v1/v2 wire codecs and a provider-neutral bidirectional client.
//! Child-process ownership belongs to the injected [`Peer`], not this library.
pub mod client;
mod normalize;
pub mod protocol;
pub mod schema;
pub mod types;
pub mod v1;
pub mod v2;
pub use client::{AgentMethod, Client, ClientEvent, Generation, Notification, RequestContext};
pub use protocol::*;
