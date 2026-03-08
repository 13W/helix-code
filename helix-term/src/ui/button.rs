use helix_view::graphics::{CursorKind, Rect};
use helix_view::Editor;
use tui::buffer::Buffer as Surface;

use crate::compositor::{Component, Context, EventResult};
use crate::ui::prompt::PromptEvent;

/// A horizontal row of labeled buttons.
///
/// Renders as:  `[ Allow ]  [ Deny ]  [ Cancel ]`
///
/// The focused button is highlighted with `ui.menu.selected`.
pub struct ButtonGroup {
    buttons: Vec<String>,
    selected: usize,
    callback: Box<dyn Fn(&mut Editor, usize, PromptEvent) + 'static>,
}

impl ButtonGroup {
    pub fn new(
        buttons: Vec<String>,
        callback: impl Fn(&mut Editor, usize, PromptEvent) + 'static,
    ) -> Self {
        Self {
            buttons,
            selected: 0,
            callback: Box::new(callback),
        }
    }

    /// Total width needed to render all buttons side by side.
    pub fn required_width(&self) -> u16 {
        self.buttons
            .iter()
            .map(|b| b.chars().count() as u16 + 4) // "[ label ]"
            .sum::<u16>()
            .saturating_add(
                2 * self.buttons.len().saturating_sub(1) as u16, // 2-space gaps
            )
    }

    pub fn required_size(&self) -> (u16, u16) {
        (self.required_width(), 1)
    }
}

impl Component for ButtonGroup {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let normal_style = cx.editor.theme.get("ui.menu");
        let selected_style = cx.editor.theme.get("ui.menu.selected");

        let mut x = area.x;
        let y = area.y;

        for (i, label) in self.buttons.iter().enumerate() {
            if i > 0 {
                // 2-space gap between buttons
                x += 2;
            }
            let style = if i == self.selected {
                selected_style
            } else {
                normal_style
            };
            let text = format!("[ {} ]", label);
            let width = text.chars().count() as u16;
            surface.set_stringn(x, y, &text, width as usize, style);
            x += width;
        }
    }

    fn handle_event(&mut self, event: &crate::compositor::Event, cx: &mut Context) -> EventResult {
        use helix_view::input::{Event, KeyCode, KeyModifiers};

        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };

        match key.code {
            // Left arrow or Shift+Tab: move selection left.
            KeyCode::Left => {
                self.selected = self.selected.saturating_sub(1);
                EventResult::Consumed(None)
            }
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.selected = self.selected.saturating_sub(1);
                EventResult::Consumed(None)
            }
            KeyCode::Right | KeyCode::Tab => {
                self.selected = (self.selected + 1).min(self.buttons.len().saturating_sub(1));
                EventResult::Consumed(None)
            }
            KeyCode::Enter => {
                let idx = self.selected;
                (self.callback)(cx.editor, idx, PromptEvent::Validate);
                EventResult::Consumed(Some(Box::new(|compositor, _cx| {
                    compositor.remove(ButtonGroup::ID_HINT);
                })))
            }
            KeyCode::Esc => {
                let idx = self.selected;
                (self.callback)(cx.editor, idx, PromptEvent::Abort);
                EventResult::Consumed(Some(Box::new(|compositor, _cx| {
                    compositor.remove(ButtonGroup::ID_HINT);
                })))
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let idx = self.selected;
                (self.callback)(cx.editor, idx, PromptEvent::Abort);
                EventResult::Consumed(Some(Box::new(|compositor, _cx| {
                    compositor.remove(ButtonGroup::ID_HINT);
                })))
            }
            _ => EventResult::Ignored(None),
        }
    }

    fn cursor(&self, _area: Rect, _editor: &Editor) -> (Option<helix_core::Position>, CursorKind) {
        (None, CursorKind::Hidden)
    }
}

impl ButtonGroup {
    // Used internally to close the layer — callers should wrap in their own
    // compositor ID when embedding into a larger dialog.
    const ID_HINT: &'static str = "button-group";
}
