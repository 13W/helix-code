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
}

impl AgentPanel {
    pub const ID: &'static str = "agent-panel";

    pub fn new(agent_id: AgentId) -> Self {
        Self {
            agent_id,
            scroll: 0,
            pinned: true,
        }
    }

    /// Build a `Vec<Spans>` from the client's display buffer.
    fn build_lines(display: &[DisplayLine], text_style: Style, thought_style: Style, tool_style: Style, done_style: Style) -> Vec<Spans<'static>> {
        let mut lines: Vec<Spans<'static>> = Vec::new();

        for entry in display {
            match entry {
                DisplayLine::Text(s) => {
                    for line in s.lines() {
                        lines.push(Spans::from(Span::styled(line.to_owned(), text_style)));
                    }
                    // Preserve trailing newline as an empty line so appending chunks work.
                    if s.ends_with('\n') {
                        lines.push(Spans::from(Span::raw("")));
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
                    let style = if *done { done_style } else { text_style };
                    lines.push(Spans::from(Span::styled(
                        format!("[{marker}] {description}"),
                        style,
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
        let text_style = cx.editor.theme.get("ui.text.info");
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

        // Size: 60% width x 40% height, anchored bottom-right above statusline.
        let width = (viewport.width * 3 / 5).max(40).min(viewport.width);
        let height = (viewport.height * 2 / 5)
            .max(6)
            .min(viewport.height.saturating_sub(2));
        let area = Rect::new(
            viewport.width.saturating_sub(width),
            viewport.height.saturating_sub(height + 2), // +2 for statusline
            width,
            height,
        );

        let title = if client.is_prompting {
            format!(" {} [thinking...] ", client.name)
        } else {
            format!(" {} ", client.name)
        };

        surface.clear_with(area, popup_style);
        let block = Block::bordered()
            .title(title.as_str())
            .border_style(popup_style);
        let inner = block.inner(area).inner(Margin::horizontal(1));
        block.render(area, surface);

        let lines = Self::build_lines(
            &client.display,
            text_style,
            thought_style,
            tool_style,
            done_style,
        );
        let visible_height = inner.height as usize;

        // Auto-scroll: pin to bottom while streaming (unless user scrolled up).
        if client.is_prompting && self.pinned {
            self.scroll = lines.len().saturating_sub(visible_height);
        }

        let text = Text::from(lines);
        Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll as u16, 0))
            .render(inner, surface);
    }

    fn handle_event(&mut self, event: &crate::compositor::Event, _cx: &mut Context) -> EventResult {
        use helix_view::input::{Event, KeyCode};
        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };
        let close_fn: Callback = Box::new(|compositor, _| {
            compositor.remove(Self::ID);
        });
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => EventResult::Consumed(Some(close_fn)),
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                self.pinned = false;
                EventResult::Consumed(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                self.pinned = false;
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored(None),
        }
    }
}
