use crate::{editor_tx, McpCommand};
use anyhow::{anyhow, Result};
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;
use tokio::sync::oneshot;

use super::serde_lenient;

// ── get_symbols_overview ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSymbolsOverviewParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// Depth: 0 = top-level only, 1 = top-level + immediate children. Default: 0.
    #[serde(default, deserialize_with = "serde_lenient::string_or_u8_opt")]
    pub depth: Option<u8>,
}

pub async fn handle_get_symbols_overview(
    params: GetSymbolsOverviewParams,
) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let depth = params.depth.unwrap_or(0);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::GetSymbolsOverview {
        path,
        depth,
        reply: reply_tx,
    })
    .await?;
    let (symbols, source) = reply_rx.await??;
    let json = serde_json::json!({
        "symbols": symbols_to_json(&symbols),
        "source": source,
    });
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&json)?,
    )]))
}

fn symbols_to_json(symbols: &[crate::SymbolInfo]) -> serde_json::Value {
    serde_json::Value::Array(
        symbols
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "kind": s.kind,
                    "range": { "start_line": s.range.start_line, "end_line": s.range.end_line },
                    "children": symbols_to_json(&s.children),
                })
            })
            .collect(),
    )
}

// ── find_symbol ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindSymbolParams {
    /// Symbol name or substring to search for
    pub query: String,
    /// Optional file or directory path to restrict results
    pub path: Option<String>,
    /// Whether to include the symbol's source body. Default: false.
    pub include_body: Option<bool>,
}

pub async fn handle_find_symbol(params: FindSymbolParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = params.path.as_deref().map(resolve_path);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::FindSymbol {
        query: params.query,
        path,
        include_body: params.include_body.unwrap_or(false),
        reply: reply_tx,
    })
    .await?;
    let matches = reply_rx.await??;
    let json = serde_json::json!({
        "symbols": matches.iter().map(|m| serde_json::json!({
            "name": m.name,
            "kind": m.kind,
            "path": m.path.to_string_lossy(),
            "range": { "start_line": m.range.start_line, "end_line": m.range.end_line },
            "body": m.body,
        })).collect::<Vec<_>>(),
    });
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&json)?,
    )]))
}

// ── find_refs ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindRefsParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// 0-indexed line number of the symbol
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub line: usize,
    /// 0-indexed column number of the symbol
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub col: usize,
}

pub async fn handle_find_refs(params: FindRefsParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::FindRefs {
        path,
        line: params.line,
        col: params.col,
        reply: reply_tx,
    })
    .await?;
    let refs = reply_rx.await??;
    let count = refs.len();
    let json = serde_json::json!({
        "refs": refs.iter().map(|r| serde_json::json!({
            "path": r.path.to_string_lossy(),
            "line": r.line,
            "col": r.col,
            "preview": r.preview,
        })).collect::<Vec<_>>(),
        "count": count,
    });
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&json)?,
    )]))
}

// ── read_symbol ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadSymbolParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// Symbol name path, e.g. `"MyStruct"` or `"MyStruct/my_method"`
    pub name_path: String,
}

pub async fn handle_read_symbol(params: ReadSymbolParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::ReadSymbol {
        path,
        name_path: params.name_path,
        reply: reply_tx,
    })
    .await?;
    let m = reply_rx.await??;
    let json = serde_json::json!({
        "name": m.name,
        "kind": m.kind,
        "path": m.path.to_string_lossy(),
        "range": { "start_line": m.range.start_line, "end_line": m.range.end_line },
        "body": m.body,
    });
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&json)?,
    )]))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn resolve_path(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    std::fs::canonicalize(&p).unwrap_or(p)
}
