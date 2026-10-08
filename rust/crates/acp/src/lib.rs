//! ACP v1/v2 wire codecs and a provider-neutral bidirectional client.
//! Child-process ownership belongs to the injected [`Peer`], not this library.
pub mod agent;
pub mod client;
pub mod errors;
pub mod extensions;
pub use agent::Agent;
pub use extensions::PayloadCodec;
mod normalize;
pub mod notifications;
pub use notifications::{IncomingNotification, NotificationStream};
pub mod protocol;
pub mod schema;
pub mod types;
pub mod transport;
pub mod v1;
pub mod v2;
pub use client::{
    AgentMethod, Client, ClientEvent, ClientOptions, Generation, Notification, RequestContext,
};
pub use protocol::*;
