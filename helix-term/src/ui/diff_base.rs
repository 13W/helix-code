use helix_view::graphics::{Margin, Rect, Style};
use tui::{
    buffer::Buffer as Surface,
    text::{Span, Spans, Text},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::{
    compositor::{Component, Context, Event, EventResult},
    ui::markdown::highlighted_code_block,
};

const CONTEXT_LINES: usize = 3;

pub struct DiffBaseView {
    base_text: String,
    doc_text: String,
    language: String,
    hunks: Vec<helix_vcs::Hunk>,
}

impl DiffBaseView {
    pub const ID: &'static str = "diff-base";

    pub fn new(
        base_text: String,
        doc_text: String,
        language: String,
        hunks: Vec<helix_vcs::Hunk>,
    ) -> Self {
        Self {
            base_text,
            doc_text,
            language,
            hunks,
        }
    }

    fn build_text(&self, cx: &mut Context) -> Text<'static> {
        let theme = &cx.editor.theme;
        let loader = &cx.editor.syn_loader.load();

        let base_hl = highlighted_code_block(&self.base_text, &self.language, Some(theme), loader, None);
        let base_lines: Vec<Spans<'static>> = base_hl.lines;
        let base_line_count = base_lines.len();

        // Fall back to showing the full highlighted base if no hunks
        if self.hunks.is_empty() {
            return Text::from(base_lines);
        }

        let doc_hl = highlighted_code_block(&self.doc_text, &self.language, Some(theme), loader, None);
        let doc_lines: Vec<Spans<'static>> = doc_hl.lines;

        let minus_style = theme.get("diff.minus");
        let plus_style = theme.get("diff.plus");
        let header_style = theme.get("diff.delta");

        let mut result: Vec<Spans<'static>> = Vec::new();

        for hunk in &self.hunks {
            // Hunk header
            let before_len = hunk.before.end.saturating_sub(hunk.before.start);
            let after_len = hunk.after.end.saturating_sub(hunk.after.start);
            let header = format!(
                "@@ -{},{} +{},{} @@",
                hunk.before.start + 1,
                before_len,
                hunk.after.start + 1,
                after_len,
            );
            result.push(Spans::from(Span::styled(header, header_style)));

            // Context before
            let ctx_start = (hunk.before.start as usize).saturating_sub(CONTEXT_LINES);
            for i in ctx_start..hunk.before.start as usize {
                if let Some(line) = base_lines.get(i) {
                    result.push(prepend_prefix(line.clone(), " "));
                }
            }

            // Removed lines (base)
            for i in hunk.before.start as usize..hunk.before.end as usize {
                if let Some(line) = base_lines.get(i) {
                    result.push(apply_diff_style(line.clone(), "-", minus_style));
                }
            }

            // Added lines (current doc)
            for i in hunk.after.start as usize..hunk.after.end as usize {
                if let Some(line) = doc_lines.get(i) {
                    result.push(apply_diff_style(line.clone(), "+", plus_style));
                }
            }

            // Context after
            let ctx_end = (hunk.before.end as usize + CONTEXT_LINES).min(base_line_count);
            for i in hunk.before.end as usize..ctx_end {
                if let Some(line) = base_lines.get(i) {
                    result.push(prepend_prefix(line.clone(), " "));
                }
            }
        }

        Text::from(result)
    }

    fn count_lines(&self) -> u16 {
        if self.hunks.is_empty() {
            return self.base_text.lines().count() as u16;
        }
        let base_line_count = self.base_text.lines().count();
        let mut count: u16 = 0;
        for hunk in &self.hunks {
            count += 1; // header
            let ctx_before = (hunk.before.start as usize).saturating_sub(CONTEXT_LINES);
            count += (hunk.before.start as usize - ctx_before) as u16;
            count += (hunk.before.end - hunk.before.start) as u16;
            count += (hunk.after.end - hunk.after.start) as u16;
            let ctx_end = (hunk.before.end as usize + CONTEXT_LINES).min(base_line_count);
            count += (ctx_end - hunk.before.end as usize) as u16;
        }
        count
    }
}

/// Prepend a plain prefix character (space for context lines).
fn prepend_prefix(mut spans: Spans<'static>, prefix: &'static str) -> Spans<'static> {
    spans.0.insert(0, Span::raw(prefix));
    spans
}

/// Prepend a `+`/`-` prefix and patch every span's style with the diff background.
fn apply_diff_style(spans: Spans<'static>, prefix: &'static str, diff_style: Style) -> Spans<'static> {
    let mut new_spans: Vec<Span<'static>> = Vec::with_capacity(spans.0.len() + 1);
    new_spans.push(Span::styled(prefix, diff_style));
    for span in spans.0 {
        new_spans.push(Span::styled(span.content, diff_style.patch(span.style)));
    }
    Spans::from(new_spans)
}

impl Component for DiffBaseView {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let margin = Margin::all(1);
        let inner = area.inner(margin);
        let text = self.build_text(cx);
        Paragraph::new(&text)

            .wrap(Wrap { trim: false })
            .scroll((cx.scroll.unwrap_or_default() as u16, 0))
            .render(inner, surface);
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let max_width = viewport.0.saturating_sub(4).min(120);
        let line_count = self.count_lines();
        Some((max_width + 4, line_count + 4))
    }

    fn handle_event(&mut self, _event: &Event, _ctx: &mut Context) -> EventResult {
        EventResult::Ignored(None)
    }
}
