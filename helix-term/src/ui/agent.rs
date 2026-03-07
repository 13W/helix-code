use helix_acp::AgentId;
use helix_view::graphics::{Margin, Rect};
use tui::buffer::Buffer as Surface;
use tui::text::Text;
use tui::widgets::{Block, Paragraph, Widget, Wrap};

use crate::compositor::{Callback, Component, Context, EventResult};

pub struct AgentPanel {
    pub agent_id: AgentId,
    scroll: usize,
}

impl AgentPanel {
    pub const ID: &'static str = "agent-panel";

    pub fn new(agent_id: AgentId) -> Self {
        Self { agent_id, scroll: 0 }
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

        // Size: 60% width × 40% height, anchored bottom-right above statusline
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
            format!(" {} [thinking…] ", client.name)
        } else {
            format!(" {} ", client.name)
        };

        surface.clear_with(area, popup_style);
        let block = Block::bordered()
            .title(title.as_str())
            .border_style(popup_style);
        let inner = block.inner(area).inner(Margin::horizontal(1));
        block.render(area, surface);

        let text = Text::styled(client.response_buf.as_str(), text_style);
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
                EventResult::Consumed(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored(None),
        }
    }
}
