//! Glue between the editor and the `helix-claude-ide` server: start/stop,
//! name and workspace resolution, and lock-file cleanup on abnormal exit.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, bail};
use helix_claude_ide::{EditorHandler, Session, SharedHandler};
use helix_view::Editor;

/// Lock file of the running server, for the panic hook and `process::exit` paths.
static LOCK_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
static PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

/// The single workspace folder advertised to the CLI: the workspace root
/// (`.git`/`.helix` marker) or the current directory, canonicalised so that
/// `claude`'s `process.cwd()` compares equal (PROTO §1.3).
pub fn workspace_folder() -> PathBuf {
    let (root, _) = helix_loader::find_workspace();
    std::fs::canonicalize(&root).unwrap_or(root)
}

/// Name shown in the CLI's `/ide` picker: flag → config → workspace directory name.
pub fn ide_name(editor: &Editor, override_name: Option<&str>) -> String {
    if let Some(name) = override_name.map(str::trim).filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    let configured = editor.config().claude_ide.name.trim().to_string();
    if !configured.is_empty() {
        return configured;
    }
    workspace_folder()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Helix".to_string())
}

/// Start the IDE server and register it on the editor. Blocks the calling
/// thread on the Tokio runtime (same pattern as the MCP server start-up).
pub fn start<'e>(
    editor: &'e mut Editor,
    port: Option<u16>,
    name: Option<&str>,
) -> anyhow::Result<&'e Session> {
    if editor.claude_ide.is_some() {
        bail!("Claude IDE server is already running");
    }
    let mcp_tx = helix_mcp::editor_tx()
        .ok_or_else(|| anyhow!("editor command channel is not initialised"))?;
    let handler = Arc::new(EditorHandler::new(mcp_tx));
    let mut config = helix_claude_ide::Config::new(workspace_folder(), ide_name(editor, name));
    config.fixed_port = port;

    let runtime = tokio::runtime::Handle::current();
    let shared: SharedHandler = handler.clone();
    let handle =
        tokio::task::block_in_place(|| runtime.block_on(helix_claude_ide::start(config, shared)))?;

    *LOCK_PATH.lock().unwrap() = Some(handle.lock_path().to_path_buf());
    install_panic_hook();

    editor.claude_ide = Some(Session { handle, handler });
    Ok(editor.claude_ide.as_ref().unwrap())
}

/// Detach the running session from the editor; the caller must finish it
/// with [`stop_session`].
pub fn stop(editor: &mut Editor) -> Option<Session> {
    editor.claude_ide.take()
}

/// Close the client, stop the server and delete the lock file.
pub async fn stop_session(session: Session) {
    session.handle.stop().await;
    let mut guard = LOCK_PATH.lock().unwrap();
    if guard.as_deref() == Some(session.handle.lock_path()) {
        *guard = None;
    }
}

/// Best-effort synchronous lock-file removal for exits that bypass
/// `Application::close` (panics, `process::exit`). Idempotent.
pub fn remove_lock_file_now() {
    let path = LOCK_PATH.lock().ok().and_then(|guard| guard.clone());
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
}

fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            remove_lock_file_now();
            previous(info);
        }));
    });
}
