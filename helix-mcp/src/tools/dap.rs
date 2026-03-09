//! DAP (Debug Adapter Protocol) tools: breakpoints, state queries, execution control.

use rmcp::model::{CallToolResult, Content};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use crate::{editor_tx, McpCommand};

// ---------------------------------------------------------------------------
// get_breakpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetBreakpointsParams {
    /// Optional absolute path to filter to a single file.
    /// Omit to get all breakpoints across all files.
    pub path: Option<String>,
}

#[derive(Serialize)]
struct BreakpointJson {
    pub path: String,
    pub line: usize,
    pub column: Option<usize>,
    pub condition: Option<String>,
    pub verified: bool,
    pub id: Option<usize>,
    pub message: Option<String>,
}

pub async fn handle_get_breakpoints(
    params: GetBreakpointsParams,
) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let path = params.path.map(PathBuf::from);
    tx.send(McpCommand::GetBreakpoints { path, reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let bps = reply_rx.await.map_err(|_| anyhow::anyhow!("editor did not reply"))?;
    let json_items: Vec<BreakpointJson> = bps
        .into_iter()
        .map(|b| BreakpointJson {
            path: b.path.to_string_lossy().into_owned(),
            line: b.line,
            column: b.column,
            condition: b.condition,
            verified: b.verified,
            id: b.id,
            message: b.message,
        })
        .collect();
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&json_items)?,
    )]))
}

// ---------------------------------------------------------------------------
// set_breakpoint
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SetBreakpointParams {
    /// Absolute path to the source file.
    pub path: String,
    /// 0-indexed line number.
    pub line: usize,
    /// Optional conditional expression (e.g. `"x > 5"`).
    pub condition: Option<String>,
}

pub async fn handle_set_breakpoint(params: SetBreakpointParams) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::SetBreakpoint {
        path: PathBuf::from(&params.path),
        line: params.line,
        condition: params.condition,
        reply: Arc::new(Mutex::new(Some(reply_tx))),
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let bp = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("editor did not reply"))??;
    let json = serde_json::to_string_pretty(&BreakpointJson {
        path: bp.path.to_string_lossy().into_owned(),
        line: bp.line,
        column: bp.column,
        condition: bp.condition,
        verified: bp.verified,
        id: bp.id,
        message: bp.message,
    })?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ---------------------------------------------------------------------------
// remove_breakpoint
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RemoveBreakpointParams {
    /// Absolute path to the source file.
    pub path: String,
    /// 0-indexed line number.
    pub line: usize,
}

pub async fn handle_remove_breakpoint(
    params: RemoveBreakpointParams,
) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::RemoveBreakpoint {
        path: PathBuf::from(&params.path),
        line: params.line,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("editor did not reply"))??;
    Ok(CallToolResult::success(vec![Content::text(
        "{\"ok\":true}",
    )]))
}

// ---------------------------------------------------------------------------
// get_dap_status
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DapStatusJson {
    pub active: bool,
    pub paused: bool,
    pub thread_id: Option<usize>,
    pub active_frame: Option<usize>,
}

pub async fn handle_get_dap_status() -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::GetDapStatus { reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let s = reply_rx.await.map_err(|_| anyhow::anyhow!("editor did not reply"))?;
    let json = serde_json::to_string_pretty(&DapStatusJson {
        active: s.active,
        paused: s.paused,
        thread_id: s.thread_id,
        active_frame: s.active_frame,
    })?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ---------------------------------------------------------------------------
// get_stack_trace
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetStackTraceParams {
    /// Thread id to query. Omit to use the active thread.
    pub thread_id: Option<usize>,
}

#[derive(Serialize)]
struct StackFrameJson {
    pub id: usize,
    pub name: String,
    pub path: Option<String>,
    pub line: usize,
    pub col: usize,
    pub is_active: bool,
}

pub async fn handle_get_stack_trace(
    params: GetStackTraceParams,
) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::GetStackTrace {
        thread_id: params.thread_id,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let frames = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("editor did not reply"))??;
    let json_items: Vec<StackFrameJson> = frames
        .into_iter()
        .map(|f| StackFrameJson {
            id: f.id,
            name: f.name,
            path: f.path.map(|p| p.to_string_lossy().into_owned()),
            line: f.line,
            col: f.col,
            is_active: f.is_active,
        })
        .collect();
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&json_items)?,
    )]))
}

// ---------------------------------------------------------------------------
// get_scopes
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetScopesParams {
    /// Stack frame id (from get_stack_trace).
    pub frame_id: usize,
}

#[derive(Serialize)]
struct ScopeJson {
    pub name: String,
    pub variables_ref: usize,
}

pub async fn handle_get_scopes(params: GetScopesParams) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::GetScopes {
        frame_id: params.frame_id,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let scopes = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("editor did not reply"))??;
    let json_items: Vec<ScopeJson> = scopes
        .into_iter()
        .map(|s| ScopeJson {
            name: s.name,
            variables_ref: s.variables_ref,
        })
        .collect();
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&json_items)?,
    )]))
}

// ---------------------------------------------------------------------------
// get_variables
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetVariablesParams {
    /// Stack frame index (into the active thread's frame list). Omit for the active frame.
    pub frame_id: Option<usize>,
}

#[derive(Serialize)]
struct VariableJson {
    pub name: String,
    pub value: String,
    pub type_name: Option<String>,
    pub variables_ref: usize,
}

pub async fn handle_get_variables(params: GetVariablesParams) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::GetVariables {
        frame_id: params.frame_id,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let vars = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("editor did not reply"))??;
    let json_items: Vec<VariableJson> = vars
        .into_iter()
        .map(|v| VariableJson {
            name: v.name,
            value: v.value,
            type_name: v.type_name,
            variables_ref: v.variables_ref,
        })
        .collect();
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&json_items)?,
    )]))
}

// ---------------------------------------------------------------------------
// dap_continue
// ---------------------------------------------------------------------------

pub async fn handle_dap_continue() -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::DapContinue { reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("editor did not reply"))??;
    Ok(CallToolResult::success(vec![Content::text("{\"ok\":true}")]))
}

// ---------------------------------------------------------------------------
// dap_pause
// ---------------------------------------------------------------------------

pub async fn handle_dap_pause() -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::DapPause { reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("editor did not reply"))??;
    Ok(CallToolResult::success(vec![Content::text("{\"ok\":true}")]))
}

// ---------------------------------------------------------------------------
// dap_step_over
// ---------------------------------------------------------------------------

pub async fn handle_dap_step_over() -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::DapStepOver { reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("editor did not reply"))??;
    Ok(CallToolResult::success(vec![Content::text("{\"ok\":true}")]))
}

// ---------------------------------------------------------------------------
// dap_step_in
// ---------------------------------------------------------------------------

pub async fn handle_dap_step_in() -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::DapStepIn { reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("editor did not reply"))??;
    Ok(CallToolResult::success(vec![Content::text("{\"ok\":true}")]))
}

// ---------------------------------------------------------------------------
// dap_step_out
// ---------------------------------------------------------------------------

pub async fn handle_dap_step_out() -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(McpCommand::DapStepOut { reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("editor did not reply"))??;
    Ok(CallToolResult::success(vec![Content::text("{\"ok\":true}")]))
}
