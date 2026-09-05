//! Stand-alone IDE server for manual interoperability checks against the real
//! `claude` CLI, without starting Helix:
//!
//! ```text
//! cargo run -p helix-claude-ide --example serve            # serves the cwd
//! cargo run -p helix-claude-ide --example serve -- /path   # serves /path
//! ```
//!
//! Then, in that directory: `claude --ide` (or `/ide` inside a session).
//! Every JSON-RPC frame and tool call is printed to stderr.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use helix_claude_ide::{ClientId, Config, Notifier, ToolHandler, ToolResult};

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        eprintln!("[{}] {}", record.level(), record.args());
    }
    fn flush(&self) {}
}

static STDERR_LOGGER: StderrLogger = StderrLogger;

struct LoggingHandler;

#[async_trait]
impl ToolHandler for LoggingHandler {
    async fn call(
        &self,
        client: ClientId,
        name: &str,
        arguments: Value,
    ) -> anyhow::Result<ToolResult> {
        eprintln!("[tools/call {client}] {name} {arguments}");
        Ok(match name {
            "closeAllDiffTabs" => ToolResult::text("CLOSED_0_DIFF_TABS"),
            "close_tab" => ToolResult::text("TAB_CLOSED"),
            "getDiagnostics" => ToolResult::text("[]"),
            other => ToolResult::error(format!("{other}: not implemented in example")),
        })
    }

    fn on_client_connected(&self, client: ClientId, notifier: Notifier) {
        eprintln!("[client connected] {client} {notifier:?}");
    }

    fn on_client_disconnected(&self, client: ClientId) {
        eprintln!("[client disconnected] {client}");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = log::set_logger(&STDERR_LOGGER).map(|()| log::set_max_level(log::LevelFilter::Debug));
    let workspace = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let workspace = workspace.canonicalize()?;
    let config = Config::new(workspace.clone(), "Helix (example)");
    let handle = helix_claude_ide::start(config, Arc::new(LoggingHandler)).await?;
    eprintln!(
        "serving {} on ws://127.0.0.1:{} — lock file {}",
        workspace.display(),
        handle.port(),
        handle.lock_path().display()
    );
    tokio::signal::ctrl_c().await?;
    handle.stop().await;
    Ok(())
}
