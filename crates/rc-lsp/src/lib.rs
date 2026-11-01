//! Language Server Protocol support for RustCode.
//!
//! - [`transport`]: base-protocol framing
//! - [`client`]: one server process, requests/notifications over stdio
//! - [`manager`]: starts servers per language and keeps documents in sync

pub mod client;
pub mod config;
pub mod manager;
pub mod transport;

pub use client::{LspClient, LspError};
pub use config::{default_servers, ServerConfig};
pub use manager::{LspManager, ManagerEvent, ServerStatus};
