pub mod buffer;
pub mod dap;
pub mod editor;
pub mod fs;
pub mod lsp_extras;
pub mod read;
pub mod registers;
pub mod serde_lenient;
pub mod symbols;
pub mod vcs;
pub mod write;

use std::time::Duration;

/// Default timeout for MCP tool operations waiting on the editor.
const EDITOR_REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// Await a oneshot reply from the editor with a timeout.
pub async fn editor_reply<T>(
    rx: tokio::sync::oneshot::Receiver<T>,
) -> anyhow::Result<T> {
    tokio::time::timeout(EDITOR_REPLY_TIMEOUT, rx)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "operation timed out ({}s)",
                EDITOR_REPLY_TIMEOUT.as_secs()
            )
        })?
        .map_err(|_| anyhow::anyhow!("editor did not reply"))
}
