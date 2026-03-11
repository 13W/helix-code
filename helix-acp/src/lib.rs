//! `helix-acp` — Agent Client Protocol (ACP) client for the Helix editor.
//!
//! ACP enables bidirectional communication between the editor (client) and
//! autonomous AI agents (subprocesses).  The protocol is JSON-RPC 2.0 over
//! newline-delimited stdio.
//!
//! # Usage
//!
//! ```rust,ignore
//! use helix_acp::{Registry, client::AgentConfig};
//!
//! let mut registry = Registry::new();
//! let config = AgentConfig::new("my-agent");
//! let id = registry.start_agent(&config).unwrap();
//!
//! let client = registry.get_mut(id).unwrap();
//! client.initialize().await.unwrap();
//! client.session_new().await.unwrap();
//! let _stop = client.prompt_text("Hello, agent!").await.unwrap();
//! ```

pub mod client;
pub mod registry;
pub mod types;

/// Re-export the official ACP SDK so downstream phases can reach SDK types
/// via `helix_acp::sdk::*` without needing a direct dep on `agent-client-protocol`.
pub use agent_client_protocol as sdk;

pub use client::{AcpEvent, Client, ClientHandle, DisplayLine, ReplyChannel};
pub use registry::Registry;
pub use types::*;

/// Opaque identifier for a running ACP agent.
///
/// Constructed only by [`Registry`]; not directly constructable by users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentId(pub(crate) u64);

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "agent#{}", self.0)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    ParseError(String),

    #[error("stream closed")]
    StreamClosed,

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::ParseError(e.to_string())
    }
}
