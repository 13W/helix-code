use arc_swap::ArcSwap;
use helix_core::syntax;
use helix_core::unicode::width::{UnicodeWidthChar, UnicodeWidthStr};
use helix_view::graphics::{Margin, Rect};
use helix_view::input::{KeyModifiers, MouseEvent, MouseEventKind};
use std::sync::Arc;
use tui::{
    buffer::Buffer as Surface,
    text::{Span, Spans},
    widgets::{Block, Widget},
};

use crate::{
    compositor::{Callback, Component, Context, Event, EventResult},
    ctrl, key,
    ui::Markdown,
};

// ─── MarkdownPreview ─────────────────────────────────────────────────────────
//
// Full-window reading view that renders the current document as markdown
// (headings, emphasis, lists, tables, syntax-highlighted code blocks) using the
// shared `ui::Markdown` renderer. Opened by `markdown_preview` (`space m`).
//
// Prose lines are word-wrapped to the view width, but preformatted lines — table
// borders/rows and horizontal rules — are left intact (word-wrapping them mangles
// the box-drawing). Wide tables are truncated at the right edge instead.
//
// Self-contained: scrolls with j/k, mouse wheel, PageUp/PageDown, Ctrl-u/Ctrl-d
// and closes on Esc/q — mirrors the `DiffBaseView` component.

/// How many rows a single mouse-wheel notch scrolls.
const MOUSE_SCROLL_LINES: usize = 3;

pub struct MarkdownPreview {
    contents: String,
    syn_loader: Arc<ArcSwap<syntax::Loader>>,
    scroll_offset: usize,
    /// Horizontal scroll offset in columns (for content wider than the view,
    /// e.g. wide tables that must not be wrapped).
    h_scroll: usize,
    /// Visual (already-wrapped) lines, cached for `cached_width`.
    visual_lines: Vec<Spans<'static>>,
    /// Width in columns of the widest visual line (for horizontal scroll clamp).
    max_line_width: usize,
    /// Content width the `visual_lines` cache was built for (0 = not built yet).
    cached_width: u16,
    /// Content-area height from the last render, used to size page scrolls.
    last_height: usize,
}

/// Columns a single horizontal-scroll step moves.
const H_SCROLL_STEP: usize = 8;

impl MarkdownPreview {
    pub const ID: &'static str = "markdown-preview";

    pub fn new(contents: String, syn_loader: Arc<ArcSwap<syntax::Loader>>) -> Self {
        Self {
            contents,
            syn_loader,
            scroll_offset: 0,
            h_scroll: 0,
            visual_lines: Vec::new(),
            max_line_width: 0,
            cached_width: 0,
            last_height: 20,
        }
    }

    fn max_scroll(&self) -> usize {
        self.visual_lines.len().saturating_sub(self.last_height)
    }

    fn max_h_scroll(&self) -> usize {
        self.max_line_width.saturating_sub(self.cached_width as usize)
    }
}

fn line_width(line: &Spans) -> usize {
    line.0.iter().map(|s| s.content.as_ref().width()).sum()
}

/// Return the tail of `content` after dropping `cols` display columns.
/// Returns `""` if the whole string is within the dropped range. If a wide
/// character straddles the boundary the next full character is kept.
fn drop_columns(content: &str, cols: usize) -> &str {
    if cols == 0 {
        return content;
    }
    let mut acc = 0usize;
    for (bi, ch) in content.char_indices() {
        if acc >= cols {
            return &content[bi..];
        }
        acc += ch.width().unwrap_or(0);
    }
    ""
}

/// Lines beginning with one of these characters are preformatted (table borders,
/// table rows or a horizontal rule) and must not be word-wrapped.
fn is_protected(line: &Spans) -> bool {
    let first = line
        .0
        .iter()
        .flat_map(|s| s.content.chars())
        .find(|c| !c.is_whitespace());
    matches!(
        first,
        Some('┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼' | '│' | '─')
    )
}

fn to_static(line: &Spans) -> Spans<'static> {
    Spans::from(
        line.0
            .iter()
            .map(|s| Span::styled(s.content.to_string(), s.style))
            .collect::<Vec<_>>(),
    )
}

/// Word-wrap a styled line to `width` columns, preserving each span's style.
/// Leading whitespace is kept on the first visual line (indentation) but dropped
/// at the start of continuation lines. Words longer than `width` are hard-split.
fn wrap_spans(line: &Spans, width: usize) -> Vec<Spans<'static>> {
    let width = width.max(1);
    let mut out: Vec<Spans<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    let mut first_line = true;

    for span in &line.0 {
        let style = span.style;
        let mut chars = span.content.chars().peekable();
        while let Some(&c) = chars.peek() {
            let is_space = c == ' ' || c == '\t';
            // Collect a run of same-class characters (all-space or all-non-space).
            let mut token = String::new();
            while let Some(&c2) = chars.peek() {
                if (c2 == ' ' || c2 == '\t') != is_space {
                    break;
                }
                token.push(c2);
                chars.next();
            }

            if is_space {
                // Drop whitespace at the start of a continuation line.
                if cur_w == 0 && !first_line {
                    continue;
                }
                for _ in 0..token.chars().count() {
                    if cur_w + 1 > width {
                        out.push(Spans::from(std::mem::take(&mut cur)));
                        cur_w = 0;
                        first_line = false;
                        break; // collapse the wrap-point whitespace
                    }
                    cur.push(Span::styled(" ".to_string(), style));
                    cur_w += 1;
                }
            } else {
                let word_w = token.as_str().width();
                if word_w <= width.saturating_sub(cur_w) {
                    cur.push(Span::styled(token, style));
                    cur_w += word_w;
                } else if word_w <= width {
                    // Fits on a line of its own — wrap first, then place it.
                    out.push(Spans::from(std::mem::take(&mut cur)));
                    first_line = false;
                    cur.push(Span::styled(token, style));
                    cur_w = word_w;
                } else {
                    // Longer than a whole line — hard-split by column width.
                    let mut chunk = String::new();
                    let mut chunk_w = 0usize;
                    for ch in token.chars() {
                        let cw = ch.width().unwrap_or(0);
                        if cur_w + chunk_w + cw > width {
                            if !chunk.is_empty() {
                                cur.push(Span::styled(std::mem::take(&mut chunk), style));
                                chunk_w = 0;
                            }
                            out.push(Spans::from(std::mem::take(&mut cur)));
                            cur_w = 0;
                            first_line = false;
                        }
                        chunk.push(ch);
                        chunk_w += cw;
                    }
                    if !chunk.is_empty() {
                        cur.push(Span::styled(chunk, style));
                        cur_w += chunk_w;
                    }
                }
            }
        }
    }
    out.push(Spans::from(cur));
    out
}

impl Component for MarkdownPreview {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let theme = &cx.editor.theme;
        let popup_style = theme.get("ui.popup");
        let title_style = theme.get("ui.statusline.inactive");

        surface.clear_with(area, popup_style);
        Widget::render(Block::bordered(), area, surface);

        let inner = area.inner(Margin::all(1));
        if inner.height < 2 || inner.width < 4 {
            return;
        }

        // Title row.
        let title =
            " Markdown preview  (Esc/q: close · j/k, wheel, C-u/C-d: scroll · h/l: pan wide tables)";
        surface.set_stringn(
            inner.x,
            inner.y,
            &format!("{:<width$}", title, width = inner.width as usize),
            inner.width as usize,
            title_style,
        );

        // Content area below the title.
        let content_area = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
        let content_h = content_area.height as usize;
        self.last_height = content_h;

        // (Re)build the wrapped visual lines when the width changes.
        if self.cached_width != content_area.width {
            let md = Markdown::new(self.contents.clone(), self.syn_loader.clone())
                .with_max_table_width(content_area.width as usize);
            let text = md.parse(Some(theme));
            let width = content_area.width as usize;
            let mut vlines: Vec<Spans<'static>> = Vec::new();
            for line in &text.lines {
                if is_protected(line) {
                    vlines.push(to_static(line));
                } else {
                    vlines.extend(wrap_spans(line, width));
                }
            }
            self.max_line_width = vlines.iter().map(line_width).max().unwrap_or(0);
            self.visual_lines = vlines;
            self.cached_width = content_area.width;
        }

        // Clamp both scroll axes.
        let max_scroll = self.visual_lines.len().saturating_sub(content_h);
        if self.scroll_offset > max_scroll {
            self.scroll_offset = max_scroll;
        }
        let max_h = self.max_line_width.saturating_sub(content_area.width as usize);
        if self.h_scroll > max_h {
            self.h_scroll = max_h;
        }

        // Render the visible visual lines, applying the horizontal offset and
        // truncating each at the right edge (so the window border is preserved).
        let end_x = content_area.x + content_area.width;
        for i in 0..content_h {
            let idx = self.scroll_offset + i;
            let Some(spans) = self.visual_lines.get(idx) else {
                break;
            };
            let y = content_area.y + i as u16;
            let mut cur_x = content_area.x;
            let mut skip = self.h_scroll;
            for span in &spans.0 {
                let content_str = span.content.as_ref();
                let visible = drop_columns(content_str, skip);
                skip = skip.saturating_sub(content_str.width());
                if visible.is_empty() {
                    continue;
                }
                let remaining = end_x.saturating_sub(cur_x) as usize;
                if remaining == 0 {
                    break;
                }
                let style = popup_style.patch(span.style);
                let (next_x, _) = surface.set_stringn(cur_x, y, visible, remaining, style);
                cur_x = next_x;
            }
        }

        // Scrollbar on the right border (mirrors DiffBaseView).
        let total = self.visual_lines.len();
        if total > content_h && content_h > 0 {
            let scroll_style = theme.try_get("ui.menu.scroll").unwrap_or(popup_style);
            let scroll_height = ((content_h * content_h) / total.max(1)).clamp(1, content_h);
            let scroll_line = if max_scroll > 0 {
                content_h.saturating_sub(scroll_height) * self.scroll_offset / max_scroll
            } else {
                0
            };
            for i in 0..content_h {
                let sy = content_area.y + i as u16;
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
        let max_scroll = self.max_scroll();
        let page = (self.last_height / 2).max(1);

        let pan_right = |this: &mut Self| {
            this.h_scroll = this
                .h_scroll
                .saturating_add(H_SCROLL_STEP)
                .min(this.max_h_scroll());
        };
        let pan_left = |this: &mut Self| {
            this.h_scroll = this.h_scroll.saturating_sub(H_SCROLL_STEP);
        };

        match event {
            Event::Mouse(MouseEvent {
                kind, modifiers, ..
            }) => {
                let shift = modifiers.contains(KeyModifiers::SHIFT);
                match kind {
                    // Horizontal wheel / trackpad, or Shift + vertical wheel.
                    MouseEventKind::ScrollRight => pan_right(self),
                    MouseEventKind::ScrollLeft => pan_left(self),
                    MouseEventKind::ScrollDown if shift => pan_right(self),
                    MouseEventKind::ScrollUp if shift => pan_left(self),
                    MouseEventKind::ScrollDown => {
                        self.scroll_offset = self
                            .scroll_offset
                            .saturating_add(MOUSE_SCROLL_LINES)
                            .min(max_scroll);
                    }
                    MouseEventKind::ScrollUp => {
                        self.scroll_offset =
                            self.scroll_offset.saturating_sub(MOUSE_SCROLL_LINES);
                    }
                    // Consume other mouse events so they don't fall through.
                    _ => {}
                }
                EventResult::Consumed(None)
            }
            Event::Key(key_event) => match key_event {
                key!(Esc) | key!('q') => {
                    let close_fn: Callback = Box::new(|compositor, _| {
                        compositor.remove(MarkdownPreview::ID);
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
                key!('g') => {
                    self.scroll_offset = 0;
                    EventResult::Consumed(None)
                }
                key!('G') => {
                    self.scroll_offset = max_scroll;
                    EventResult::Consumed(None)
                }
                key!('l') | key!(Right) => {
                    self.h_scroll = self
                        .h_scroll
                        .saturating_add(H_SCROLL_STEP)
                        .min(self.max_h_scroll());
                    EventResult::Consumed(None)
                }
                key!('h') | key!(Left) => {
                    self.h_scroll = self.h_scroll.saturating_sub(H_SCROLL_STEP);
                    EventResult::Consumed(None)
                }
                key!('0') => {
                    self.h_scroll = 0;
                    EventResult::Consumed(None)
                }
                key!('$') => {
                    self.h_scroll = self.max_h_scroll();
                    EventResult::Consumed(None)
                }
                key!(PageDown) | ctrl!('d') | ctrl!('f') => {
                    self.scroll_offset = self.scroll_offset.saturating_add(page).min(max_scroll);
                    EventResult::Consumed(None)
                }
                key!(PageUp) | ctrl!('u') | ctrl!('b') => {
                    self.scroll_offset = self.scroll_offset.saturating_sub(page);
                    EventResult::Consumed(None)
                }
                _ => EventResult::Ignored(None),
            },
            _ => EventResult::Ignored(None),
        }
    }

    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> Spans<'static> {
        Spans::from(Span::raw(s.to_string()))
    }

    fn text_of(line: &Spans) -> String {
        line.0.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn protects_table_and_rule_lines() {
        assert!(is_protected(&plain("┌──────┬──────┐")));
        assert!(is_protected(&plain("│ Col1 │ Col2 │")));
        assert!(is_protected(&plain("├──────┼──────┤")));
        assert!(is_protected(&plain("───")));
        assert!(!is_protected(&plain("regular paragraph text")));
        assert!(!is_protected(&plain("")));
    }

    #[test]
    fn wide_table_row_is_not_wrapped() {
        // A table row far wider than the width must stay a single visual line.
        let row = "│ Column Alpha │ Column Beta │ Column Gamma │ Column Delta │";
        assert!(is_protected(&plain(row)));
        // Protected lines bypass wrap_spans entirely, so `to_static` keeps them intact.
        let kept = to_static(&plain(row));
        assert_eq!(text_of(&kept), row);
    }

    #[test]
    fn long_prose_wraps_to_width() {
        let prose = plain("the quick brown fox jumps over the lazy dog again and again");
        let wrapped = wrap_spans(&prose, 20);
        assert!(wrapped.len() > 1, "expected multiple wrapped lines");
        for line in &wrapped {
            assert!(
                text_of(line).width() <= 20,
                "line exceeds width: {:?}",
                text_of(line)
            );
        }
        // No content is lost (word order preserved, whitespace-insensitive).
        let joined: String = wrapped
            .iter()
            .flat_map(|l| l.0.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        for word in ["quick", "brown", "lazy", "again"] {
            assert!(joined.contains(word), "missing word {word}");
        }
    }

    #[test]
    fn overlong_word_is_hard_split() {
        let word = plain("supercalifragilisticexpialidocious");
        let wrapped = wrap_spans(&word, 10);
        assert!(wrapped.len() >= 4);
        for line in &wrapped {
            assert!(text_of(line).width() <= 10);
        }
    }

    #[test]
    fn empty_line_preserved() {
        let wrapped = wrap_spans(&plain(""), 20);
        assert_eq!(wrapped.len(), 1);
        assert_eq!(text_of(&wrapped[0]), "");
    }

    #[test]
    fn drop_columns_pans_horizontally() {
        assert_eq!(drop_columns("┌────┬────┐", 0), "┌────┬────┐");
        assert_eq!(drop_columns("┌────┬────┐", 5), "┬────┐");
        assert_eq!(drop_columns("abcdef", 3), "def");
        assert_eq!(drop_columns("abc", 3), "");
        assert_eq!(drop_columns("abc", 10), "");
        // Wide (2-column) chars: dropping 1 col of a 2-col char keeps the next char.
        assert_eq!(drop_columns("あい", 2), "い");
    }

    /// End-to-end: parse a document with a wide table + long prose, build the
    /// visual lines the way `render` does, and assert prose wraps while the table
    /// stays intact on single lines.
    #[test]
    fn render_pipeline_wraps_prose_keeps_tables() {
        let loader = Arc::new(ArcSwap::from_pointee(
            helix_core::config::default_lang_loader(),
        ));
        let src = "\
# Heading

This is a fairly long prose paragraph that should be word-wrapped across several visual lines when the view is narrow.

| Column Alpha | Column Beta | Column Gamma | Column Delta |
| ------------ | ----------- | ------------ | ------------ |
| a1 value     | b1 value    | c1 value     | d1 value     |
";
        let md = Markdown::new(src.to_string(), loader);
        let text = md.parse(None);

        let width = 30usize;
        let mut vlines: Vec<Spans<'static>> = Vec::new();
        for line in &text.lines {
            if is_protected(line) {
                vlines.push(to_static(line));
            } else {
                vlines.extend(wrap_spans(line, width));
            }
        }

        // Prose wrapped to width; table rows kept whole (wider than `width`).
        assert!(vlines.iter().any(|l| !is_protected(l) && text_of(l).contains("word-wrapped")));
        for l in &vlines {
            if is_protected(l) {
                assert!(text_of(l).width() > width, "table row should stay intact");
            } else {
                assert!(text_of(l).width() <= width, "prose must fit the width");
            }
        }
    }
}
