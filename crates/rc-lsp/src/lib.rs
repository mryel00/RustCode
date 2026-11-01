//! Language Server Protocol support for RustCode.
//!
//! - [`transport`]: base-protocol framing
//! - [`client`]: one server process, requests/notifications over stdio
//! - [`manager`]: starts servers per language and keeps documents in sync
//! - [`watched_files`]: `workspace/didChangeWatchedFiles` registrations

pub mod client;
pub mod config;
pub mod manager;
pub mod transport;
pub mod watched_files;

pub use client::{LspClient, LspError};
pub use config::{default_servers, ServerConfig};
pub use manager::{LspManager, ManagerEvent, ServerStatus};
