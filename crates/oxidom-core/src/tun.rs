//! Profile network-interface primitives.
//!
//! This layer knows nothing about Xray or SOCKS as an application-level
//! concept: its process adapter receives a proxy address from the caller. That
//! boundary lets a future WireGuard backend reuse device and routing policy.

pub mod caps;
pub mod core;
pub mod device;
pub mod net;
pub mod plan;
pub mod resolve;
