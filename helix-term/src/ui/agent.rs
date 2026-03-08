use helix_acp::{AgentId, DisplayLine};
use helix_core::Position;
use helix_view::graphics::{CursorKind, Margin, Modifier, Rect};
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
        }
    }

    /// Count the total number of visual (post-wrap) rows that `lines` would
    /// occupy in a panel of the given `width`.
    fn count_visual_lines(lines: &[Spans<'_>], width: u16) -> usize {
        let w = width as usize;
        if w == 0 {
            return lines.len();
        }
        lines
            .iter()
            .map(|spans| {
                let chars: usize =
                    spans.0.iter().map(|s| s.content.chars().count()).sum();
                if chars == 0 { 1 } else { chars.div_ceil(w) }
            })
            .sum()
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
                DisplayLine::ToolCall { name, .. } => {
                    lines.push(Spans::from(Span::styled(
                        format!("> {name}..."),
                        tool_style,
                    )));
                }
                DisplayLine::ToolDone { status, output, .. } => {
                    let (icon, style) = match status.as_str() {
                        "done" | "completed" => ("✓", done_style),
                        _ => ("✗", tool_style),
                    };
                    lines.push(Spans::from(Span::styled(
                        format!("  {icon} [{status}]"),
                        style,
                    )));
                    // Show up to 5 output lines, then a truncation hint.
                    const MAX_OUTPUT_LINES: usize = 5;
                    let total = output.len();
                    for line in output.iter().take(MAX_OUTPUT_LINES) {
                        lines.push(Spans::from(Span::styled(
                            format!("  {line}"),
                            thought_style,
                        )));
                    }
                    if total > MAX_OUTPUT_LINES {
                        lines.push(Spans::from(Span::styled(
                            format!("  ... (+{} lines)", total - MAX_OUTPUT_LINES),
                            thought_style,
                        )));
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

        // Build title badges: [mode] [auto-accept] [thinking…]
        let mut badges = String::new();
        if let Some(mode) = &client.current_mode {
            let label = match mode.as_str() {
                "plan" | "planMode" => "plan",
                "default" | "edit" | "editMode" => "edit",
                "auto" | "acceptEdits" | "accept_edits" => "edit",
                other => other,
            };
            badges.push_str(&format!(" [{label}]"));
        }
        if client.auto_accept_edits {
            badges.push_str(" [auto-accept]");
        }
        if client.is_prompting {
            badges.push_str(" [thinking…]");
        }
        if let Some((used, size, amount, currency)) = &client.usage {
            badges.push_str(&format!(" [{used}/{size} ${amount:.2}{currency}]"));
        }
        let title = format!(" {}{badges} ", client.name);
        let is_prompting = client.is_prompting;
        let has_commands = !client.available_commands.is_empty();

        surface.clear_with(area, popup_style);
        let block = Block::bordered()
            .title(title.as_str())
            .border_style(popup_style);
        let inner = block.inner(area).inner(Margin::horizontal(1));
        block.render(area, surface);

        let mut lines = Self::build_lines(
            &client.display,
            &cx.editor.theme,
            &cx.editor.syn_loader,
            thought_style,
            tool_style,
            done_style,
        );
        // Show a placeholder while the first chunk is in flight.
        if lines.is_empty() && is_prompting {
            lines.push(Spans::from(Span::styled("...", thought_style)));
        }

        let input_rows = self.input.visual_rows() as u16;

        // Reserve: 1 separator row + input_rows.
        let content_height = inner.height.saturating_sub(1 + input_rows);
        self.page_height = content_height as usize;

        // Auto-scroll: pin to bottom while streaming (unless user scrolled up).
        if self.pinned {
            let total_rows = Self::count_visual_lines(&lines, inner.width);
            self.scroll = total_rows.saturating_sub(content_height as usize);
        }

        let content_area = Rect::new(inner.x, inner.y, inner.width, content_height);
        let text = Text::from(lines);
        Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll as u16, 0))
            .render(content_area, surface);

        // Draw horizontal separator above input field.
        // When commands are available, show a dim "Tab: commands" hint on the right.
        let sep_y = inner.y + content_height;
        let hint = if has_commands { " Tab: commands " } else { "" };
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
            // Close panel.
            KeyCode::Esc => EventResult::Consumed(Some(Box::new(|compositor, _cx| {
                compositor.remove(AgentPanel::ID);
            }))),

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
                self.input.delete_after();
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
                    cx.editor.set_status("Agent thinking…");
                } else {
                    cx.editor.set_error("Agent is still initializing");
                }
                EventResult::Consumed(None)
            }

            // Tab: open slash-command menu if commands are available.
            KeyCode::Tab if key.modifiers.is_empty() => {
                let commands = cx.editor.acp.get(self.agent_id)
                    .map(|c| c.available_commands.clone())
                    .unwrap_or_default();

                if commands.is_empty() {
                    return EventResult::Consumed(None);
                }

                let agent_id = self.agent_id;
                let items: Vec<crate::ui::MultiMenuItem> = commands
                    .iter()
                    .map(|cmd| crate::ui::MultiMenuItem {
                        label: format!("/{}", cmd.name),
                        sublabel: Some(cmd.description.clone()),
                    })
                    .collect();

                let menu = crate::ui::MultiMenu::new(items, move |editor, idx, event| {
                    use crate::ui::PromptEvent;
                    if event != PromptEvent::Validate {
                        return;
                    }
                    if let Some(client) = editor.acp.get_mut(agent_id) {
                        if let Some(cmd) = commands.get(idx) {
                            let text = if cmd.input.is_some() {
                                format!("/{} ", cmd.name)
                            } else {
                                format!("/{}", cmd.name)
                            };
                            client.pending_command = Some(text);
                        }
                    }
                });

                EventResult::Consumed(Some(Box::new(move |compositor, _cx| {
                    compositor.push(Box::new(menu));
                })))
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
}
