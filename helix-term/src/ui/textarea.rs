use helix_view::graphics::Rect;
use helix_view::theme::Style;
use tui::buffer::Buffer as Surface;
use helix_core::unicode::segmentation::UnicodeSegmentation;

pub struct TextArea {
    text: String,
    /// Char index (not byte index) of the cursor position.
    cursor: usize,
    /// Index of the first visible line (0-based).
    scroll: usize,
    /// Maximum number of visible rows (default 4).
    pub max_lines: usize,
}

impl Default for TextArea {
    fn default() -> Self {
        Self::new()
    }
}

impl TextArea {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            scroll: 0,
            max_lines: 4,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Reset the widget to empty state.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.scroll = 0;
    }

    // ── Editing ─────────────────────────────────────────────────────────────

    pub fn insert_char(&mut self, c: char) {
        let byte_pos = self.char_to_byte(self.cursor);
        self.text.insert(byte_pos, c);
        self.cursor += 1;
        self.ensure_cursor_visible();
    }

    pub fn insert_str(&mut self, s: &str) {
        let byte_pos = self.char_to_byte(self.cursor);
        self.text.insert_str(byte_pos, s);
        self.cursor += s.chars().count();
        self.ensure_cursor_visible();
    }

    /// Delete the grapheme cluster immediately before the cursor (Backspace).
    pub fn delete_before(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end_byte = self.char_to_byte(self.cursor);
        let text_before = &self.text[..end_byte];
        if let Some(g) = text_before.graphemes(true).last() {
            let g_chars = g.chars().count();
            let g_bytes = g.len();
            let start_byte = end_byte - g_bytes;
            self.text.drain(start_byte..end_byte);
            self.cursor -= g_chars;
        }
        self.ensure_cursor_visible();
    }

    /// Delete the grapheme cluster at the cursor (Delete).
    pub fn delete_after(&mut self) {
        let byte_pos = self.char_to_byte(self.cursor);
        if byte_pos >= self.text.len() {
            return;
        }
        let text_from = &self.text[byte_pos..];
        if let Some(g) = text_from.graphemes(true).next() {
            let end = byte_pos + g.len();
            self.text.drain(byte_pos..end);
        }
    }

    // ── Navigation ───────────────────────────────────────────────────────────

    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end_byte = self.char_to_byte(self.cursor);
        let text_before = &self.text[..end_byte];
        let g_chars = text_before
            .graphemes(true)
            .last()
            .map(|g| g.chars().count())
            .unwrap_or(1);
        self.cursor = self.cursor.saturating_sub(g_chars);
        self.ensure_cursor_visible();
    }

    pub fn move_right(&mut self) {
        let byte_pos = self.char_to_byte(self.cursor);
        if byte_pos >= self.text.len() {
            return;
        }
        let text_from = &self.text[byte_pos..];
        let g_chars = text_from
            .graphemes(true)
            .next()
            .map(|g| g.chars().count())
            .unwrap_or(1);
        self.cursor += g_chars;
        self.ensure_cursor_visible();
    }

    pub fn move_up(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return;
        }
        let lines: Vec<&str> = self.text.split('\n').collect();
        let prev_line_chars = lines[line - 1].chars().count();
        let new_col = col.min(prev_line_chars);
        self.cursor = self.line_col_to_char(line - 1, new_col);
        self.ensure_cursor_visible();
    }

    pub fn move_down(&mut self) {
        let (line, col) = self.cursor_line_col();
        let lines: Vec<&str> = self.text.split('\n').collect();
        if line + 1 >= lines.len() {
            return;
        }
        let next_line_chars = lines[line + 1].chars().count();
        let new_col = col.min(next_line_chars);
        self.cursor = self.line_col_to_char(line + 1, new_col);
        self.ensure_cursor_visible();
    }

    /// Move backward past non-word chars, then past word chars.
    pub fn move_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars_before: Vec<char> = self.text.chars().take(self.cursor).collect();
        let mut j = chars_before.len();
        while j > 0 && !is_word_char(chars_before[j - 1]) {
            j -= 1;
        }
        while j > 0 && is_word_char(chars_before[j - 1]) {
            j -= 1;
        }
        self.cursor = j;
        self.ensure_cursor_visible();
    }

    /// Move forward past word chars, then past non-word chars.
    pub fn move_word_right(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let total = chars.len();
        if self.cursor >= total {
            return;
        }
        let mut j = self.cursor;
        while j < total && is_word_char(chars[j]) {
            j += 1;
        }
        while j < total && !is_word_char(chars[j]) {
            j += 1;
        }
        self.cursor = j;
        self.ensure_cursor_visible();
    }

    // ── Rendering helpers ────────────────────────────────────────────────────

    /// Number of `\n`-separated lines (always at least 1).
    pub fn line_count(&self) -> usize {
        if self.text.is_empty() {
            1
        } else {
            self.text.split('\n').count()
        }
    }

    /// Number of rows actually visible: `min(line_count(), max_lines)`.
    pub fn visual_rows(&self) -> usize {
        self.line_count().min(self.max_lines)
    }

    /// Render text content into `area` on `surface`.
    pub fn render(&mut self, area: Rect, surface: &mut Surface, style: Style) {
        self.ensure_cursor_visible();
        let lines: Vec<&str> = self.text.split('\n').collect();
        let rows = self.visual_rows();
        for row in 0..rows {
            let line_idx = self.scroll + row;
            if line_idx >= lines.len() {
                break;
            }
            surface.set_stringn(
                area.x,
                area.y + row as u16,
                lines[line_idx],
                area.width as usize,
                style,
            );
        }
    }

    /// Screen position `(col, row)` of the cursor relative to `area`.
    /// Returns `None` if the cursor line is scrolled out of view.
    pub fn cursor_screen_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let (cursor_line, cursor_col) = self.cursor_line_col();
        if cursor_line < self.scroll {
            return None;
        }
        let display_row = cursor_line - self.scroll;
        if display_row >= self.visual_rows() {
            return None;
        }
        Some((
            area.x + cursor_col as u16,
            area.y + display_row as u16,
        ))
    }

    // ── Public position query ────────────────────────────────────────────────

    /// Returns `(line_index, col_char_offset)` of the cursor.
    pub fn cursor_position(&self) -> (usize, usize) {
        self.cursor_line_col()
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /// `(line_index, col_char_offset)` of the cursor.
    fn cursor_line_col(&self) -> (usize, usize) {
        let byte_pos = self.char_to_byte(self.cursor);
        let text_before = &self.text[..byte_pos];
        let parts: Vec<&str> = text_before.split('\n').collect();
        let line = parts.len() - 1;
        let col = parts.last().map(|s| s.chars().count()).unwrap_or(0);
        (line, col)
    }

    /// Adjust `scroll` so the cursor line stays in `[scroll, scroll+max_lines)`.
    fn ensure_cursor_visible(&mut self) {
        let (cursor_line, _) = self.cursor_line_col();
        if cursor_line < self.scroll {
            self.scroll = cursor_line;
        } else if cursor_line >= self.scroll + self.max_lines {
            self.scroll = cursor_line + 1 - self.max_lines;
        }
    }

    /// Convert a char index to a byte index in `self.text`.
    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    /// Convert a `(line, col)` pair to a char index.
    fn line_col_to_char(&self, line: usize, col: usize) -> usize {
        let mut char_idx = 0usize;
        for (i, l) in self.text.split('\n').enumerate() {
            if i == line {
                return char_idx + col.min(l.chars().count());
            }
            char_idx += l.chars().count() + 1; // +1 for the '\n'
        }
        char_idx
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
