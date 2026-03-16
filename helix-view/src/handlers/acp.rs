//! ACP event handler — processes [`AcpEvent`]s on the [`Editor`].
//!
//! Follows the same pattern as `handlers/dap.rs`: the bulk of the logic
//! lives here as `impl Editor` methods, and `helix-term` only handles
//! the thin layer that requires the compositor (permission dialogs).

use std::path::PathBuf;

use helix_acp::sdk::{self, ContentBlock, PlanEntryStatus, SessionUpdate, ToolCallStatus, ToolKind};
use helix_acp::{AcpEvent, AgentId, ReplyChannel, ToolCallUpdate};

use crate::editor::Action;
use crate::events::AcpStateChanged;
use crate::Editor;

/// Side effects that the handler cannot perform because they require the
/// compositor (which lives in `helix-term`).
pub enum AcpSideEffect {
    /// No compositor action needed.
    None,
    /// A permission dialog should be shown to the user.
    PermissionDialog {
        agent_id: AgentId,
        params: sdk::RequestPermissionRequest,
        reply: ReplyChannel<sdk::RequestPermissionResponse>,
    },
    /// An AskUserQuestion dialog should be shown.
    QuestionDialog {
        agent_id: AgentId,
        question: String,
        options: Vec<sdk::PermissionOption>,
        reply: ReplyChannel<sdk::RequestPermissionResponse>,
    },
}

/// Register ACP event hooks. Call once during startup.
pub fn register_hooks() {
    helix_event::register_hook!(move |_event: &mut AcpStateChanged| {
        helix_event::request_redraw();
        Ok(())
    });
}

impl Editor {
    /// Main entry point for ACP events.
    ///
    /// Performs all state mutations and file I/O directly.  Returns an
    /// [`AcpSideEffect`] when compositor interaction is required.
    pub fn handle_acp_event(
        &mut self,
        agent_id: AgentId,
        event: AcpEvent,
    ) -> AcpSideEffect {
        let side_effect = match event {
            AcpEvent::SessionNotification(notif) => {
                self.handle_acp_session_notification(agent_id, notif);
                AcpSideEffect::None
            }

            AcpEvent::RequestPermission { params, reply } => {
                // Detect AskUserQuestion tool calls by title.
                let is_question = params
                    .tool_call
                    .fields
                    .title
                    .as_deref()
                    == Some("AskUserQuestion");

                if is_question {
                    let question = params
                        .tool_call
                        .fields
                        .raw_input
                        .as_ref()
                        .and_then(|ri| ri["question"].as_str())
                        .unwrap_or("Question")
                        .to_string();
                    // Show the question in the agent panel.
                    if let Some(state) = self.acp.state_mut(agent_id) {
                        state.append_text(&question);
                    }
                    AcpSideEffect::QuestionDialog {
                        agent_id,
                        question,
                        options: params.options,
                        reply,
                    }
                } else {
                    // Push plan text to display buffer before handing off to the UI.
                    if let Some(plan) = params
                        .tool_call
                        .fields
                        .raw_input
                        .as_ref()
                        .and_then(|ri| ri["plan"].as_str())
                    {
                        if let Some(state) = self.acp.state_mut(agent_id) {
                            state.append_text(plan);
                        }
                    }
                    AcpSideEffect::PermissionDialog {
                        agent_id,
                        params,
                        reply,
                    }
                }
            }

            AcpEvent::ReadTextFile { params, reply } => {
                let response = match std::fs::read_to_string(&params.path) {
                    Ok(content) => sdk::ReadTextFileResponse::new(content),
                    Err(e) => {
                        log::warn!(
                            "ACP {agent_id}: fs/read_text_file error for {}: {e}",
                            params.path.display()
                        );
                        sdk::ReadTextFileResponse::new("")
                    }
                };
                let _ = reply.lock().unwrap().take().map(|tx| tx.send(response));
                AcpSideEffect::None
            }

            AcpEvent::WriteTextFile { params, reply } => {
                let path = params.path.clone();
                let write_ok = std::fs::write(&params.path, &params.content).is_ok();
                if !write_ok {
                    log::warn!(
                        "ACP {agent_id}: fs/write_text_file error for {}",
                        params.path.display()
                    );
                }
                let _ = reply
                    .lock()
                    .unwrap()
                    .take()
                    .map(|tx| tx.send(sdk::WriteTextFileResponse::new()));
                if write_ok {
                    self.acp_open_or_reload(&path);
                }
                AcpSideEffect::None
            }

            AcpEvent::Disconnected => {
                log::info!("ACP agent {agent_id} disconnected");
                self.acp.stop_agent(agent_id);
                AcpSideEffect::None
            }

            AcpEvent::UsageUpdate {
                used,
                size,
                amount,
                currency,
            } => {
                if let Some(state) = self.acp.state_mut(agent_id) {
                    state.update_usage(used, size, Some((amount, currency)));
                }
                AcpSideEffect::None
            }

            AcpEvent::TurnTokens {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            } => {
                if let Some(state) = self.acp.state_mut(agent_id) {
                    state.add_turn_tokens(
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_write_tokens,
                    );
                }
                AcpSideEffect::None
            }

            AcpEvent::ConfigOptionsUpdate(opts) => {
                if let Some(state) = self.acp.state_mut(agent_id) {
                    state.config_options = opts;
                }
                AcpSideEffect::None
            }
        };

        helix_event::dispatch(AcpStateChanged { agent_id });
        side_effect
    }

    /// Handle a `session/update` notification — state mutations + file I/O.
    fn handle_acp_session_notification(
        &mut self,
        agent_id: AgentId,
        notif: sdk::SessionNotification,
    ) {
        let mut paths_to_reload: Vec<PathBuf> = Vec::new();
        let mut paths_to_open: Vec<PathBuf> = Vec::new();

        let Some(state) = self.acp.state_mut(agent_id) else {
            return;
        };

        match notif.update {
            SessionUpdate::AgentMessageChunk(chunk)
            | SessionUpdate::UserMessageChunk(chunk) => {
                if let ContentBlock::Text(tc) = chunk.content {
                    state.append_text(&tc.text);
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                if let ContentBlock::Text(tc) = chunk.content {
                    state.append_thought(&tc.text);
                }
            }
            SessionUpdate::ToolCall(tc) => {
                let id_s = tc.tool_call_id.to_string();
                if tc.kind == ToolKind::Read {
                    for loc in &tc.locations {
                        paths_to_open.push(loc.path.clone());
                    }
                }
                let edit_paths = if tc.kind == ToolKind::Edit {
                    Some(
                        tc.locations
                            .iter()
                            .map(|l| l.path.to_string_lossy().into_owned())
                            .collect(),
                    )
                } else {
                    None
                };
                let input = format_tool_input(tc.raw_input.as_ref(), &tc.locations);
                state.start_tool_call(id_s, tc.title, input, edit_paths);
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let locations = update.fields.locations.as_deref().unwrap_or(&[]);
                let mut new_paths: Vec<String> = locations
                    .iter()
                    .map(|l| l.path.to_string_lossy().into_owned())
                    .collect();
                if let Some(fp) = update
                    .fields
                    .raw_input
                    .as_ref()
                    .and_then(|ri| ri["file_path"].as_str())
                {
                    let fp = fp.to_string();
                    if !new_paths.contains(&fp) {
                        new_paths.push(fp);
                    }
                }
                let new_output: Vec<String> = update
                    .fields
                    .content
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|c| {
                        use sdk::ToolCallContent;
                        match c {
                            ToolCallContent::Content(c) => {
                                if let ContentBlock::Text(t) = &c.content {
                                    Some(t.text.clone())
                                } else {
                                    None
                                }
                            }
                            ToolCallContent::Diff(diff) => {
                                Some(format!("~ {}", diff.path.display()))
                            }
                            _ => None,
                        }
                    })
                    .flat_map(|s| s.lines().map(|l| l.to_string()).collect::<Vec<_>>())
                    .collect();
                let input = format_tool_input(update.fields.raw_input.as_ref(), locations);
                let tc_update = ToolCallUpdate {
                    id: update.tool_call_id.to_string(),
                    title: update.fields.title.clone(),
                    input,
                    output: new_output,
                    paths: new_paths,
                    is_completed: update.fields.status == Some(ToolCallStatus::Completed),
                    is_failed: update.fields.status == Some(ToolCallStatus::Failed),
                };
                paths_to_reload.extend(state.update_tool_call(tc_update));
            }
            SessionUpdate::Plan(plan) => {
                let entries: Vec<(bool, String)> = plan
                    .entries
                    .into_iter()
                    .map(|e| (e.status == PlanEntryStatus::Completed, e.content))
                    .collect();
                state.set_plan(&entries);
            }
            SessionUpdate::CurrentModeUpdate(cmu) => {
                state.current_mode = Some(cmu.current_mode_id.to_string());
            }
            SessionUpdate::AvailableCommandsUpdate(acu) => {
                state.available_commands = acu.available_commands;
            }
            SessionUpdate::ConfigOptionUpdate(cou) => {
                state.config_options = cou.config_options;
            }
            SessionUpdate::UsageUpdate(uu) => {
                let cost = uu.cost.map(|c| (c.amount, c.currency));
                state.update_usage(uu.used, uu.size, cost);
            }
            _ => {}
        }

        // Open files the bot is reading so they're visible in the editor.
        for path in paths_to_open {
            if let Err(e) = self.open(&path, Action::Load) {
                log::warn!("ACP: could not open {}: {e}", path.display());
            }
        }

        // Reload files the bot has written; switch to the first edited file.
        let mut first_edit = true;
        for path in paths_to_reload {
            if self.document_by_path(&path).is_none() {
                if let Err(e) = self.open(&path, Action::Load) {
                    log::warn!("ACP: could not open {}: {e}", path.display());
                    continue;
                }
            }
            if first_edit {
                if let Some(doc_id) = self.document_by_path(&path).map(|d| d.id()) {
                    self.switch(doc_id, Action::Replace);
                }
                first_edit = false;
            }
            self.acp_reload_document(&path);
        }
    }

    /// Open a file if not loaded, or reload if already open.
    fn acp_open_or_reload(&mut self, path: &std::path::Path) {
        if self.document_by_path(path).is_none() {
            if let Err(e) = self.open(path, Action::Load) {
                log::warn!("ACP: could not open {}: {e}", path.display());
            }
        } else {
            self.acp_reload_document(path);
        }
    }

    /// Reload an open document by its filesystem path.
    ///
    /// Called after an ACP agent writes a file so the editor buffer reflects
    /// the new content.  No-op if the file is not currently open.
    pub fn acp_reload_document(&mut self, path: &std::path::Path) {
        let Some(doc) = self.document_by_path(path) else {
            return;
        };
        let doc_id = doc.id();
        let view_id = match doc.selections().keys().next().copied() {
            Some(v) => v,
            None => return,
        };
        let scrolloff = self.config().scrolloff;

        let view = self.tree.get_mut(view_id);
        let doc = self.documents.get_mut(&doc_id).unwrap();
        view.sync_changes(doc);
        if let Err(e) = doc.reload(view, &self.diff_providers) {
            log::warn!("ACP: reload failed for {}: {e}", path.display());
            return;
        }
        view.ensure_cursor_in_view(doc, scrolloff);

        // Notify LSP about the on-disk change.
        let doc = self.documents.get(&doc_id).unwrap();
        if let Some(p) = doc.path().map(|p| p.to_owned()) {
            self.language_servers.file_event_handler.file_changed(p);
        }
    }
}

/// Format tool input for display — prefers file locations, falls back to
/// raw input object values.  Returns an empty string when nothing useful.
fn format_tool_input(
    raw_input: Option<&serde_json::Value>,
    locations: &[sdk::ToolCallLocation],
) -> String {
    if !locations.is_empty() {
        let parts: Vec<String> = locations
            .iter()
            .map(|l| {
                l.path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| l.path.to_string_lossy().into_owned())
            })
            .collect();
        let joined = parts.join(", ");
        return truncate_display(&joined, 60);
    }

    if let Some(serde_json::Value::Object(map)) = raw_input {
        let parts: Vec<String> = map
            .values()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect();
        if !parts.is_empty() {
            let joined = parts.join(", ");
            return truncate_display(&joined, 60);
        }
    }

    String::new()
}

/// Truncate a string to `max_len` characters, appending `…` if truncated.
fn truncate_display(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        let end = s
            .char_indices()
            .nth(max_len - 3)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}…", &s[..end])
    } else {
        s.to_string()
    }
}
