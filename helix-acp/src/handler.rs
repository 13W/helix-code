//! `HelixClientHandler` — implements `sdk::Client` for agent-to-client callbacks
//! (session notifications, permission requests, file I/O).
//!
//! Also contains the small type-conversion helpers that translate between
//! `agent_client_protocol` types and `helix_acp_types`.

use helix_acp_types::*;
use agent_client_protocol as sdk;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::client::{AcpEvent, ReplyChannel};

// ---------------------------------------------------------------------------

pub(crate) struct HelixClientHandler {
    pub(crate) agent_id: AgentId,
    pub(crate) event_tx: UnboundedSender<(AgentId, AcpEvent)>,
}

#[async_trait::async_trait(?Send)]
impl sdk::Client for HelixClientHandler {
    async fn session_notification(&self, args: sdk::SessionNotification) -> sdk::Result<()> {
        let _ = self
            .event_tx
            .send((self.agent_id, AcpEvent::SessionNotification(args)));
        Ok(())
    }

    async fn request_permission(
        &self,
        args: sdk::RequestPermissionRequest,
    ) -> sdk::Result<sdk::RequestPermissionResponse> {
        let (tx, rx) = oneshot::channel();
        let reply: ReplyChannel<sdk::RequestPermissionResponse> =
            Arc::new(Mutex::new(Some(tx)));
        let _ = self.event_tx.send((
            self.agent_id,
            AcpEvent::RequestPermission { params: args, reply },
        ));
        rx.await.map_err(|_| sdk::Error::internal_error())
    }

    async fn read_text_file(
        &self,
        args: sdk::ReadTextFileRequest,
    ) -> sdk::Result<sdk::ReadTextFileResponse> {
        let (tx, rx) = oneshot::channel();
        let reply: ReplyChannel<sdk::ReadTextFileResponse> =
            Arc::new(Mutex::new(Some(tx)));
        let _ = self.event_tx.send((
            self.agent_id,
            AcpEvent::ReadTextFile { params: args, reply },
        ));
        rx.await.map_err(|_| sdk::Error::internal_error())
    }

    async fn write_text_file(
        &self,
        args: sdk::WriteTextFileRequest,
    ) -> sdk::Result<sdk::WriteTextFileResponse> {
        let (tx, rx) = oneshot::channel();
        let reply: ReplyChannel<sdk::WriteTextFileResponse> =
            Arc::new(Mutex::new(Some(tx)));
        let _ = self.event_tx.send((
            self.agent_id,
            AcpEvent::WriteTextFile { params: args, reply },
        ));
        rx.await.map_err(|_| sdk::Error::internal_error())
    }
}

// ---------------------------------------------------------------------------
// Type conversion helpers
// ---------------------------------------------------------------------------

pub(crate) fn to_sdk_content_block(cb: ContentBlock) -> sdk::ContentBlock {
    match cb {
        ContentBlock::Text { text } => sdk::ContentBlock::Text(sdk::TextContent::new(text)),
        // Fallback for non-text blocks — these are not used in current prompts
        _ => sdk::ContentBlock::Text(sdk::TextContent::new("[unsupported content block]")),
    }
}

pub(crate) fn convert_stop_reason(r: sdk::StopReason) -> StopReason {
    match r {
        sdk::StopReason::EndTurn => StopReason::EndTurn,
        sdk::StopReason::MaxTokens => StopReason::MaxTokens,
        sdk::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
        sdk::StopReason::Refusal => StopReason::Refusal,
        sdk::StopReason::Cancelled => StopReason::Cancelled,
        _ => StopReason::EndTurn,
    }
}

pub(crate) fn convert_init_response(resp: sdk::InitializeResponse) -> InitializeResult {
    let caps = resp.agent_capabilities;
    InitializeResult {
        protocol_version: caps.load_session as u16, // placeholder, not used
        capabilities: AgentCapabilities {
            load_session: Some(caps.load_session),
            prompt_capabilities: Some(PromptCapabilities {
                audio: caps.prompt_capabilities.audio,
                image: caps.prompt_capabilities.image,
                embedded_context: caps.prompt_capabilities.embedded_context,
            }),
            mcp_capabilities: None,
            session_capabilities: None,
        },
        agent_info: resp.agent_info.map(|i| AgentInfo {
            name: i.name,
            title: i.title,
            version: Some(i.version),
        }),
        auth_methods: resp
            .auth_methods
            .into_iter()
            .map(|m| AuthMethod {
                id: m.id().to_string(),
                name: m.name().to_owned(),
                description: m.description().map(|s| s.to_owned()),
            })
            .collect(),
    }
}
