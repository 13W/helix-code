use helix_core::unicode::width::UnicodeWidthStr;
use helix_core::Position;
use helix_view::graphics::{CursorKind, Margin, Modifier, Rect};
use helix_view::Editor;
use tui::buffer::Buffer as Surface;
use tui::widgets::{Block, Widget};

use crate::compositor::{Callback, Component, Compositor, Context, EventResult};
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
    scroll: usize,
    last_inner_width: u16,
    last_inner_height: u16,
    callback: Box<dyn Fn(&mut Editor, usize, PromptEvent) + 'static>,
    /// Called (once) right after the menu removes itself on Validate.
    on_close: Option<Callback>,
}

/// Wrap `text` to fit within `max_width` display columns, splitting on word
/// boundaries.  Always returns at least one element.
fn wrap_text(text: &str, max_width: u16) -> Vec<String> {
    if max_width == 0 {
        return vec![String::new()];
    }
    let max = max_width as usize;
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width: usize = 0;

    for word in text.split_whitespace() {
        let word_width = word.width();
        if current.is_empty() {
            if word_width > max {
                lines.push(word.to_string());
            } else {
                current.push_str(word);
                current_width = word_width;
            }
        } else if current_width + 1 + word_width <= max {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            lines.push(current.clone());
            current.clear();
            current_width = 0;
            if word_width > max {
                lines.push(word.to_string());
            } else {
                current.push_str(word);
                current_width = word_width;
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Number of visual rows an item occupies given `inner_width`.
fn item_visual_height(item: &MultiMenuItem, inner_width: u16) -> u16 {
    let label_lines = wrap_text(&item.label, inner_width.saturating_sub(2)).len(); // "▶ "/"  "
    let sublabel_lines = item
        .sublabel
        .as_ref()
        .map(|s| wrap_text(s, inner_width.saturating_sub(4)).len()) // "    "
        .unwrap_or(0);
    (label_lines + sublabel_lines).max(1) as u16
}

fn total_content_height(items: &[MultiMenuItem], inner_width: u16) -> u16 {
    items
        .iter()
        .map(|item| item_visual_height(item, inner_width))
        .sum()
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
            scroll: 0,
            last_inner_width: 0,
            last_inner_height: 0,
            callback: Box::new(callback),
            on_close: None,
        }
    }

    /// Register a one-shot callback to run (via the compositor) immediately
    /// after the menu closes on `Validate` (Enter).  The callback receives
    /// mutable access to both the `Compositor` and `Context`, so it can e.g.
    /// find another component and insert text without requiring a second
    /// keypress.
    pub fn with_on_close(
        mut self,
        f: impl FnOnce(&mut Compositor, &mut Context) + 'static,
    ) -> Self {
        self.on_close = Some(Box::new(f));
        self
    }

    /// `(width, height)` required to render all items without wrapping.
    pub fn content_size(&self) -> (u16, u16) {
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

    /// Adjust `self.scroll` so that `self.selected` is visible.
    fn ensure_visible(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
            return;
        }
        let w = self.last_inner_width;
        let h = self.last_inner_height as usize;
        if h == 0 {
            return;
        }
        loop {
            if self.scroll > self.selected {
                break;
            }
            let rows_used: usize = self.items[self.scroll..=self.selected]
                .iter()
                .map(|item| item_visual_height(item, w) as usize)
                .sum();
            if rows_used <= h {
                break;
            }
            self.scroll += 1;
        }
    }
}

impl Component for MultiMenu {
    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }

    fn render(&mut self, viewport: Rect, surface: &mut Surface, cx: &mut Context) {
        let normal_style = cx.editor.theme.get("ui.menu");
        let selected_style = cx.editor.theme.get("ui.menu.selected");
        let sublabel_style = normal_style.add_modifier(Modifier::DIM);
        let popup_style = cx.editor.theme.get("ui.popup");

        // Reserve top (bufferline) and bottom (statusline/commandline) rows.
        let safe_area = Rect::new(
            viewport.x,
            viewport.y + 1,
            viewport.width,
            viewport.height.saturating_sub(2),
        );
        if safe_area.width == 0 || safe_area.height == 0 {
            return;
        }

        // Window width: exactly 50% of viewport width, min 20, capped at safe_area.
        let win_w = (safe_area.width / 2).max(20).min(safe_area.width);
        let inner_w = win_w.saturating_sub(2); // subtract left+right border

        // Window height: content (with wrap) + 2 border rows, capped at 90% of safe area.
        let total_h = total_content_height(&self.items, inner_w);
        let max_h = ((safe_area.height * 9 / 10).max(3)).min(safe_area.height);
        let win_h = (total_h + 2).min(max_h).min(safe_area.height);

        // Center in safe area.
        let x = safe_area.x + (safe_area.width.saturating_sub(win_w)) / 2;
        let y = safe_area.y + (safe_area.height.saturating_sub(win_h)) / 2;
        let area = Rect::new(x, y, win_w, win_h);

        // Clear background and draw border.
        surface.clear_with(area, popup_style);
        Widget::render(Block::bordered(), area, surface);

        // Inner area (inside the border).
        let inner = area.inner(Margin::all(1));

        // Remember for scroll management (ensure_visible uses these).
        self.last_inner_width = inner.width;
        self.last_inner_height = inner.height;

        let mut row = inner.y;
        for (i, item) in self.items.iter().enumerate().skip(self.scroll) {
            if row >= inner.y + inner.height {
                break;
            }

            let (prefix, style) = if i == self.selected {
                ("▶ ", selected_style)
            } else {
                ("  ", normal_style)
            };

            let label_lines = wrap_text(&item.label, inner.width.saturating_sub(2));
            for line in &label_lines {
                if row >= inner.y + inner.height {
                    break;
                }
                let text = format!("{}{}", prefix, line);
                surface.set_stringn(inner.x, row, &text, inner.width as usize, style);
                row += 1;
            }

            if let Some(sub) = &item.sublabel {
                let sub_lines = wrap_text(sub, inner.width.saturating_sub(4));
                for line in &sub_lines {
                    if row >= inner.y + inner.height {
                        break;
                    }
                    let text = format!("    {}", line);
                    surface.set_stringn(
                        inner.x,
                        row,
                        &text,
                        inner.width as usize,
                        sublabel_style,
                    );
                    row += 1;
                }
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
                self.ensure_visible();
                EventResult::Consumed(None)
            }
            KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
                self.selected =
                    (self.selected + 1).min(self.items.len().saturating_sub(1));
                self.ensure_visible();
                EventResult::Consumed(None)
            }
            KeyCode::Enter => {
                let idx = self.selected;
                (self.callback)(cx.editor, idx, PromptEvent::Validate);
                let on_close = self.on_close.take();
                EventResult::Consumed(Some(Box::new(move |compositor, cx| {
                    compositor.remove(MultiMenu::ID);
                    if let Some(f) = on_close {
                        f(compositor, cx);
                    }
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
