use anyhow::Result;
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;

use crate::{editor_tx, McpCommand};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadFileParams {
    /// Absolute path to the file to load into the buffer.
    pub path: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnloadFileParams {
    /// Absolute path to the file to unload from the buffer.
    pub path: PathBuf,
}

pub async fn handle_load_file(params: LoadFileParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::LoadFile {
        path: params.path,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let msg = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("reply channel closed"))??;
    Ok(CallToolResult::success(vec![Content::text(msg)]))
}

pub async fn handle_unload_file(params: UnloadFileParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::UnloadFile {
        path: params.path,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let msg = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("reply channel closed"))??;
    Ok(CallToolResult::success(vec![Content::text(msg)]))
}
