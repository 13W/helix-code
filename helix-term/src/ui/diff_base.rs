use helix_view::graphics::{Margin, Rect, Style};
use tui::{
    buffer::Buffer as Surface,
    text::Spans,
    widgets::{Block, Widget},
};

use crate::{
    compositor::{Callback, Component, Context, Event, EventResult},
    ctrl, key,
    ui::markdown::highlighted_code_block,
};

const CONTEXT_LINES: usize = 3;

// ─── Shared row model ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum RowKind {
    Context,
    Removed,
    Added,
    /// 1-to-1 paired modification (left = before, right = after).
    Modified,
}

#[derive(Clone)]
struct DiffRow {
    left: Option<(usize, RowKind)>,
    right: Option<(usize, RowKind)>,
}

// ─── DiffHunkView ─────────────────────────────────────────────────────────────
//
// Simple single-panel popup that shows the original (base) text of the hunk
// at the cursor. Used by `space+K` (`show_diff_base`).

pub struct DiffHunkView {
    language: String,
    /// The original lines to display (from diff base, scoped to the hunk).
    base_hunk_text: String,
}

impl DiffHunkView {
    pub const ID: &'static str = "diff-hunk";

    pub fn new(language: String, base_hunk_text: String) -> Self {
        Self {
            language,
            base_hunk_text,
        }
    }
}

impl Component for DiffHunkView {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let theme = &cx.editor.theme;
        let loader = cx.editor.syn_loader.load();
        let popup_style = theme.get("ui.popup");

        surface.clear_with(area, popup_style);
        Widget::render(Block::bordered(), area, surface);

        let inner = area.inner(Margin::all(1));
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let hl =
            highlighted_code_block(&self.base_hunk_text, &self.language, Some(theme), &loader, None);
        let lines: Vec<Spans<'static>> = hl.lines;

        for (i, spans) in lines.iter().enumerate().take(inner.height as usize) {
            let y = inner.y + i as u16;
            let mut cur_x = inner.x;
            let end_x = inner.x + inner.width;
            for span in &spans.0 {
                let remaining = end_x.saturating_sub(cur_x) as usize;
                if remaining == 0 {
                    break;
                }
                let style = popup_style.patch(span.style);
                let (next_x, _) =
                    surface.set_stringn(cur_x, y, span.content.as_ref(), remaining, style);
                cur_x = next_x;
            }
        }
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let line_count = self.base_hunk_text.lines().count() as u16;
        let max_line_len = self
            .base_hunk_text
            .lines()
            .map(|l| l.len() as u16)
            .max()
            .unwrap_or(40);
        let w = (max_line_len + 4).min(viewport.0.saturating_sub(4)).min(120);
        let h = (line_count + 2).min(viewport.1.saturating_sub(4));
        Some((w, h))
    }

    fn handle_event(&mut self, _event: &Event, _cx: &mut Context) -> EventResult {
        EventResult::Ignored(None)
    }

    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }
}

// ─── DiffBaseView ─────────────────────────────────────────────────────────────
//
// IntelliJ-style side-by-side diff: original on the left, modified on the right.
// Used by `show_diff_view`.
//
// Two display modes:
//   `full_file = false`  — show only the hunk(s) passed in, with CONTEXT_LINES
//                          of surrounding context (default for per-hunk use).
//   `full_file = true`   — show the entire file; changed sections are highlighted.

pub struct DiffBaseView {
    base_text: String,
    doc_text: String,
    language: String,
    hunks: Vec<helix_vcs::Hunk>,
    full_file: bool,
    scroll_offset: usize,
    cursor_line: usize,
    /// Content-area height from the last render, used to size page scrolls.
    last_height: usize,
}

impl DiffBaseView {
    pub const ID: &'static str = "diff-base";

    /// Per-hunk side-by-side view (shows only the passed hunks with context).
    pub fn new(
        base_text: String,
        doc_text: String,
        language: String,
        hunks: Vec<helix_vcs::Hunk>,
        cursor_line: usize,
    ) -> Self {
        Self {
            base_text,
            doc_text,
            language,
            hunks,
            full_file: false,
            scroll_offset: 0,
            cursor_line,
            last_height: 20,
        }
    }

    /// Full-file side-by-side diff.  Initial scroll is set so that
    /// `cursor_line` is visible near the top of the viewport.
    pub fn new_full_file(
        base_text: String,
        doc_text: String,
        language: String,
        hunks: Vec<helix_vcs::Hunk>,
        cursor_line: usize,
    ) -> Self {
        Self {
            base_text,
            doc_text,
            language,
            hunks,
            full_file: true,
            scroll_offset: cursor_line.saturating_sub(3),
            cursor_line,
            last_height: 20,
        }
    }

    fn build_rows(&self) -> Vec<DiffRow> {
        if self.full_file {
            self.build_rows_full_file()
        } else {
            self.build_rows_hunk_only()
        }
    }

    /// Build rows only for the listed hunks (with CONTEXT_LINES around each).
    fn build_rows_hunk_only(&self) -> Vec<DiffRow> {
        let base_line_count = self.base_text.lines().count();
        let doc_line_count = self.doc_text.lines().count();

        if self.hunks.is_empty() {
            // No hunk at cursor — show a context window centred on cursor_line.
            let start = self.cursor_line.saturating_sub(5);
            let end = (self.cursor_line + 6)
                .min(base_line_count)
                .min(doc_line_count);
            return (start..end)
                .map(|i| DiffRow {
                    left: Some((i, RowKind::Context)),
                    right: Some((i, RowKind::Context)),
                })
                .collect();
        }

        let mut rows = Vec::new();
        for hunk in &self.hunks {
            let before_start = hunk.before.start as usize;
            let before_end = hunk.before.end as usize;
            let after_start = hunk.after.start as usize;
            let after_end = hunk.after.end as usize;
            let before_len = before_end - before_start;
            let after_len = after_end - after_start;

            let ctx_start = before_start.saturating_sub(CONTEXT_LINES);
            let ctx_count = before_start - ctx_start;
            let doc_ctx_start = after_start.saturating_sub(ctx_count);
            for j in 0..ctx_count {
                rows.push(DiffRow {
                    left: Some((ctx_start + j, RowKind::Context)),
                    right: Some((doc_ctx_start + j, RowKind::Context)),
                });
            }

            if before_len > 0 && before_len == after_len {
                for j in 0..before_len {
                    rows.push(DiffRow {
                        left: Some((before_start + j, RowKind::Modified)),
                        right: Some((after_start + j, RowKind::Modified)),
                    });
                }
            } else {
                for j in 0..before_len {
                    rows.push(DiffRow {
                        left: Some((before_start + j, RowKind::Removed)),
                        right: None,
                    });
                }
                for j in 0..after_len {
                    rows.push(DiffRow {
                        left: None,
                        right: Some((after_start + j, RowKind::Added)),
                    });
                }
            }

            let ctx_end = (before_end + CONTEXT_LINES).min(base_line_count);
            for j in 0..ctx_end - before_end {
                rows.push(DiffRow {
                    left: Some((before_end + j, RowKind::Context)),
                    right: Some((after_end + j, RowKind::Context)),
                });
            }
        }
        rows
    }

    /// Build rows for the entire file, highlighting changed sections.
    fn build_rows_full_file(&self) -> Vec<DiffRow> {
        let base_line_count = self.base_text.lines().count();
        let doc_line_count = self.doc_text.lines().count();
        let mut rows = Vec::new();
        let mut base_i = 0usize;
        let mut doc_i = 0usize;

        for hunk in &self.hunks {
            let before_start = hunk.before.start as usize;
            let before_end = hunk.before.end as usize;
            let after_start = hunk.after.start as usize;
            let after_end = hunk.after.end as usize;

            // Unchanged lines before this hunk.
            while base_i < before_start && doc_i < after_start {
                rows.push(DiffRow {
                    left: Some((base_i, RowKind::Context)),
                    right: Some((doc_i, RowKind::Context)),
                });
                base_i += 1;
                doc_i += 1;
            }

            let before_len = before_end - before_start;
            let after_len = after_end - after_start;

            if before_len > 0 && before_len == after_len {
                for j in 0..before_len {
                    rows.push(DiffRow {
                        left: Some((before_start + j, RowKind::Modified)),
                        right: Some((after_start + j, RowKind::Modified)),
                    });
                }
            } else {
                for j in 0..before_len {
                    rows.push(DiffRow {
                        left: Some((before_start + j, RowKind::Removed)),
                        right: None,
                    });
                }
                for j in 0..after_len {
                    rows.push(DiffRow {
                        left: None,
                        right: Some((after_start + j, RowKind::Added)),
                    });
                }
            }

            base_i = before_end;
            doc_i = after_end;
        }

        // Remaining unchanged lines after the last hunk.
        let remaining = base_line_count.max(doc_line_count) - base_i.min(base_line_count);
        for j in 0..remaining {
            let left = if base_i + j < base_line_count {
                Some((base_i + j, RowKind::Context))
            } else {
                None
            };
            let right = if doc_i + j < doc_line_count {
                Some((doc_i + j, RowKind::Context))
            } else {
                None
            };
            rows.push(DiffRow { left, right });
        }

        rows
    }
}

// ─── Rendering helpers ────────────────────────────────────────────────────────

fn cell_bg(kind: RowKind, is_left: bool, minus: Style, plus: Style, normal: Style) -> Style {
    match kind {
        RowKind::Context => normal,
        RowKind::Removed => minus,
        RowKind::Added => plus,
        RowKind::Modified => {
            if is_left {
                minus
            } else {
                plus
            }
        }
    }
}

/// Render one panel cell: `[  NNN │ <syntax content>  ]`
fn render_panel_cell(
    surface: &mut Surface,
    x: u16,
    y: u16,
    width: u16,
    cell: Option<(usize, RowKind)>,
    lines: &[Spans<'static>],
    is_left: bool,
    minus_style: Style,
    plus_style: Style,
    linenr_style: Style,
    normal_style: Style,
) {
    if width == 0 {
        return;
    }
    let bg = match cell {
        None => normal_style,
        Some((_, kind)) => cell_bg(kind, is_left, minus_style, plus_style, normal_style),
    };
    // Fill row background.
    surface.set_stringn(x, y, &" ".repeat(width as usize), width as usize, bg);

    let (idx, _) = match cell {
        None => return,
        Some(v) => v,
    };

    // Line number (3 digits right-aligned + space = 4 chars).
    if width >= 4 {
        surface.set_stringn(x, y, &format!("{:>3} ", idx + 1), 4, linenr_style.patch(bg));
    }
    // Gutter "│ " (2 chars).
    if width >= 6 {
        surface.set_stringn(x + 4, y, "│ ", 2, linenr_style.patch(bg));
    }

    let content_x = x + 6;
    if content_x >= x + width {
        return;
    }
    let content_w = (x + width).saturating_sub(content_x) as usize;

    if let Some(spans) = lines.get(idx) {
        let mut cur_x = content_x;
        for span in &spans.0 {
            let remaining = (content_x + content_w as u16).saturating_sub(cur_x) as usize;
            if remaining == 0 {
                break;
            }
            let style = bg.patch(span.style);
            let (next_x, _) =
                surface.set_stringn(cur_x, y, span.content.as_ref(), remaining, style);
            cur_x = next_x;
        }
    }
}

// ─── Component impl ──────────────────────────────────────────────────────────

impl Component for DiffBaseView {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let theme = &cx.editor.theme;
        let loader = cx.editor.syn_loader.load();

        let popup_style = theme.get("ui.popup");
        let title_style = theme.get("ui.statusline.inactive");
        let minus_style = theme.get("diff.minus");
        let plus_style = theme.get("diff.plus");
        let linenr_style = theme.get("ui.linenr");

        let base_hl =
            highlighted_code_block(&self.base_text, &self.language, Some(theme), &loader, None);
        let doc_hl =
            highlighted_code_block(&self.doc_text, &self.language, Some(theme), &loader, None);
        let base_lines: Vec<Spans<'static>> = base_hl.lines;
        let doc_lines: Vec<Spans<'static>> = doc_hl.lines;

        let rows = self.build_rows();
        let total_rows = rows.len();

        surface.clear_with(area, popup_style);
        Widget::render(Block::bordered(), area, surface);

        let inner = area.inner(Margin::all(1));
        if inner.height < 2 || inner.width < 12 {
            return;
        }

        let content_h = inner.height.saturating_sub(1) as usize;
        self.last_height = content_h;

        let panel_w = inner.width / 2;
        let divider_x = inner.x + panel_w;

        // Title row.
        let ty = inner.y;
        surface.set_stringn(
            inner.x,
            ty,
            &format!("{:<width$}", " Original", width = panel_w as usize),
            panel_w as usize,
            title_style,
        );
        surface.set_stringn(divider_x, ty, "│", 1, popup_style);
        let right_label_w = inner.width.saturating_sub(panel_w).saturating_sub(1) as usize;
        surface.set_stringn(
            divider_x + 1,
            ty,
            &format!("{:<width$}", " Modified", width = right_label_w),
            right_label_w,
            title_style,
        );

        // Clamp scroll.
        self.scroll_offset = self
            .scroll_offset
            .min(total_rows.saturating_sub(content_h));

        // Content rows.
        let right_w = inner.width.saturating_sub(panel_w).saturating_sub(1);
        for (i, row) in rows
            .iter()
            .skip(self.scroll_offset)
            .take(content_h)
            .enumerate()
        {
            let y = inner.y + 1 + i as u16;

            render_panel_cell(
                surface,
                inner.x,
                y,
                panel_w,
                row.left,
                &base_lines,
                true,
                minus_style,
                plus_style,
                linenr_style,
                popup_style,
            );
            surface.set_stringn(divider_x, y, "│", 1, popup_style);
            render_panel_cell(
                surface,
                divider_x + 1,
                y,
                right_w,
                row.right,
                &doc_lines,
                false,
                minus_style,
                plus_style,
                linenr_style,
                popup_style,
            );
        }

        // Scrollbar on the right border.
        if total_rows > content_h && content_h > 0 {
            let scroll_style = theme.try_get("ui.menu.scroll").unwrap_or(popup_style);
            let scroll_height =
                ((content_h * content_h) / total_rows.max(1)).clamp(1, content_h);
            let max_offset = total_rows.saturating_sub(content_h);
            let scroll_line = if max_offset > 0 {
                content_h.saturating_sub(scroll_height) * self.scroll_offset / max_offset
            } else {
                0
            };
            for i in 0..content_h {
                let sy = inner.y + 1 + i as u16;
                if sy < inner.bottom() {
                    let cell = &mut surface[(area.right() - 1, sy)];
                    if scroll_line <= i && i < scroll_line + scroll_height {
                        cell.set_symbol("▐");
                        if let Some(fg) = scroll_style.fg {
                            cell.set_fg(fg);
                        }
                    }
                }
            }
        }
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        Some(viewport)
    }

    fn handle_event(&mut self, event: &Event, _cx: &mut Context) -> EventResult {
        let key_event = match event {
            Event::Key(k) => k,
            _ => return EventResult::Ignored(None),
        };

        let page = (self.last_height / 2).max(1);
        let rows = self.build_rows();
        let total = rows.len();
        let max_scroll = total.saturating_sub(self.last_height);

        match key_event {
            key!(Esc) | key!('q') => {
                let close_fn: Callback = Box::new(|compositor, _| {
                    compositor.remove(DiffBaseView::ID);
                });
                EventResult::Consumed(Some(close_fn))
            }
            key!('j') | key!(Down) => {
                self.scroll_offset = self.scroll_offset.saturating_add(1).min(max_scroll);
                EventResult::Consumed(None)
            }
            key!('k') | key!(Up) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                EventResult::Consumed(None)
            }
            key!(PageDown) | ctrl!('d') => {
                self.scroll_offset = self.scroll_offset.saturating_add(page).min(max_scroll);
                EventResult::Consumed(None)
            }
            key!(PageUp) | ctrl!('u') => {
                self.scroll_offset = self.scroll_offset.saturating_sub(page);
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored(None),
        }
    }

    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }
}
