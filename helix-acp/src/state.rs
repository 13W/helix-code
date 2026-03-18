//! `AcpState` — UI-facing state for a single ACP agent session.
//!
//! This struct owns all state that was previously mixed into [`Client`]:
//! display buffer, session usage, config options, prompting flag, etc.
//! It lives alongside `Client` (keyed by the same `AgentId`) in the
//! [`Registry`], but can be borrowed independently.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use agent_client_protocol as sdk;
use helix_acp_types::*;

use crate::ReplyChannel;

/// Pre-processed data for updating an in-flight tool call.
///
/// The caller (currently `handle_acp_message`) extracts and formats
/// SDK fields into this struct; `AcpState::update_tool_call` then
/// applies pure state mutations and returns paths to reload.
#[derive(Debug, Default)]
pub struct ToolCallUpdate {
    /// Tool call ID (stringified).
    pub id: String,
    /// Updated title, if the update provides one.
    pub title: Option<String>,
    /// Pre-formatted input string (from `format_tool_input`).
    pub input: String,
    /// Text lines extracted from content blocks.
    pub output: Vec<String>,
    /// File paths from locations + rawInput.file_path.
    pub paths: Vec<String>,
    /// Whether the status is `Completed`.
    pub is_completed: bool,
    /// Whether the status is `Failed`.
    pub is_failed: bool,
}

/// UI-facing state for a single ACP agent session.
///
/// Created by [`Registry`] when an agent is spawned.  Borrowed by the
/// agent panel for rendering and by `handle_acp_message` for mutation.
#[derive(Debug)]
pub struct AcpState {
    /// Structured display buffer, accumulated from `session/update` notifications.
    pub display: Vec<DisplayLine>,
    /// True while a `session/prompt` job is in flight.
    pub is_prompting: bool,
    /// Tracks file paths for in-progress "edit" tool calls.
    pub pending_edits: HashMap<String, Vec<String>>,
    /// Current session mode received via `CurrentModeUpdate`.
    pub current_mode: Option<String>,
    /// Set to true by the permission dialog when the user selects an `AllowAlways` option.
    pub auto_continue: Arc<AtomicBool>,
    /// True after the user has selected "auto-accept edits" for this session.
    pub auto_accept_edits: bool,
    /// Accumulated token and cost statistics for the current session.
    pub session_usage: SessionUsage,
    /// Commands received via `AvailableCommandsUpdate`.
    pub available_commands: Vec<sdk::AvailableCommand>,
    /// Command text to drain into the textarea on the next panel event.
    pub pending_command: Option<String>,
    /// Session config options (model, mode, …) from `session/new` or `ConfigOptionUpdate`.
    pub config_options: Vec<sdk::SessionConfigOption>,
    /// Pending (option_id, value) to apply via `session_set_config_option` from the UI.
    pub pending_config_change: Option<(String, String)>,
    /// Pending reply channel + allow_always_id for a deferred "clean context" permission response.
    /// Set by the permission dialog; consumed by the main loop to send /clear then reply.
    pub pending_clean_context_reply:
        Option<(ReplyChannel<sdk::RequestPermissionResponse>, String)>,
    /// Authenticated user info, fetched after `authenticate()` + `session_new()` succeed.
    pub account_info: Option<AccountInfo>,
}

impl AcpState {
    /// Concatenate all [`DisplayLine::Text`] entries into a single string.
    pub fn response_text(&self) -> String {
        self.display
            .iter()
            .filter_map(|l| {
                if let DisplayLine::Text(s) = l {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Create a new default state, sharing the given `auto_continue` flag.
    pub fn new(auto_continue: Arc<AtomicBool>) -> Self {
        AcpState {
            display: Vec::new(),
            is_prompting: false,
            pending_edits: HashMap::new(),
            current_mode: None,
            auto_continue,
            auto_accept_edits: false,
            session_usage: SessionUsage::default(),
            available_commands: Vec::new(),
            pending_command: None,
            config_options: Vec::new(),
            pending_config_change: None,
            pending_clean_context_reply: None,
            account_info: None,
        }
    }

    // ── Display buffer mutations ─────────────────────────────────

    /// Append a text chunk, coalescing with the last [`DisplayLine::Text`].
    pub fn append_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.display.last_mut() {
            Some(DisplayLine::Text(s)) => s.push_str(text),
            _ => self.display.push(DisplayLine::Text(text.to_string())),
        }
    }

    /// Append a thought chunk, coalescing with the last [`DisplayLine::Thought`].
    pub fn append_thought(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.display.last_mut() {
            Some(DisplayLine::Thought(s)) => s.push_str(text),
            _ => self.display.push(DisplayLine::Thought(text.to_string())),
        }
    }

    /// Append an error message to the display buffer.
    pub fn append_error(&mut self, text: &str) {
        self.display.push(DisplayLine::Error(text.to_string()));
    }

    /// Record a new tool call in the display buffer.
    ///
    /// `edit_paths` should be `Some(paths)` for Edit-kind tools so we can
    /// track them for later buffer reload.  Returns nothing — the caller
    /// handles opening Read-kind paths separately.
    pub fn start_tool_call(
        &mut self,
        id: String,
        name: String,
        input: String,
        edit_paths: Option<Vec<String>>,
    ) {
        if let Some(paths) = edit_paths {
            self.pending_edits.insert(id.clone(), paths);
        }
        self.display.push(DisplayLine::ToolCall {
            id,
            name,
            input,
            output: Vec::new(),
        });
    }

    /// Apply an incremental tool-call update.
    ///
    /// Returns file paths to reload when the tool call reaches a terminal
    /// (`Completed`) status; empty otherwise.
    pub fn update_tool_call(&mut self, update: ToolCallUpdate) -> Vec<PathBuf> {
        // Track new file paths in pending_edits.
        if !update.paths.is_empty() {
            let entry = self.pending_edits.entry(update.id.clone()).or_default();
            for p in &update.paths {
                if !entry.contains(p) {
                    entry.push(p.clone());
                }
            }
        }

        // Step A: accumulate output on in-flight ToolCall entry.
        if !update.output.is_empty() {
            if let Some(pos) = self.find_tool_call(&update.id) {
                if let DisplayLine::ToolCall { output, .. } = &mut self.display[pos] {
                    *output = update.output.clone();
                }
            }
        }

        // Step A2: update name/input when the update provides them.
        if let Some(pos) = self.find_tool_call(&update.id) {
            if let DisplayLine::ToolCall { name, input, .. } = &mut self.display[pos] {
                if let Some(new_title) = &update.title {
                    *name = new_title.clone();
                }
                if !update.input.is_empty() {
                    *input = update.input.clone();
                }
            }
        }

        // Step B: flip to ToolDone on terminal status.
        let is_terminal = update.is_completed || update.is_failed;
        if is_terminal {
            let status_str = if update.is_failed {
                "failed".to_string()
            } else {
                "done".to_string()
            };
            if let Some(pos) = self.find_tool_call(&update.id) {
                let (name, prev_input, accumulated_output) =
                    if let DisplayLine::ToolCall {
                        name, input, output, ..
                    } = &self.display[pos]
                    {
                        (name.clone(), input.clone(), output.clone())
                    } else {
                        (String::new(), String::new(), Vec::new())
                    };
                let input = if update.input.is_empty() {
                    prev_input
                } else {
                    update.input
                };
                let final_output = if !update.output.is_empty() {
                    update.output
                } else {
                    accumulated_output
                };
                self.display[pos] = DisplayLine::ToolDone {
                    id: update.id.clone(),
                    name,
                    input,
                    status: status_str,
                    output: final_output,
                };
            }
        }

        // On completion: drain pending_edits → paths to reload.
        if update.is_completed {
            if let Some(paths) = self.pending_edits.remove(&update.id) {
                return paths.into_iter().map(PathBuf::from).collect();
            }
        }
        Vec::new()
    }

    /// Replace all plan steps in the display buffer.
    pub fn set_plan(&mut self, entries: &[(bool, String)]) {
        self.display
            .retain(|l| !matches!(l, DisplayLine::PlanStep { .. }));
        for (done, description) in entries {
            self.display.push(DisplayLine::PlanStep {
                done: *done,
                description: description.clone(),
            });
        }
    }

    /// Clear the display buffer and pending edits.
    pub fn clear(&mut self) {
        self.display.clear();
        self.pending_edits.clear();
    }

    // ── Scalar field setters ─────────────────────────────────────

    /// Update session usage from a `UsageUpdate` notification.
    pub fn update_usage(&mut self, used: u64, size: u64, cost: Option<(f64, String)>) {
        if let Some((amount, currency)) = cost {
            self.session_usage.cost_amount = amount;
            self.session_usage.currency = currency;
        }
        self.session_usage.context_used = used;
        self.session_usage.context_size = size;
    }

    /// Accumulate per-turn token counts.
    pub fn add_turn_tokens(
        &mut self,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) {
        self.session_usage.input_tokens += input;
        self.session_usage.output_tokens += output;
        self.session_usage.cache_read_tokens += cache_read;
        self.session_usage.cache_write_tokens += cache_write;
    }

    // ── Private helpers ──────────────────────────────────────────

    /// Find the display-buffer index of an in-flight `ToolCall` by ID.
    fn find_tool_call(&self, id: &str) -> Option<usize> {
        self.display
            .iter()
            .position(|l| matches!(l, DisplayLine::ToolCall { id: tid, .. } if tid == id))
    }
}
