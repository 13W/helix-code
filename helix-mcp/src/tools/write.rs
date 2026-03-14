use crate::{editor_tx, McpCommand, TextEdit};
use anyhow::{anyhow, Result};
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;

use super::editor_reply;
use tokio::sync::oneshot;

use super::serde_lenient;

// ── write_file ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// New file contents (full replacement)
    pub content: String,
}

pub async fn handle_write_file(params: WriteFileParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::WriteFile {
        path,
        content: params.content,
        reply: reply_tx,
    })
    .await?;
    let result = editor_reply(reply_rx).await??;
    let json = serde_json::json!({
        "path": result.path.to_string_lossy(),
        "lines_changed": result.lines_changed,
        "saved": result.saved,
    });
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}

// ── patch_file (apply_edits) ───────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TextEditParams {
    /// First line to replace (1-indexed, inclusive)
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub start_line: usize,
    /// First line NOT replaced (1-indexed, exclusive).
    /// Use `end_line == start_line` for a pure insertion (no lines removed).
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub end_line: usize,
    /// Replacement text. Empty string to delete lines.
    pub new_text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PatchFileParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// List of edits to apply
    #[serde(deserialize_with = "serde_lenient::string_or_vec")]
    pub edits: Vec<TextEditParams>,
}

pub async fn handle_patch_file(params: PatchFileParams) -> Result<CallToolResult> {
    if params.edits.is_empty() {
        anyhow::bail!("edits list is empty");
    }
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let edits: Vec<TextEdit> = params
        .edits
        .into_iter()
        .map(|e| TextEdit {
            start_line: e.start_line,
            end_line: e.end_line,
            new_text: e.new_text,
        })
        .collect();
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::ApplyEdits {
        path,
        edits,
        reply: reply_tx,
    })
    .await?;
    let result = editor_reply(reply_rx).await??;
    let json = serde_json::json!({
        "path": result.path.to_string_lossy(),
        "lines_changed": result.lines_changed,
        "saved": result.saved,
    });
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}

// ── insert_text ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InsertTextParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// Line number before which to insert (1-indexed)
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub line: usize,
    /// Text to insert (should end with a newline)
    pub text: String,
}

pub async fn handle_insert_text(params: InsertTextParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::InsertText {
        path,
        line: params.line,
        text: params.text,
        reply: reply_tx,
    })
    .await?;
    let result = editor_reply(reply_rx).await??;
    let json = serde_json::json!({
        "path": result.path.to_string_lossy(),
        "lines_changed": result.lines_changed,
        "saved": result.saved,
    });
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}

// ── edit_file ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// Exact string to find and replace. Omit to do a pure line-range replacement.
    pub old_string: Option<String>,
    /// Replacement text
    pub new_string: String,
    /// 1-indexed inclusive start line (scope for old_string search, or range start)
    #[serde(default, deserialize_with = "serde_lenient::string_or_usize_opt")]
    pub start_line: Option<usize>,
    /// 1-indexed exclusive end line (scope for old_string search, or range end)
    #[serde(default, deserialize_with = "serde_lenient::string_or_usize_opt")]
    pub end_line: Option<usize>,
    /// Replace all occurrences instead of just the first. Default: false.
    #[serde(default)]
    pub replace_all: bool,
}


pub async fn handle_edit_file(params: EditFileParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::EditFile {
        path,
        old_string: params.old_string,
        new_string: params.new_string,
        start_line: params.start_line,
        end_line: params.end_line,
        replace_all: params.replace_all,
        reply: reply_tx,
    })
    .await?;
    let result = editor_reply(reply_rx).await??;
    let json = serde_json::json!({
        "path": result.path.to_string_lossy(),
        "lines_changed": result.lines_changed,
        "saved": result.saved,
    });
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}

// ── rename_symbol ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenameSymbolParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// 0-indexed line number of the symbol to rename
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub line: usize,
    /// 0-indexed column number of the symbol to rename
    #[serde(deserialize_with = "serde_lenient::string_or_usize")]
    pub col: usize,
    /// New name for the symbol
    pub new_name: String,
}

pub async fn handle_rename_symbol(params: RenameSymbolParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::RenameSymbol {
        path,
        line: params.line,
        col: params.col,
        new_name: params.new_name,
        reply: reply_tx,
    })
    .await?;
    let result = editor_reply(reply_rx).await??;
    let json = serde_json::json!({
        "path": result.path.to_string_lossy(),
        "lines_changed": result.lines_changed,
        "saved": result.saved,
    });
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn resolve_path(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    std::fs::canonicalize(&p).unwrap_or(p)
}

// ── replace_symbol ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplaceSymbolParams {
    /// File path (absolute or relative to CWD)
    pub path: String,
    /// Symbol name-path, e.g. `"MyStruct"` or `"MyStruct/my_method"`
    pub name_path: String,
    /// New symbol body (full replacement, including the signature line)
    pub body: String,
}

pub async fn handle_replace_symbol(params: ReplaceSymbolParams) -> Result<CallToolResult> {
    let tx = editor_tx().ok_or_else(|| anyhow!("no editor connection"))?;
    let path = resolve_path(&params.path);
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(McpCommand::ReplaceSymbol {
        path,
        name_path: params.name_path,
        body: params.body,
        reply: reply_tx,
    })
    .await?;
    let result = editor_reply(reply_rx).await??;
    let json = serde_json::json!({
        "path": result.path.to_string_lossy(),
        "lines_changed": result.lines_changed,
        "saved": result.saved,
    });
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}
