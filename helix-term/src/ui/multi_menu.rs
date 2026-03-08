use helix_core::Position;
use helix_view::graphics::{CursorKind, Modifier, Rect};
use helix_view::Editor;
use tui::buffer::Buffer as Surface;

use crate::compositor::{Component, Context, EventResult};
use crate::ui::prompt::PromptEvent;

pub struct MultiMenuItem {
    pub label: String,
    pub sublabel: Option<String>,
}

/// A vertical menu where each item may have an optional dimmed sublabel.
///
/// ```text
/// ▶ Allow once
///   Allow always
///     (will not ask again)
///   Deny
/// ```
pub struct MultiMenu {
    items: Vec<MultiMenuItem>,
    selected: usize,
    callback: Box<dyn Fn(&mut Editor, usize, PromptEvent) + 'static>,
}

impl MultiMenu {
    pub const ID: &'static str = "multi-menu";

    pub fn new(
        items: Vec<MultiMenuItem>,
        callback: impl Fn(&mut Editor, usize, PromptEvent) + 'static,
    ) -> Self {
        Self {
            items,
            selected: 0,
            callback: Box::new(callback),
        }
    }

    /// `(width, height)` required to render all items.
    pub fn required_size(&self) -> (u16, u16) {
        let max_label = self
            .items
            .iter()
            .map(|item| item.label.chars().count())
            .max()
            .unwrap_or(0);
        let max_sublabel = self
            .items
            .iter()
            .filter_map(|item| item.sublabel.as_ref())
            .map(|s| s.chars().count() + 2) // +2 for "  " indent
            .max()
            .unwrap_or(0);
        let width = (max_label + 2).max(max_sublabel) as u16; // +2 for prefix "▶ " / "  "

        let height: u16 = self
            .items
            .iter()
            .map(|item| if item.sublabel.is_some() { 2u16 } else { 1u16 })
            .sum();

        (width, height)
    }
}

impl Component for MultiMenu {
    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let normal_style = cx.editor.theme.get("ui.menu");
        let selected_style = cx.editor.theme.get("ui.menu.selected");
        let sublabel_style = normal_style.add_modifier(Modifier::DIM);

        let mut y = area.y;

        for (i, item) in self.items.iter().enumerate() {
            let (prefix, style) = if i == self.selected {
                ("▶ ", selected_style)
            } else {
                ("  ", normal_style)
            };

            let label = format!("{}{}", prefix, item.label);
            surface.set_stringn(area.x, y, &label, area.width as usize, style);
            y += 1;

            if let Some(sub) = &item.sublabel {
                let sub_text = format!("    {}", sub);
                surface.set_stringn(area.x, y, &sub_text, area.width as usize, sublabel_style);
                y += 1;
            }

            if y >= area.y + area.height {
                break;
            }
        }
    }

    fn handle_event(&mut self, event: &crate::compositor::Event, cx: &mut Context) -> EventResult {
        use helix_view::input::{Event, KeyCode, KeyModifiers};

        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };

        match key.code {
            KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
                self.selected = self.selected.saturating_sub(1);
                EventResult::Consumed(None)
            }
            KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
                self.selected =
                    (self.selected + 1).min(self.items.len().saturating_sub(1));
                EventResult::Consumed(None)
            }
            KeyCode::Enter => {
                let idx = self.selected;
                (self.callback)(cx.editor, idx, PromptEvent::Validate);
                EventResult::Consumed(Some(Box::new(|compositor, _cx| {
                    compositor.remove(MultiMenu::ID);
                })))
            }
            KeyCode::Esc => {
                let idx = self.selected;
                (self.callback)(cx.editor, idx, PromptEvent::Abort);
                EventResult::Consumed(Some(Box::new(|compositor, _cx| {
                    compositor.remove(MultiMenu::ID);
                })))
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let idx = self.selected;
                (self.callback)(cx.editor, idx, PromptEvent::Abort);
                EventResult::Consumed(Some(Box::new(|compositor, _cx| {
                    compositor.remove(MultiMenu::ID);
                })))
            }
            _ => EventResult::Ignored(None),
        }
    }

    fn cursor(&self, _area: Rect, _editor: &Editor) -> (Option<Position>, CursorKind) {
        (None, CursorKind::Hidden)
    }
}
