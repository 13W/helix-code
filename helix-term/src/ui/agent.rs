use helix_acp::{AgentId, DisplayLine};
use helix_core::Position;
use helix_view::graphics::{Color, CursorKind, Margin, Modifier, Rect};
use tui::buffer::Buffer as Surface;
use tui::text::{Span, Spans, Text};
use tui::widgets::{Block, Paragraph, Widget, Wrap};

use crate::compositor::{Component, Context, EventResult};
use crate::ui::TextArea;

pub struct AgentPanel {
    pub agent_id: AgentId,
    scroll: usize,
    /// When true the panel follows the bottom of the buffer during streaming.
    /// Set to false as soon as the user manually scrolls up.
    pinned: bool,
    page_height: usize,
    input: TextArea,
    /// Set during render(); used by cursor() to report screen position.
    input_area: Rect,
    /// Cached visual-row height per DisplayLine entry.
    /// Computed as ceil(char_width / content_width) per span, summed — this
    /// approximates word-wrap and must be invalidated when content_width changes.
    line_heights: Vec<usize>,
    /// Content width used when the cache was last built; cleared on resize.
    last_content_width: u16,
}

impl AgentPanel {
    pub const ID: &'static str = "agent-panel";

    pub fn new(agent_id: AgentId) -> Self {
        let mut input = TextArea::new();
        input.max_lines = 4;
        Self {
            agent_id,
            scroll: 0,
            pinned: true,
            page_height: 10,
            input,
            input_area: Rect::default(),
            line_heights: Vec::new(),
            last_content_width: 0,
        }
    }

    /// Returns the estimated visual row height of a single DisplayLine entry
    /// for the given panel content width.
    ///
    /// Each Span produced by `build_lines` may word-wrap when rendered by
    /// `Paragraph`.  We approximate that with `ceil(char_width / content_width)`
    /// per span so that `total_rows` reflects visual rows, not logical span
    /// counts.  The cache must be invalidated whenever `content_width` changes.
    fn entry_height(
        entry: &DisplayLine,
        content_width: u16,
        theme: &helix_view::Theme,
        loader: &std::sync::Arc<arc_swap::ArcSwap<helix_core::syntax::Loader>>,
        thought_style: helix_view::theme::Style,
        tool_style: helix_view::theme::Style,
        done_style: helix_view::theme::Style,
    ) -> usize {
        let lines = Self::build_lines(
            std::slice::from_ref(entry),
            theme,
            loader,
            thought_style,
            tool_style,
            done_style,
        );
        if content_width == 0 {
            return lines.len().max(1);
        }
        let w = content_width as usize;
        lines
            .iter()
            .map(|spans| {
                let chars: usize = spans.0.iter().map(|s| s.content.chars().count()).sum();
                ((chars + w - 1) / w).max(1)
            })
            .sum::<usize>()
            .max(1)
    }

    /// Build a `Vec<Spans>` from the client's display buffer.
    fn build_lines(
        display: &[DisplayLine],
        theme: &helix_view::Theme,
        loader: &std::sync::Arc<arc_swap::ArcSwap<helix_core::syntax::Loader>>,
        thought_style: helix_view::theme::Style,
        tool_style: helix_view::theme::Style,
        done_style: helix_view::theme::Style,
    ) -> Vec<Spans<'static>> {
        let mut lines: Vec<Spans<'static>> = Vec::new();

        for entry in display {
            match entry {
                DisplayLine::Text(s) => {
                    let md = crate::ui::Markdown::new(s.clone(), loader.clone());
                    let parsed = md.parse(Some(theme));
                    for spans in parsed.lines {
                        let owned: Vec<Span<'static>> = spans
                            .0
                            .into_iter()
                            .map(|sp| Span::styled(sp.content.into_owned(), sp.style))
                            .collect();
                        lines.push(Spans::from(owned));
                    }
                }
                DisplayLine::Thought(s) => {
                    for line in s.lines() {
                        lines.push(Spans::from(Span::styled(
                            format!("~ {line}"),
                            thought_style,
                        )));
                    }
                }
                DisplayLine::ToolCall { name, input, .. } => {
                    // In-progress: yellow ●
                    let bullet_style = helix_view::theme::Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD);
                    let label = if input.is_empty() {
                        name.clone()
                    } else {
                        format!("{name}({input})")
                    };
                    lines.push(Spans::from(vec![
                        Span::styled("● ", bullet_style),
                        Span::styled(label, tool_style),
                    ]));
                }
                DisplayLine::ToolDone { name, input, status, output, .. } => {
                    let is_success = matches!(status.as_str(), "done" | "completed");
                    let bullet_style = if is_success {
                        helix_view::theme::Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        helix_view::theme::Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::BOLD)
                    };
                    let label = if input.is_empty() {
                        name.clone()
                    } else {
                        format!("{name}({input})")
                    };
                    lines.push(Spans::from(vec![
                        Span::styled("● ", bullet_style),
                        Span::styled(label, done_style),
                    ]));
                    // Show ALL output lines: first gets "  ⎿  ", rest get "     ".
                    if output.is_empty() {
                        lines.push(Spans::from(Span::styled("  ⎿  Done", thought_style)));
                    } else {
                        for (i, line) in output.iter().enumerate() {
                            let prefix = if i == 0 { "  ⎿  " } else { "     " };
                            lines.push(Spans::from(Span::styled(
                                format!("{prefix}{line}"),
                                thought_style,
                            )));
                        }
                    }
                }
                DisplayLine::PlanStep { done, description } => {
                    let marker = if *done { "x" } else { "-" };
                    let text_style = theme.get("ui.text.info");
                    let style = if *done { done_style } else { text_style };
                    lines.push(Spans::from(Span::styled(
                        format!("[{marker}] {description}"),
                        style,
                    )));
                }
                DisplayLine::UserMessage(s) => {
                    let user_style = theme.get("ui.text").add_modifier(Modifier::BOLD);
                    let mut iter = s.lines();
                    if let Some(first) = iter.next() {
                        lines.push(Spans::from(Span::styled(
                            format!("You: {first}"),
                            user_style,
                        )));
                    }
                    for rest in iter {
                        lines.push(Spans::from(Span::styled(
                            format!("     {rest}"),
                            user_style,
                        )));
                    }
                }
                DisplayLine::Separator => {
                    lines.push(Spans::from(Span::styled(
                        "─".repeat(40),
                        thought_style,
                    )));
                }
            }
        }

        lines
    }
}

impl Component for AgentPanel {
    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }

    fn render(&mut self, viewport: Rect, surface: &mut Surface, cx: &mut Context) {
        let Some(client) = cx.editor.acp.get(self.agent_id) else {
            return;
        };

        let popup_style = cx.editor.theme.get("ui.popup.info");
        let thought_style = cx
            .editor
            .theme
            .get("ui.text.info")
            .add_modifier(Modifier::DIM | Modifier::ITALIC);
        let tool_style = cx
            .editor
            .theme
            .get("ui.text.info")
            .add_modifier(Modifier::BOLD);
        let done_style = cx
            .editor
            .theme
            .get("ui.text.info")
            .add_modifier(Modifier::DIM);

        // Size: 60% width x 80% height, anchored bottom-right above statusline.
        let width = (viewport.width * 3 / 5).max(40).min(viewport.width);
        let height = (viewport.height * 4 / 5)
            .max(6)
            .min(viewport.height.saturating_sub(2));
        let area = Rect::new(
            viewport.width.saturating_sub(width),
            viewport.height.saturating_sub(height + 2), // +2 for statusline
            width,
            height,
        );

        // Build title: status first, then model, then mode, then usage — all inline.
        let mut title = format!(" {}", client.name);

        // Status badge first.
        if client.is_prompting {
            title.push_str(" [thinking…]");
        }

        // Model label from config_options (id = "model").
        if let Some(label) = config_option_current_label(&client.config_options, "model") {
            title.push_str(&format!(" [{label}]"));
        }

        // Mode label: use current_mode with find_label_for_value first (so it updates
        // immediately on CurrentModeUpdate even before config_options is refreshed),
        // then fall back to config_option_current_label, then raw current_mode.
        let mode_label = client
            .current_mode
            .as_deref()
            .and_then(|m| find_label_for_value(&client.config_options, "mode", m))
            .or_else(|| config_option_current_label(&client.config_options, "mode"))
            .or_else(|| {
                client.current_mode.as_ref().map(|m| match m.as_str() {
                    "plan" | "planMode" => "plan".to_string(),
                    "default" | "edit" | "editMode" => "edit".to_string(),
                    "auto" | "acceptEdits" | "accept_edits" => "edit".to_string(),
                    other => other.to_string(),
                })
            });
        if let Some(mode) = &mode_label {
            // Strip parenthetical suffix e.g. "Default (recommended)" → "Default".
            let short = mode.split(" (").next().unwrap_or(mode);
            title.push_str(&format!(" [{short}]"));
        }

        // Usage inline in title.
        let usage_label = {
            let su = &client.session_usage;
            let has_tokens = su.input_tokens > 0 || su.output_tokens > 0;
            let tokens_part = has_tokens.then(|| format!("↑{} ↓{}", su.output_tokens, su.input_tokens));
            let cost_part = (su.cost_amount > 0.0 || !su.currency.is_empty())
                .then(|| format!("${:.2}{}", su.cost_amount, su.currency));
            match (tokens_part, cost_part) {
                (Some(t), Some(c)) => format!("{t} {c}"),
                (Some(t), None)    => t,
                (None, Some(c))    => c,
                (None, None)       => String::new(),
            }
        };
        if !usage_label.is_empty() {
            title.push_str(&format!(" [{usage_label}]"));
        }

        // Context window usage: show percentage and compact used/total.
        if client.session_usage.context_size > 0 {
            let used = client.session_usage.context_used;
            let size = client.session_usage.context_size;
            let pct = (used as f64 / size as f64 * 100.0) as u64;
            title.push_str(&format!(" [ctx {pct}% {}/{}]", fmt_tokens(used), fmt_tokens(size)));
        }
        title.push(' ');

        let is_prompting = client.is_prompting;
        let has_commands = !client.available_commands.is_empty();

        surface.clear_with(area, popup_style);
        let block = Block::bordered()
            .title(title.as_str())
            .border_style(popup_style);
        let inner = block.inner(area).inner(Margin::horizontal(1));
        block.render(area, surface);

        // --- Virtual rendering: maintain per-entry visual-row height cache ---
        // Heights are approximated as ceil(char_width / content_width) per span
        // so they reflect word-wrap.  The cache is width-dependent and is cleared
        // whenever the content area width changes (e.g. terminal resize).

        let display_len = client.display.len();

        // Invalidate cache on width change so heights are recomputed for new wrap.
        if inner.width != self.last_content_width {
            self.line_heights.clear();
            self.last_content_width = inner.width;
        }

        // If display shrank (new session / cleared messages), reset auto-scroll
        // so the new conversation is followed from the start.
        if self.line_heights.len() > display_len {
            self.pinned = true;
            self.scroll = 0;
        }

        // Truncate to stable entries (all except last 2).
        // Last 2 are always recomputed: the last may be actively growing from streaming,
        // and the second-to-last may have grown in the same render gap when a new entry
        // was added, leaving a stale underestimate in the cache.
        self.line_heights.truncate(display_len.saturating_sub(2));

        for entry in &client.display[self.line_heights.len()..display_len] {
            let h = Self::entry_height(
                entry,
                inner.width,
                &cx.editor.theme,
                &cx.editor.syn_loader,
                thought_style,
                tool_style,
                done_style,
            );
            self.line_heights.push(h);
        }

        let input_rows = self.input.visual_rows_for(inner.width) as u16;

        // Reserve: 1 separator row + input_rows.
        let content_height = inner.height.saturating_sub(1 + input_rows);
        self.page_height = content_height as usize;

        let total_rows: usize = self.line_heights.iter().sum();

        // Auto-scroll: pin to bottom while streaming (unless user scrolled up).
        if self.pinned {
            self.scroll = total_rows.saturating_sub(content_height as usize);
        } else {
            // Clamp to prevent scrolling past bottom (shows empty space otherwise).
            let max_scroll = total_rows.saturating_sub(content_height as usize);
            self.scroll = self.scroll.min(max_scroll);
            // Restore auto-scroll when the user has scrolled back to the exact
            // bottom of overflowing content (max_scroll > 0 guards against the
            // trivial case where content fits in the panel and max_scroll == 0).
            if max_scroll > 0 && self.scroll >= max_scroll {
                self.pinned = true;
            }
        }

        let win_start = self.scroll;
        let win_end = self.scroll + content_height as usize;

        // Walk to find entry_start and the sub-entry span offset (scroll_within).
        // scroll_within is in logical Spans, not visual rows, so it is exact.
        let mut cumulative = 0usize;
        let mut entry_start = display_len; // default: past end (empty window)
        let mut scroll_within = 0usize;
        for (i, &h) in self.line_heights.iter().enumerate() {
            if cumulative + h > win_start {
                entry_start = i;
                scroll_within = win_start - cumulative;
                break;
            }
            cumulative += h;
        }

        // Walk forward from entry_start to find entry_end.
        let mut entry_end = entry_start;
        let mut cum2 = cumulative;
        for &h in &self.line_heights[entry_start..] {
            entry_end += 1;
            cum2 += h;
            if cum2 >= win_end {
                break;
            }
        }
        // Add one buffer entry to avoid clipping at the bottom.
        entry_end = (entry_end + 1).min(display_len);
        // Build visible spans: for entry_start, skip the first `scroll_within`
        // visual rows worth of spans (they are above the viewport). Include all
        // spans for subsequent entries.
        let first_spans = if entry_start < display_len {
            Self::build_lines(
                std::slice::from_ref(&client.display[entry_start]),
                &cx.editor.theme,
                &cx.editor.syn_loader,
                thought_style,
                tool_style,
                done_style,
            )
        } else {
            Vec::new()
        };
        let rest_start = (entry_start + 1).min(entry_end).min(display_len);
        let rest_spans = if rest_start < entry_end.min(display_len) {
            Self::build_lines(
                &client.display[rest_start..entry_end.min(display_len)],
                &cx.editor.theme,
                &cx.editor.syn_loader,
                thought_style,
                tool_style,
                done_style,
            )
        } else {
            Vec::new()
        };

        // Convert scroll_within from visual rows to a span count.
        // Each span may wrap to multiple visual rows; walk the spans
        // accumulating their visual heights to find how many to skip.
        let w = inner.width as usize;
        let mut skip = 0usize;
        let mut rows_accum = 0usize;
        for spans in &first_spans {
            if rows_accum >= scroll_within {
                break;
            }
            let chars: usize = spans.0.iter().map(|s| s.content.chars().count()).sum();
            let span_rows = if w > 0 { ((chars + w - 1) / w).max(1) } else { 1 };
            if rows_accum + span_rows > scroll_within {
                // This span straddles the boundary — show it fully
                // rather than clipping mid-span.
                break;
            }
            rows_accum += span_rows;
            skip += 1;
        }
        let mut lines: Vec<Spans<'static>> = Vec::new();
        lines.extend_from_slice(&first_spans[skip..]);
        lines.extend(rest_spans);

        // Show a placeholder while the first chunk is in flight.
        if lines.is_empty() && is_prompting {
            lines.push(Spans::from(Span::styled("...", thought_style)));
        }

        let content_area = Rect::new(inner.x, inner.y, inner.width, content_height);
        let text = Text::from(lines);
        Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .render(content_area, surface);

        // Draw horizontal separator above input field.
        // When commands are available, show a dim "/: commands" hint on the right.
        let sep_y = inner.y + content_height;
        let hint = if has_commands { " /: commands " } else { "" };
        let hint_len = hint.chars().count();
        let sep_len = (inner.width as usize).saturating_sub(hint_len);
        let sep_str: String = "─".repeat(sep_len);
        let hint_style = popup_style.add_modifier(Modifier::DIM);
        surface.set_string(inner.x, sep_y, &sep_str, popup_style);
        if hint_len > 0 {
            surface.set_string(inner.x + sep_len as u16, sep_y, hint, hint_style);
        }

        let input_top = sep_y + 1;
        let input_area = Rect::new(inner.x, input_top, inner.width, input_rows);
        self.input_area = input_area;

        let text_style = cx.editor.theme.get("ui.text");
        self.input.render(input_area, surface, text_style);
    }

    fn cursor(&self, _area: Rect, _editor: &helix_view::Editor) -> (Option<Position>, CursorKind) {
        if self.input_area == Rect::default() {
            return (None, CursorKind::Hidden);
        }
        match self.input.cursor_screen_pos(self.input_area) {
            Some((col, row)) => (
                Some(Position::new(row as usize, col as usize)),
                CursorKind::Block,
            ),
            None => (None, CursorKind::Hidden),
        }
    }

    fn handle_event(&mut self, event: &crate::compositor::Event, cx: &mut Context) -> EventResult {
        use helix_view::input::{Event, KeyCode, KeyModifiers};

        // Drain any command selected from the command menu.
        {
            let pending = cx.editor.acp.get_mut(self.agent_id)
                .and_then(|c| c.pending_command.take());
            if let Some(cmd) = pending {
                self.input.insert_str(&cmd);
                return EventResult::Consumed(None);
            }
        }

        match event {
            // Paste: insert clipboard text at cursor.
            Event::Paste(data) => {
                self.input.insert_str(data);
                return EventResult::Consumed(None);
            }
            Event::Key(_) => {}
            _ => return EventResult::Ignored(None),
        }

        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };

        match key.code {
            // Cancel running agent (Esc).
            KeyCode::Esc => {
                let client = cx.editor.acp.get(self.agent_id);
                if let Some(client) = client {
                    if client.is_prompting {
                        if let Some(ref session_id) = client.session_id {
                            let session_id = session_id.clone();
                            let handle = client.handle();
                            let _ = handle.session_cancel(session_id);
                        }
                        let client = cx.editor.acp.get_mut(self.agent_id).unwrap();
                        client.is_prompting = false;
                        cx.editor.set_status("Agent cancelled");
                    }
                }
                EventResult::Consumed(None)
            }

            // Cursor left (grapheme).
            KeyCode::Left if key.modifiers.is_empty() => {
                self.input.move_left();
                EventResult::Consumed(None)
            }

            // Cursor right (grapheme).
            KeyCode::Right if key.modifiers.is_empty() => {
                self.input.move_right();
                EventResult::Consumed(None)
            }

            // Ctrl+Left: word left.
            KeyCode::Left if key.modifiers == KeyModifiers::CONTROL => {
                self.input.move_word_left();
                EventResult::Consumed(None)
            }

            // Ctrl+Right: word right.
            KeyCode::Right if key.modifiers == KeyModifiers::CONTROL => {
                self.input.move_word_right();
                EventResult::Consumed(None)
            }

            // Up: move cursor up in input, or scroll output if already on first line.
            KeyCode::Up if key.modifiers.is_empty() => {
                let (line, _) = self.input_cursor_line_col();
                if line == 0 {
                    self.scroll = self.scroll.saturating_sub(1);
                    self.pinned = false;
                } else {
                    self.input.move_up();
                }
                EventResult::Consumed(None)
            }

            // Down: move cursor down in input, or scroll output if on last content line.
            KeyCode::Down if key.modifiers.is_empty() => {
                let (line, _) = self.input_cursor_line_col();
                let last_line = self.input.line_count().saturating_sub(1);
                if line >= last_line {
                    self.scroll = self.scroll.saturating_add(1);
                    self.pinned = false;
                } else {
                    self.input.move_down();
                }
                EventResult::Consumed(None)
            }

            // Page up/down for output scroll.
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(self.page_height.max(1));
                self.pinned = false;
                EventResult::Consumed(None)
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(self.page_height.max(1));
                self.pinned = false;
                EventResult::Consumed(None)
            }

            // Home/End for output scroll.
            KeyCode::Home => {
                self.scroll = 0;
                self.pinned = false;
                EventResult::Consumed(None)
            }
            KeyCode::End => {
                self.pinned = true; // render() will snap scroll to bottom
                EventResult::Consumed(None)
            }

            // Backspace: delete previous grapheme.
            KeyCode::Backspace => {
                self.input.delete_before();
                EventResult::Consumed(None)
            }

            // Delete: delete next grapheme.
            KeyCode::Delete => {
                EventResult::Consumed(None)
            }

            // Enter (no modifier or Shift only): insert newline.
            KeyCode::Enter
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.insert_char('\n');
                EventResult::Consumed(None)
            }

            // Ctrl+Enter or Alt+Enter: submit the input.
            // Alt+Enter is the reliable fallback for terminals that can't
            // distinguish Ctrl+Enter from plain Enter.
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                let text = self.input.text().trim_end_matches('\n').to_string();
                if text.is_empty() {
                    return EventResult::Consumed(None);
                }
                self.input.clear();

                // Handle /exit: stop the agent subprocess and remove all agent UI.
                if text.trim() == "/exit" {
                    let agent_id = self.agent_id;
                    cx.editor.acp.stop_agent(agent_id);
                    cx.editor.set_status("Agent session ended");
                    return EventResult::Consumed(Some(Box::new(move |compositor, _cx| {
                        compositor.remove(AgentPanel::ID);
                        compositor.remove("acp-permission");
                        compositor.stashed_agent_panel = None;
                        compositor.stashed_permission_dialogs.clear();
                    })));
                }

                // Handle /clear: reset local display and scroll, then send to agent.
                if text.trim() == "/clear" {
                    let agent_id = self.agent_id;
                    if let Some(client) = cx.editor.acp.get_mut(agent_id) {
                        client.display.clear();
                        client.session_usage = helix_acp::client::SessionUsage::default();
                    }
                    self.scroll = 0;
                    self.pinned = true;
                    self.line_heights.clear();

                    // Send /clear as a prompt so the agent clears its context too.
                    let state = cx.editor.acp.get(agent_id).and_then(|c| {
                        c.session_id.clone().map(|sid| (sid, c.handle(), c.auto_continue.clone()))
                    });
                    if let Some((session_id, handle, auto_continue)) = state {
                        if let Some(client) = cx.editor.acp.get_mut(agent_id) {
                            client.is_prompting = true;
                        }
                        let prompt = vec![helix_acp::ContentBlock::Text { text }];
                        cx.jobs.callback(async move {
                            use crate::job::Callback;
                            use std::sync::atomic::Ordering;

                            let mut current_prompt = prompt;
                            loop {
                                match handle
                                    .session_prompt(session_id.clone(), current_prompt)
                                    .await
                                {
                                    Err(e) => {
                                        return Ok(Callback::Editor(Box::new(
                                            move |editor: &mut helix_view::Editor| {
                                                if let Some(c) = editor.acp.get_mut(agent_id) {
                                                    c.is_prompting = false;
                                                }
                                                editor.set_error(format!("Agent error: {e}"));
                                            },
                                        )));
                                    }
                                    Ok(_stop) => {
                                        if auto_continue.swap(false, Ordering::SeqCst) {
                                            current_prompt = vec![];
                                        } else {
                                            break;
                                        }
                                    }
                                }
                            }
                            Ok(Callback::Editor(Box::new(
                                move |editor: &mut helix_view::Editor| {
                                    if let Some(c) = editor.acp.get_mut(agent_id) {
                                        c.is_prompting = false;
                                        // Clear display again in case the agent echoed
                                        // back messages during the /clear handling.
                                        c.display.clear();
                                    }
                                    editor.set_status("Context cleared");
                                },
                            )))
                        });
                        cx.editor.set_status("Clearing context\u{2026}");
                    }
                    return EventResult::Consumed(None);
                }

                let agent_id = self.agent_id;
                let state = cx.editor.acp.get(agent_id).and_then(|client| {
                    client.session_id.clone().map(|sid| {
                        (sid, client.handle(), client.auto_continue.clone())
                    })
                });

                if let Some((session_id, handle, auto_continue)) = state {
                    {
                        let client = cx.editor.acp.get_mut(agent_id).unwrap();
                        if !client.display.is_empty() {
                            client.display.push(helix_acp::DisplayLine::Separator);
                        }
                        client.display.push(helix_acp::DisplayLine::UserMessage(text.clone()));
                        client.is_prompting = true;
                    }
                    let prompt = vec![helix_acp::ContentBlock::Text { text }];
                    cx.jobs.callback(async move {
                        use crate::job::Callback;
                        use std::sync::atomic::Ordering;

                        let mut current_prompt = prompt;
                        let mut stop;

                        loop {
                            stop = match handle
                                .session_prompt(session_id.clone(), current_prompt)
                                .await
                            {
                                Err(e) => {
                                    return Ok(Callback::Editor(Box::new(
                                        move |editor: &mut helix_view::Editor| {
                                            if let Some(c) = editor.acp.get_mut(agent_id) {
                                                c.is_prompting = false;
                                            }
                                            editor.set_error(format!("Agent error: {e}"));
                                        },
                                    )));
                                }
                                Ok(s) => s,
                            };

                            let should_continue =
                                auto_continue.swap(false, Ordering::SeqCst);

                            if should_continue {
                                current_prompt = vec![];
                            } else {
                                break;
                            }
                        }

                        Ok(Callback::Editor(Box::new(
                            move |editor: &mut helix_view::Editor| {
                                if let Some(c) = editor.acp.get_mut(agent_id) {
                                    c.is_prompting = false;
                                }
                                editor.set_status(format!("Agent done ({stop:?})"));
                            },
                        )))
                    });
                    self.pinned = true;
                    cx.editor.set_status("Agent thinking\u{2026}");
                } else {
                    cx.editor.set_error("Agent is still initializing");
                }
                EventResult::Consumed(None)
            }

            // '/': open slash-command menu when input is empty and this is the first char.
            KeyCode::Char('/') if key.modifiers.is_empty() => {
                // If there is already text in the input, treat '/' as a regular character.
                if !self.input.text().is_empty() {
                    self.input.insert_char('/');
                    return EventResult::Consumed(None);
                }

                // Builtin local commands, always shown regardless of agent state.
                let builtins: &[(&str, &str)] = &[
                    ("exit",  "End session and close panel"),
                    ("clear", "Clear conversation context"),
                ];
                let builtin_count = builtins.len();

                let agent_commands = cx.editor.acp.get(self.agent_id)
                    .map(|c| c.available_commands.clone())
                    .unwrap_or_default();

                let mut items: Vec<crate::ui::MultiMenuItem> = builtins
                    .iter()
                    .map(|(name, desc)| crate::ui::MultiMenuItem {
                        label:    format!("/{name}"),
                        sublabel: Some((*desc).to_string()),
                    })
                    .collect();
                items.extend(agent_commands.iter().map(|cmd| crate::ui::MultiMenuItem {
                    label:    format!("/{}", cmd.name),
                    sublabel: Some(cmd.description.clone()),
                }));

                // Side-channel: validate callback records the chosen text;
                // on_close callback reads it and inserts into the panel input.
                // Rc is fine here — these closures are 'static but not Send.
                let selected = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
                let selected_for_close = selected.clone();

                let menu = crate::ui::MultiMenu::new(items, move |_editor, idx, event| {
                    use crate::ui::PromptEvent;
                    if event != PromptEvent::Validate { return; }
                    let text = if idx < builtin_count {
                        Some(format!("/{}", builtins[idx].0))
                    } else {
                        agent_commands.get(idx - builtin_count).map(|cmd| {
                            if cmd.input.is_some() { format!("/{} ", cmd.name) }
                            else                   { format!("/{}", cmd.name) }
                        })
                    };
                    *selected.borrow_mut() = text;
                })
                .with_on_close(move |compositor, _cx| {
                    if let Some(text) = selected_for_close.borrow_mut().take() {
                        if let Some(panel) = compositor.find_id::<AgentPanel>(AgentPanel::ID) {
                            panel.insert_input_text(&text);
                        }
                    }
                });

                EventResult::Consumed(Some(Box::new(move |compositor, _cx| {
                    compositor.push(Box::new(menu));
                })))
            }

            // Alt+M: open model picker.
            KeyCode::Char('m') if key.modifiers == KeyModifiers::ALT => {
                self.open_config_option_menu(cx, "model")
            }

            // Alt+P: open mode (permission mode) picker.
            KeyCode::Char('p') if key.modifiers == KeyModifiers::ALT => {
                self.open_config_option_menu(cx, "mode")
            }

            // Regular character insertion.
            KeyCode::Char(c)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.insert_char(c);
                EventResult::Consumed(None)
            }

            _ => EventResult::Ignored(None),
        }
    }
}

impl AgentPanel {
    fn input_cursor_line_col(&self) -> (usize, usize) {
        self.input.cursor_position()
    }

    /// Insert `text` at the current cursor position in the input field.
    /// Used by the slash-command menu's `on_close` hook to avoid a second keypress.
    pub fn insert_input_text(&mut self, text: &str) {
        self.input.insert_str(text);
    }

    /// Open a picker menu for a session config option (e.g. "model" or "mode").
    /// On selection, calls `session_set_config_option` via a background job.
    fn open_config_option_menu(&self, cx: &mut Context, option_id: &'static str) -> EventResult {
        use crate::ui::{MultiMenuItem, MultiMenu, PromptEvent};
        use helix_acp::sdk::{SessionConfigKind, SessionConfigSelectOptions};

        let Some(client) = cx.editor.acp.get(self.agent_id) else {
            return EventResult::Consumed(None);
        };

        // Find the matching config option and extract its select options.
        let select = client.config_options.iter().find_map(|opt| {
            if opt.id.to_string() == option_id {
                if let SessionConfigKind::Select(sel) = &opt.kind {
                    return Some(sel.clone());
                }
            }
            None
        });

        let Some(sel) = select else {
            cx.editor.set_error(format!("No config option '{option_id}' available"));
            return EventResult::Consumed(None);
        };

        // Flatten Ungrouped / Grouped into a plain Vec for the menu.
        let flat_options: Vec<helix_acp::sdk::SessionConfigSelectOption> = match &sel.options {
            SessionConfigSelectOptions::Ungrouped(opts) => opts.clone(),
            SessionConfigSelectOptions::Grouped(groups) => {
                groups.iter().flat_map(|g| g.options.iter().cloned()).collect()
            }
            _ => vec![],
        };

        let agent_id = self.agent_id;
        let items: Vec<MultiMenuItem> = flat_options.iter().map(|o| MultiMenuItem {
            label: o.name.clone(),
            sublabel: o.description.clone(),
        }).collect();

        let menu = MultiMenu::new(items, move |editor, idx, event| {
            if event != PromptEvent::Validate {
                return;
            }
            if let Some(o) = flat_options.get(idx) {
                if let Some(client) = editor.acp.get_mut(agent_id) {
                    client.pending_config_change =
                        Some((option_id.to_string(), o.value.to_string()));
                }
            }
        })
        .with_on_close(move |_, cx| {
            let change = cx.editor.acp.get_mut(agent_id)
                .and_then(|c| c.pending_config_change.take());
            if let Some((opt_id, value)) = change {
                if let Some(client) = cx.editor.acp.get(agent_id) {
                    if let Some(sid) = client.session_id.clone() {
                        let handle = client.handle();
                        cx.jobs.callback(async move {
                            let _ = handle
                                .session_set_config_option(sid, opt_id, value)
                                .await;
                            Ok(crate::job::Callback::Editor(Box::new(|_| {})))
                        });
                    }
                }
            }
        });

        EventResult::Consumed(Some(Box::new(move |compositor, _cx| {
            compositor.push(Box::new(menu));
        })))
    }
}


fn config_option_current_label(
    config_options: &[helix_acp::sdk::SessionConfigOption],
    option_id: &str,
) -> Option<String> {
    use helix_acp::sdk::SessionConfigKind;
    for opt in config_options {
        if opt.id.to_string() == option_id {
            if let SessionConfigKind::Select(sel) = &opt.kind {
                return Some(sel.current_value.to_string());
            }
        }
    }
    None
}

/// Look up the display name for `value` within the Select option identified by `option_id`.
/// Unlike `config_option_current_label`, this does not rely on the stored `current_value`.
fn find_label_for_value(
    config_options: &[helix_acp::sdk::SessionConfigOption],
    option_id: &str,
    value: &str,
) -> Option<String> {
    use helix_acp::sdk::{SessionConfigKind, SessionConfigSelectOptions};
    for opt in config_options {
        if opt.id.to_string() == option_id {
            if let SessionConfigKind::Select(sel) = &opt.kind {
                let choices: Vec<_> = match &sel.options {
                    SessionConfigSelectOptions::Ungrouped(opts) => opts.iter().collect(),
                    SessionConfigSelectOptions::Grouped(groups) => {
                        groups.iter().flat_map(|g| g.options.iter()).collect()
                    }
                    _ => vec![],
                };
                for choice in choices {
                    if choice.value.to_string() == value {
                        return Some(choice.name.clone());
                    }
                }
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Format a token count in compact form: 1234 → "1.2k", 1234567 → "1.2M".
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
