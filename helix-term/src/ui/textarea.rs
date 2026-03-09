use helix_view::graphics::Rect;
use helix_view::theme::Style;
use tui::buffer::Buffer as Surface;
use helix_core::unicode::segmentation::UnicodeSegmentation;

pub struct TextArea {
    text: String,
    /// Char index (not byte index) of the cursor position.
    cursor: usize,
    /// Index of the first visible VISUAL row (0-based).
    scroll: usize,
    /// Maximum number of visible rows (default 4).
    pub max_lines: usize,
    /// Width passed to the last render(); 0 means wrap disabled.
    area_width: u16,
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
            area_width: 0,
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

    /// Total visual rows when rendered at `width` chars wide.
    pub fn visual_rows_for(&self, width: u16) -> usize {
        let w = width as usize;
        let total: usize = self.text
            .split('\n')
            .map(|line| logical_line_vrows(line.chars().count(), w))
            .sum();
        total.min(self.max_lines)
    }

    /// Number of rows actually visible using the last rendered width.
    pub fn visual_rows(&self) -> usize {
        self.visual_rows_for(self.area_width)
    }

    /// Render text content into `area` on `surface`.
    pub fn render(&mut self, area: Rect, surface: &mut Surface, style: Style) {
        self.area_width = area.width;
        self.ensure_cursor_visible();

        let width = area.width as usize;
        let mut vrow = 0usize;     // global visual row counter
        let mut screen_row = 0u16; // row within area

        'outer: for line in self.text.split('\n') {
            let chars: Vec<char> = line.chars().collect();
            let total = chars.len();
            let vrows = logical_line_vrows(total, width);

            for vr in 0..vrows {
                if vrow >= self.scroll {
                    if screen_row >= area.height {
                        break 'outer;
                    }
                    let start = vr * width;
                    let end = (start + width).min(total);
                    let chunk: String = chars[start..end].iter().collect();
                    surface.set_stringn(area.x, area.y + screen_row, &chunk, width, style);
                    screen_row += 1;
                }
                vrow += 1;
            }
        }
    }

    /// Screen position `(col, row)` of the cursor relative to `area`.
    /// Returns `None` if the cursor is scrolled out of view.
    pub fn cursor_screen_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let (cursor_vrow, cursor_vcol) = self.cursor_visual_row_col();
        if cursor_vrow < self.scroll {
            return None;
        }
        let display_row = cursor_vrow - self.scroll;
        if display_row >= self.visual_rows() {
            return None;
        }
        Some((area.x + cursor_vcol as u16, area.y + display_row as u16))
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

    /// `(visual_row, col_within_visual_row)` for the current cursor.
    fn cursor_visual_row_col(&self) -> (usize, usize) {
        let width = self.area_width as usize;
        let (cursor_lline, cursor_lcol) = self.cursor_line_col();
        let mut vrow = 0usize;
        for (i, line) in self.text.split('\n').enumerate() {
            let chars = line.chars().count();
            if i == cursor_lline {
                let vrow_within = if width == 0 { 0 } else { cursor_lcol / width };
                let vcol = if width == 0 { cursor_lcol } else { cursor_lcol % width };
                return (vrow + vrow_within, vcol);
            }
            vrow += logical_line_vrows(chars, width);
        }
        (0, 0)
    }

    /// Adjust `scroll` so the cursor visual row stays in `[scroll, scroll+max_lines)`.
    fn ensure_cursor_visible(&mut self) {
        let (cursor_vrow, _) = self.cursor_visual_row_col();
        if cursor_vrow < self.scroll {
            self.scroll = cursor_vrow;
        } else if cursor_vrow >= self.scroll + self.max_lines {
            self.scroll = cursor_vrow + 1 - self.max_lines;
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

/// How many visual rows a logical line of `line_chars` chars needs at `width` cols.
fn logical_line_vrows(line_chars: usize, width: usize) -> usize {
    if width == 0 || line_chars == 0 {
        1
    } else {
        (line_chars + width - 1) / width
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
