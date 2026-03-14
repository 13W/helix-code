use anyhow::Result;
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::{editor_tx, McpCommand};
use super::editor_reply;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadRegisterParams {
    /// Single-character register name (e.g. "a", "/", "+").
    pub name: char,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteRegisterParams {
    /// Single-character register name. Only a-z, A-Z, '+', '*' are writable.
    pub name: char,
    /// Values to store in the register.
    pub values: Vec<String>,
}

#[derive(Serialize)]
struct RegisterReadOut {
    name: String,
    values: Vec<String>,
}

#[derive(Serialize)]
struct WriteOkOut {
    ok: bool,
}

#[derive(Serialize)]
struct JumplistOut {
    jumps: Vec<JumpOut>,
}

#[derive(Serialize)]
struct JumpOut {
    path: PathBuf,
    line: usize,
    col: usize,
}

pub async fn handle_read_register(params: ReadRegisterParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::ReadRegister {
        name: params.name,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let values = editor_reply(reply_rx).await??;
    let out = RegisterReadOut {
        name: params.name.to_string(),
        values,
    };
    let json = serde_json::to_string_pretty(&out)?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

pub async fn handle_write_register(params: WriteRegisterParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::WriteRegister {
        name: params.name,
        values: params.values,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    editor_reply(reply_rx).await??;
    let json = serde_json::to_string_pretty(&WriteOkOut { ok: true })?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

pub async fn handle_get_jumplist() -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::GetJumplist { reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let entries = editor_reply(reply_rx).await?;
    let out = JumplistOut {
        jumps: entries
            .into_iter()
            .map(|e| JumpOut {
                path: e.path,
                line: e.line,
                col: e.col,
            })
            .collect(),
    };
    let json = serde_json::to_string_pretty(&out)?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}
