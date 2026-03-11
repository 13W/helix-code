//! LSP extra tools: get_diagnostics, hover, code_actions, inlay_hints, completions.

use rmcp::model::{CallToolResult, Content};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::{editor_tx, McpCommand};
use super::serde_lenient;

// ---------------------------------------------------------------------------
// get_diagnostics
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetDiagnosticsParams {
    /// Optional absolute path to filter diagnostics to a single file.
    /// Omit to get all workspace diagnostics.
    pub path: Option<String>,
}

#[derive(Serialize)]
pub struct DiagnosticItemJson {
    pub path: String,
    pub line: usize,
    pub col: usize,
    pub severity: String,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

pub async fn handle_get_diagnostics(
    params: GetDiagnosticsParams,
) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let path = params.path.map(PathBuf::from);
    tx.send(McpCommand::GetDiagnostics { path, reply: reply_tx }).await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let items = reply_rx.await.map_err(|_| anyhow::anyhow!("editor did not reply"))?;

    let json_items: Vec<DiagnosticItemJson> = items
        .into_iter()
        .map(|d| DiagnosticItemJson {
            path: d.path.to_string_lossy().into_owned(),
            line: d.line,
            col: d.col,
            severity: d.severity,
            message: d.message,
            source: d.source,
            code: d.code,
        })
        .collect();
    let json = serde_json::to_string_pretty(&json_items)?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ---------------------------------------------------------------------------
// hover
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HoverParams {
    /// Absolute path to the file.
    pub path: String,
    /// 0-indexed line number.
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub line: usize,
    /// 0-indexed column number.
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub col: usize,
}

pub async fn handle_hover(params: HoverParams) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let path = PathBuf::from(&params.path);
    tx.send(McpCommand::Hover { path, line: params.line, col: params.col, reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let result = reply_rx.await.map_err(|_| anyhow::anyhow!("editor did not reply"))??;

    let json = serde_json::to_string_pretty(&result)?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ---------------------------------------------------------------------------
// code_actions
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CodeActionsParams {
    /// Absolute path to the file.
    pub path: String,
    /// 0-indexed line number.
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub line: usize,
    /// 0-indexed column number.
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub col: usize,
}

#[derive(Serialize)]
pub struct CodeActionItemJson {
    pub title: String,
    pub kind: Option<String>,
}

pub async fn handle_code_actions(params: CodeActionsParams) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let path = PathBuf::from(&params.path);
    tx.send(McpCommand::CodeActions { path, line: params.line, col: params.col, reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let actions = reply_rx.await.map_err(|_| anyhow::anyhow!("editor did not reply"))??;

    let json_items: Vec<CodeActionItemJson> = actions
        .into_iter()
        .map(|a| CodeActionItemJson { title: a.title, kind: a.kind })
        .collect();
    let json = serde_json::to_string_pretty(&json_items)?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ---------------------------------------------------------------------------
// inlay_hints
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct InlayHintsParams {
    /// Absolute path to the file.
    pub path: String,
    /// 0-indexed start line.
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub start_line: usize,
    /// 0-indexed end line (inclusive).
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub end_line: usize,
}

#[derive(Serialize)]
pub struct InlayHintItemJson {
    pub line: usize,
    pub col: usize,
    pub label: String,
    pub kind: String,
}

pub async fn handle_inlay_hints(params: InlayHintsParams) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let path = PathBuf::from(&params.path);
    tx.send(McpCommand::InlayHints {
        path,
        start_line: params.start_line,
        end_line: params.end_line,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let hints = reply_rx.await.map_err(|_| anyhow::anyhow!("editor did not reply"))??;

    let json_items: Vec<InlayHintItemJson> = hints
        .into_iter()
        .map(|h| InlayHintItemJson {
            line: h.line,
            col: h.col,
            label: h.label,
            kind: h.kind,
        })
        .collect();
    let json = serde_json::to_string_pretty(&json_items)?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ---------------------------------------------------------------------------
// completions
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CompletionsParams {
    /// Absolute path to the file.
    pub path: String,
    /// 0-indexed line number.
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub line: usize,
    /// 0-indexed column number.
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub col: usize,
}

#[derive(Serialize)]
pub struct CompletionItemJson {
    pub label: String,
    pub kind: Option<String>,
    pub detail: Option<String>,
    pub insert_text: Option<String>,
}

pub async fn handle_completions(params: CompletionsParams) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let path = PathBuf::from(&params.path);
    tx.send(McpCommand::Completions { path, line: params.line, col: params.col, reply: reply_tx })
        .await
        .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let items = reply_rx.await.map_err(|_| anyhow::anyhow!("editor did not reply"))??;

    let json_items: Vec<CompletionItemJson> = items
        .into_iter()
        .map(|c| CompletionItemJson {
            label: c.label,
            kind: c.kind,
            detail: c.detail,
            insert_text: c.insert_text,
        })
        .collect();
    let json = serde_json::to_string_pretty(&json_items)?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ---------------------------------------------------------------------------
// signature_help
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SignatureHelpParams {
    /// Absolute path to the file.
    pub path: String,
    /// 0-indexed line number.
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub line: usize,
    /// 0-indexed column number.
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub col: usize,
}

#[derive(Serialize)]
pub struct ParameterInfoJson {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

#[derive(Serialize)]
pub struct SignatureInfoJson {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    pub parameters: Vec<ParameterInfoJson>,
}

#[derive(Serialize)]
pub struct SignatureHelpJson {
    pub signatures: Vec<SignatureInfoJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

pub async fn handle_signature_help(
    params: SignatureHelpParams,
) -> anyhow::Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow::anyhow!("editor channel not available"))?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let path = PathBuf::from(&params.path);
    tx.send(McpCommand::SignatureHelp {
        path,
        line: params.line,
        col: params.col,
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor channel closed"))?;
    let result = reply_rx.await.map_err(|_| anyhow::anyhow!("editor did not reply"))??;

    match result {
        None => Ok(CallToolResult::success(vec![Content::text("null")])),
        Some(info) => {
            let json = SignatureHelpJson {
                signatures: info
                    .signatures
                    .into_iter()
                    .map(|s| SignatureInfoJson {
                        label: s.label,
                        documentation: s.documentation,
                        parameters: s
                            .parameters
                            .into_iter()
                            .map(|p| ParameterInfoJson {
                                label: p.label,
                                documentation: p.documentation,
                            })
                            .collect(),
                    })
                    .collect(),
                active_signature: info.active_signature,
                active_parameter: info.active_parameter,
            };
            let text = serde_json::to_string_pretty(&json)?;
            Ok(CallToolResult::success(vec![Content::text(text)]))
        }
    }
}
