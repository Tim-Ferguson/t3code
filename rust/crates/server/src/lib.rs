//! Native backend services. Services own behavior; transports only decode and dispatch.
pub mod auth;
pub mod persistence;
pub mod project;
pub mod thread;
pub mod transport;
