use helix_acp::{AgentId, DisplayLine};
use helix_view::graphics::{Margin, Modifier, Rect};
use helix_view::theme::Style;
use tui::buffer::Buffer as Surface;
use tui::text::{Span, Spans, Text};
use tui::widgets::{Block, Paragraph, Widget, Wrap};

use crate::compositor::{Callback, Component, Context, EventResult};

pub struct AgentPanel {
    pub agent_id: AgentId,
    scroll: usize,
    /// When true the panel follows the bottom of the buffer during streaming.
    /// Set to false as soon as the user manually scrolls up.
    pinned: bool,
    input: String,
}

impl AgentPanel {
    pub const ID: &'static str = "agent-panel";

    pub fn new(agent_id: AgentId) -> Self {
        Self {
            agent_id,
            scroll: 0,
            pinned: true,
            input: String::new(),
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
        thought_style: Style,
        tool_style: Style,
        done_style: Style,
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
                DisplayLine::ToolDone { status, .. } => {
                    lines.push(Spans::from(Span::styled(
                        format!("  [{status}]"),
                        done_style,
                    )));
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
            // Normalise common mode strings to short human-readable labels.
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
        if lines.is_empty() && client.is_prompting {
            lines.push(Spans::from(Span::styled("...", thought_style)));
        }

        // Reserve 2 rows at the bottom for separator + input field.
        let content_height = inner.height.saturating_sub(2);
        let visible_height = content_height as usize;

        // Auto-scroll: pin to bottom while streaming (unless user scrolled up).
        // Use visual-row count (accounting for line wrapping) so long Markdown
        // paragraphs scroll correctly.
        if client.is_prompting && self.pinned {
            let total_rows = Self::count_visual_lines(&lines, inner.width);
            self.scroll = total_rows.saturating_sub(visible_height);
        }

        let content_area = Rect::new(inner.x, inner.y, inner.width, content_height);
        let text = Text::from(lines);
        Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll as u16, 0))
            .render(content_area, surface);

        // Draw horizontal separator above input field.
        let sep_y = inner.y + inner.height.saturating_sub(2);
        let sep_str: String = "─".repeat(inner.width as usize);
        surface.set_string(inner.x, sep_y, &sep_str, popup_style);

        // Draw input line: dimmed "> " prefix then the typed text + cursor block.
        let dim_style = popup_style.add_modifier(Modifier::DIM);
        let input_y = inner.y + inner.height.saturating_sub(1);
        surface.set_string(inner.x, input_y, "> ", dim_style);
        let input_display = format!("{}▌", self.input);
        surface.set_string(inner.x + 2, input_y, &input_display, popup_style);
    }

    fn handle_event(&mut self, event: &crate::compositor::Event, cx: &mut Context) -> EventResult {
        use helix_view::input::{Event, KeyCode, KeyModifiers};
        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };
        let close_fn: Callback = Box::new(|compositor, _| {
            compositor.remove(Self::ID);
        });
        match key.code {
            KeyCode::Esc => EventResult::Consumed(Some(close_fn)),
            KeyCode::Char('j') | KeyCode::Down
                if key.modifiers.is_empty() && self.input.is_empty() =>
            {
                self.scroll = self.scroll.saturating_add(1);
                self.pinned = false;
                EventResult::Consumed(None)
            }
            KeyCode::Char('k') | KeyCode::Up
                if key.modifiers.is_empty() && self.input.is_empty() =>
            {
                self.scroll = self.scroll.saturating_sub(1);
                self.pinned = false;
                EventResult::Consumed(None)
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.push(c);
                EventResult::Consumed(None)
            }
            KeyCode::Backspace => {
                self.input.pop();
                EventResult::Consumed(None)
            }
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    let text = std::mem::take(&mut self.input);
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
                                    current_prompt = vec![]; // empty = continue
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
                }
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored(None),
        }
    }
}
