//! ACP agent registry.
//!
//! `Registry` manages all running ACP agents and merges their incoming message
//! streams into a single channel that the application can poll in its event loop.

use crate::{client::AcpEvent, state::AcpState, AgentConfig, Client, AgentId, Result};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use futures_util::StreamExt;
use std::collections::HashMap;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

/// Registry of all running ACP agents.
///
/// # Incoming messages
///
/// All agent-initiated calls and notifications are forwarded to
/// `Registry::incoming`.  Poll it in `tokio::select!` to handle messages
/// from any agent:
///
/// ```rust,ignore
/// while let Some((agent_id, event)) = registry.incoming.recv().await {
///     // dispatch event…
/// }
/// ```
pub struct Registry {
    clients: HashMap<AgentId, Client>,
    states: HashMap<AgentId, AcpState>,
    next_id: u64,
    /// Sender half of the shared incoming channel.  Each per-agent forwarder
    /// task clones this and writes messages from its agent.
    incoming_tx: UnboundedSender<(AgentId, AcpEvent)>,
    /// Unified stream of all incoming agent events.
    pub incoming: UnboundedReceiver<(AgentId, AcpEvent)>,
    /// The AgentId currently shown in the agent panel, if any.
    pub active_agent_id: Option<AgentId>,
}

impl Registry {
    pub fn new() -> Self {
        let (incoming_tx, incoming) = unbounded_channel();
        Registry {
            clients: HashMap::new(),
            states: HashMap::new(),
            next_id: 0,
            incoming_tx,
            incoming,
            active_agent_id: None,
        }
    }

    fn next_agent_id(&mut self) -> AgentId {
        let raw = self.next_id;
        self.next_id += 1;
        AgentId(raw)
    }

    /// Spawn an agent process and register it.
    ///
    /// Returns the new `AgentId`.  The agent is *not* yet initialized — call
    /// `client.initialize()` and `client.authenticate()` before use.
    pub fn start_agent(&mut self, config: &AgentConfig) -> Result<AgentId> {
        let id = self.next_agent_id();
        let (client, rx) = Client::start(config, id)?;

        // Spawn a lightweight forwarder task that writes all messages from
        // this agent's rx stream into the shared incoming channel.
        let shared_tx = self.incoming_tx.clone();
        tokio::spawn(async move {
            let mut stream = UnboundedReceiverStream::new(rx);
            while let Some(msg) = stream.next().await {
                if shared_tx.send(msg).is_err() {
                    // Registry was dropped — stop forwarding.
                    break;
                }
            }
        });

        let state = AcpState::new(Arc::new(AtomicBool::new(false)));
        self.clients.insert(id, client);
        self.states.insert(id, state);
        Ok(id)
    }

    /// Stop an agent and remove it from the registry.
    pub fn stop_agent(&mut self, id: AgentId) {
        self.clients.remove(&id);
        self.states.remove(&id);
    }

    // ---- Client accessors ----

    pub fn get(&self, id: AgentId) -> Option<&Client> {
        self.clients.get(&id)
    }

    pub fn get_mut(&mut self, id: AgentId) -> Option<&mut Client> {
        self.clients.get_mut(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (AgentId, &Client)> {
        self.clients.iter().map(|(&id, c)| (id, c))
    }

    // ---- AcpState accessors ----

    pub fn state(&self, id: AgentId) -> Option<&AcpState> {
        self.states.get(&id)
    }

    pub fn state_mut(&mut self, id: AgentId) -> Option<&mut AcpState> {
        self.states.get_mut(&id)
    }

    pub fn iter_states(&self) -> impl Iterator<Item = (AgentId, &AcpState)> {
        self.states.iter().map(|(&id, s)| (id, s))
    }

    /// Borrow both the Client and its AcpState simultaneously.
    pub fn client_and_state_mut(
        &mut self,
        id: AgentId,
    ) -> Option<(&mut Client, &mut AcpState)> {
        match (self.clients.get_mut(&id), self.states.get_mut(&id)) {
            (Some(c), Some(s)) => Some((c, s)),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
