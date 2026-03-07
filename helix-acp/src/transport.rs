//! ACP stdio transport layer.
//!
//! ACP uses newline-delimited JSON-RPC over stdin/stdout (no Content-Length headers).
//! Each message is a single UTF-8 JSON line terminated by `\n`.
//! Agents may write log output to stderr; clients should log it but otherwise ignore it.

use crate::{jsonrpc, AgentId, Error, Result};
use anyhow::Context;
use log::{error, info};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::{
        mpsc::{unbounded_channel, Sender, UnboundedReceiver, UnboundedSender},
        Mutex,
    },
};

/// A message payload the client wants to send to the agent.
#[derive(Debug)]
pub enum Payload {
    /// A request that expects a response.  `chan` receives the result.
    Request {
        chan: Sender<Result<Value>>,
        value: jsonrpc::MethodCall,
    },
    /// A one-way notification.
    Notification(jsonrpc::Notification),
    /// A response to an agent-initiated request.
    Response(jsonrpc::Output),
}

/// All message shapes an agent can send to the client.
#[derive(Debug, PartialEq, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum AgentMessage {
    Output(jsonrpc::Output),
    Call(jsonrpc::Call),
}

#[derive(Debug)]
pub struct Transport {
    id: AgentId,
    name: String,
    /// Pending outgoing requests, keyed by request ID.
    pending_requests: Mutex<HashMap<jsonrpc::Id, Sender<Result<Value>>>>,
}

impl Transport {
    /// Spawn the three I/O tasks (recv stdout, forward stderr, send stdin).
    ///
    /// Returns:
    /// - `rx`  — incoming calls/notifications from the agent
    /// - `tx`  — sender for outgoing payloads (requests, notifications, responses)
    pub fn start(
        server_stdout: BufReader<ChildStdout>,
        server_stdin: BufWriter<ChildStdin>,
        server_stderr: BufReader<ChildStderr>,
        id: AgentId,
        name: String,
    ) -> (
        UnboundedReceiver<(AgentId, jsonrpc::Call)>,
        UnboundedSender<Payload>,
    ) {
        let (client_tx, rx) = unbounded_channel();
        let (tx, client_rx) = unbounded_channel();

        let transport = Arc::new(Self {
            id,
            name,
            pending_requests: Mutex::new(HashMap::default()),
        });

        tokio::spawn(Self::recv(
            transport.clone(),
            server_stdout,
            client_tx.clone(),
        ));
        tokio::spawn(Self::err(transport.clone(), server_stderr));
        tokio::spawn(Self::send(transport, server_stdin, client_rx));

        (rx, tx)
    }

    // ------------------------------------------------------------------
    // Receive path
    // ------------------------------------------------------------------

    /// Read one newline-terminated JSON message from the agent's stdout.
    async fn recv_agent_message(
        reader: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut String,
        name: &str,
    ) -> Result<AgentMessage> {
        buffer.clear();
        let n = reader.read_line(buffer).await?;
        if n == 0 {
            return Err(Error::StreamClosed);
        }

        let line = buffer.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            // Skip blank lines — some agents emit them as keep-alives.
            return Err(Error::BlankLine);
        }

        info!("{name} <- {line}");

        sonic_rs::from_str(line).map_err(Into::into)
    }

    async fn recv_agent_error(
        err: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut String,
        name: &str,
    ) -> Result<()> {
        buffer.clear();
        if err.read_line(buffer).await? == 0 {
            return Err(Error::StreamClosed);
        }
        error!("{name} stderr <- {:?}", buffer.trim_end());
        Ok(())
    }

    async fn process_agent_message(
        &self,
        client_tx: &UnboundedSender<(AgentId, jsonrpc::Call)>,
        msg: AgentMessage,
    ) -> Result<()> {
        match msg {
            AgentMessage::Output(output) => self.process_response(output).await,
            AgentMessage::Call(call) => {
                client_tx
                    .send((self.id, call))
                    .context("failed to forward agent message to client")?;
                Ok(())
            }
        }
    }

    async fn process_response(&self, output: jsonrpc::Output) -> Result<()> {
        let (id, result) = match output {
            jsonrpc::Output::Success(jsonrpc::Success { id, result, .. }) => (id, Ok(result)),
            jsonrpc::Output::Failure(jsonrpc::Failure { id, error, .. }) => {
                error!("{} <- error: {error}", self.name);
                (id, Err(error.into()))
            }
        };

        if let Some(tx) = self.pending_requests.lock().await.remove(&id) {
            if let Err(_) = tx.send(result).await {
                error!(
                    "{}: response channel closed for request id={id:?} (likely timed out)",
                    self.name
                );
            }
        } else {
            error!(
                "{}: received response for unknown request id={id:?}",
                self.name
            );
        }

        Ok(())
    }

    async fn recv(
        transport: Arc<Self>,
        mut server_stdout: BufReader<ChildStdout>,
        client_tx: UnboundedSender<(AgentId, jsonrpc::Call)>,
    ) {
        let mut buffer = String::new();
        loop {
            match Self::recv_agent_message(&mut server_stdout, &mut buffer, &transport.name).await {
                Ok(msg) => {
                    if let Err(err) = transport
                        .process_agent_message(&client_tx, msg)
                        .await
                    {
                        error!("{} recv error: {err:?}", transport.name);
                        break;
                    }
                }
                Err(Error::BlankLine) => {
                    // Silently skip blank keep-alive lines.
                    continue;
                }
                Err(err) => {
                    if !matches!(err, Error::StreamClosed) {
                        error!(
                            "Exiting {} after unexpected error: {err:?}",
                            transport.name
                        );
                    }

                    // Fail all outstanding requests.
                    for (id, tx) in transport.pending_requests.lock().await.drain() {
                        let _ = tx.send(Err(Error::StreamClosed)).await;
                        error!(
                            "{}: failing pending request id={id:?} (stream closed)",
                            transport.name
                        );
                    }

                    // Inject a synthetic disconnect notification so the application layer
                    // can clean up the agent entry.
                    let _ = client_tx.send((
                        transport.id,
                        jsonrpc::Call::Notification(jsonrpc::Notification {
                            jsonrpc: None,
                            method: "$/disconnected".to_owned(),
                            params: jsonrpc::Params::None,
                        }),
                    ));

                    break;
                }
            }
        }
    }

    async fn err(transport: Arc<Self>, mut server_stderr: BufReader<ChildStderr>) {
        let mut buffer = String::new();
        loop {
            match Self::recv_agent_error(&mut server_stderr, &mut buffer, &transport.name).await {
                Ok(()) => {}
                Err(err) => {
                    error!("{} stderr loop ended: {err:?}", transport.name);
                    break;
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Send path
    // ------------------------------------------------------------------

    async fn send_payload(
        &self,
        server_stdin: &mut BufWriter<ChildStdin>,
        payload: Payload,
    ) -> Result<()> {
        let json = match payload {
            Payload::Request { chan, value } => {
                self.pending_requests
                    .lock()
                    .await
                    .insert(value.id.clone(), chan);
                serde_json::to_string(&value)?
            }
            Payload::Notification(value) => serde_json::to_string(&value)?,
            Payload::Response(value) => serde_json::to_string(&value)?,
        };
        self.send_line(server_stdin, &json).await
    }

    /// Write a single JSON line followed by `\n` to the agent's stdin.
    async fn send_line(
        &self,
        server_stdin: &mut BufWriter<ChildStdin>,
        line: &str,
    ) -> Result<()> {
        info!("{} -> {line}", self.name);
        server_stdin.write_all(line.as_bytes()).await?;
        server_stdin.write_all(b"\n").await?;
        server_stdin.flush().await?;
        Ok(())
    }

    async fn send(
        transport: Arc<Self>,
        mut server_stdin: BufWriter<ChildStdin>,
        mut client_rx: UnboundedReceiver<Payload>,
    ) {
        loop {
            match client_rx.recv().await {
                Some(payload) => {
                    if let Err(err) = transport.send_payload(&mut server_stdin, payload).await {
                        error!("{} send error: {err:?}", transport.name);
                        break;
                    }
                }
                None => break, // sender dropped — client shutting down
            }
        }
    }
}
