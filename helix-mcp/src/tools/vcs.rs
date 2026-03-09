use anyhow::Result;
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::{editor_tx, HunkKind, McpCommand};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiffHunksParams {
    /// Absolute path to the file.
    pub path: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiffBaseParams {
    /// Absolute path to the file.
    pub path: PathBuf,
}

#[derive(Serialize)]
struct DiffHunkOut {
    kind: &'static str,
    before_start: usize,
    before_end: usize,
    after_start: usize,
    after_end: usize,
}

#[derive(Serialize)]
struct DiffHunksOut {
    path: PathBuf,
    head_ref: Option<String>,
    hunks: Vec<DiffHunkOut>,
}

pub async fn handle_diff_hunks(params: DiffHunksParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::GetDiffHunks {
        path: params.path,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let result = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("reply channel closed"))??;

    let out = DiffHunksOut {
        path: result.path,
        head_ref: result.head_ref,
        hunks: result
            .hunks
            .into_iter()
            .map(|h| DiffHunkOut {
                kind: match h.kind {
                    HunkKind::Added => "added",
                    HunkKind::Deleted => "deleted",
                    HunkKind::Modified => "modified",
                },
                before_start: h.before_start,
                before_end: h.before_end,
                after_start: h.after_start,
                after_end: h.after_end,
            })
            .collect(),
    };
    let json = serde_json::to_string_pretty(&out)?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

pub async fn handle_diff_base(params: DiffBaseParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::GetDiffBase {
        path: params.path,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let content = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("reply channel closed"))??;
    Ok(CallToolResult::success(vec![Content::text(content)]))
}
