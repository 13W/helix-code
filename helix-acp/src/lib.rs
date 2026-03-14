//! `helix-acp` — Agent Client Protocol (ACP) client for the Helix editor.
//!
//! ACP enables bidirectional communication between the editor (client) and
//! autonomous AI agents (subprocesses).  The protocol is JSON-RPC 2.0 over
//! newline-delimited stdio.
//!
//! # Usage
//!
//! ```rust,ignore
//! use helix_acp::{Registry, AgentConfig};
//!
//! let mut registry = Registry::new();
//! let config = AgentConfig::new("my-agent");
//! let id = registry.start_agent(&config).unwrap();
//!
//! let client = registry.get_mut(id).unwrap();
//! client.initialize().await.unwrap();
//! client.session_new(".", None).await.unwrap();
//! let _stop = client.prompt_text("Hello, agent!").await.unwrap();
//! ```

pub mod client;
pub mod registry;
pub mod state;
pub(crate) mod rpc;
pub(crate) mod handler;

/// Re-export the official ACP SDK so downstream phases can reach SDK types
/// via `helix_acp::sdk::*` without needing a direct dep on `agent-client-protocol`.
pub use agent_client_protocol as sdk;

// Re-export all pure types from the companion types crate.
pub use helix_acp_types::*;

pub use client::{AcpEvent, Client, ClientHandle, ReplyChannel};
pub use registry::Registry;
pub use state::{AcpState, ToolCallUpdate};
