use std::borrow::Cow;

use helix_view::{graphics::Rect, Editor};
use tui::{
    buffer::Buffer as Surface,
    widgets::{Block, Widget as _},
};

use crate::compositor::{Component, Context, Event, EventResult};

use super::{menu::Item, Menu, PromptEvent, Text};

pub struct Select<T: Item> {
    message: Text,
    options: Menu<T>,
    id: Option<&'static str>,
}

impl<T: Item> Select<T> {
    pub fn new<M, I, F>(message: M, options: I, data: T::Data, callback: F) -> Self
    where
        M: Into<Cow<'static, str>>,
        I: IntoIterator<Item = T>,
        F: Fn(&mut Editor, &T, PromptEvent) + 'static,
    {
        let message = tui::text::Text::from(message.into()).into();
        let options: Vec<_> = options.into_iter().collect();
        assert!(!options.is_empty());
        let mut menu = Menu::new(options, data, move |editor, option, event| {
            // Options are non-empty (asserted above) and an option is selected by default,
            // so `option` must be Some here.
            let option = &option.unwrap();
            callback(editor, option, event)
        })
        .auto_close(true);
        // Select the first option by default.
        menu.move_down();

        Self {
            message,
            options: menu,
            id: None,
        }
    }

    /// Disable auto-close so that only Enter confirms and only Esc/Ctrl-C rejects.
    /// Any other key is ignored and the dialog stays open.
    pub fn no_auto_close(mut self) -> Self {
        self.options.set_auto_close(false);
        self
    }

    /// Set a compositor ID so this dialog can be found and removed by ID.
    pub fn with_id(mut self, id: &'static str) -> Self {
        self.id = Some(id);
        self
    }
}

impl<T: Item> Component for Select<T> {
    fn id(&self) -> Option<&'static str> {
        self.id
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let result = self.options.handle_event(event, cx);
        // Select is a modal overlay - always consume key events so they don't
        // reach the editor layer below.
        match (event, result) {
            (Event::Key(_), EventResult::Ignored(cb)) => EventResult::Consumed(cb),
            (_, result) => result,
        }
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let (message_width, message_height) = self.message.required_size(viewport).unwrap();
        let (menu_width, menu_height) = self.options.required_size(viewport).unwrap();
        Some((
            (message_width + 2).max(menu_width) + 2, // inner content + 2 border cols
            message_height + 1 + menu_height + 2,    // msg + separator + menu + 2 border rows
        ))
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // +---------------------------+
        // | message text              |
        // |---------------------------|
        // | ▶ Allow once              |
        // |   Allow always            |
        // |   Deny                    |
        // +---------------------------+

        let background = cx.editor.theme.get("ui.background");
        let text_style = cx.editor.theme.get("ui.text");
        let border_style = cx.editor.theme.get("ui.popup");

        // Limit the text width to 80% of the screen or 80 columns, whichever is smaller.
        let max_width = 80.min(((area.width as u32) * 80u32 / 100) as u16);
        let (message_width, message_height) =
            super::text::required_size(&self.message.contents, max_width);
        let (menu_width, menu_height) = self
            .options
            .required_size((max_width, area.height))
            .unwrap();

        // Inner content width: wider of padded message or menu items.
        let inner_width = (message_width + 2).max(menu_width);
        let width = inner_width + 2; // +2 for left/right border
        let height = message_height + 1 + menu_height + 2; // +2 for top/bottom border, +1 sep

        // Strictly center within the given area (origin-aware, no underflow).
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let dialog = Rect::new(x, y, width.min(area.width), height.min(area.height));

        surface.clear_with(dialog, background.patch(text_style));
        Block::bordered()
            .border_style(border_style)
            .render(dialog, surface);

        // `inner` is the area inside the border (1px stripped on all sides).
        let inner = Block::bordered().inner(dialog);

        // Message with 1-char horizontal padding so text isn't flush with the border.
        let msg_height = message_height.min(inner.height);
        let msg_area = Rect::new(
            inner.x + 1,
            inner.y,
            inner.width.saturating_sub(2),
            msg_height,
        );
        self.message.render(msg_area, surface, cx);

        // ─── separator between message and options.
        let sep_y = inner.y + msg_height;
        let sep: String = "─".repeat(inner.width as usize);
        surface.set_string(inner.x, sep_y, &sep, border_style);

        // Options menu below the separator.
        let avail = inner.height.saturating_sub(msg_height + 1);
        let menu_area = Rect::new(inner.x, sep_y + 1, inner.width, avail.min(menu_height));
        self.options.render(menu_area, surface, cx);
    }
}
