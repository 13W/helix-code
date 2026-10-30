use helix_core::doc_formatter::FormattedGrapheme;
use helix_core::line_ending::line_end_char_index;
use helix_core::text_annotations::Conceal;
use helix_core::{Position, RopeSlice};
use helix_view::theme::Style;
use helix_view::{Document, Theme, View};

use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::text_decorations::Decoration;

/// Writes how many lines the closed folds on a visual line hide after its end,
/// for example `fn main() {…}  4 lines`.
pub struct FoldSummary<'a> {
    text: RopeSlice<'a>,
    folds: &'a [Conceal],
    /// Index of the next fold whose ellipsis has not been rendered yet.
    next: usize,
    /// Lines hidden by the folds rendered on the current visual line.
    hidden_lines: usize,
    style: Style,
}

impl<'a> FoldSummary<'a> {
    pub fn new(doc: &'a Document, view: &View, theme: &Theme) -> Self {
        Self {
            text: doc.text().slice(..),
            folds: doc.folded_ranges(view.id),
            next: 0,
            hidden_lines: 0,
            style: theme.get("ui.virtual.fold"),
        }
    }

    fn next_anchor(&self) -> usize {
        self.folds
            .get(self.next)
            .map_or(usize::MAX, |fold| fold.range.start)
    }

    /// Document lines whose content the fold hides completely. A closing token
    /// left visible after the ellipsis keeps its line out of the count.
    fn hidden_lines(&self, fold: &Conceal) -> usize {
        let start_line = self.text.char_to_line(fold.range.start);
        let end_line = self.text.char_to_line(fold.range.end);
        let end_line_hidden = fold.range.end == line_end_char_index(&self.text, end_line);
        (end_line - start_line - 1) + usize::from(end_line_hidden)
    }
}

impl Decoration for FoldSummary<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        self.next = self.folds.partition_point(|fold| fold.range.start < pos);
        self.hidden_lines = 0;
        self.next_anchor()
    }

    fn skip_concealed_anchor(&mut self, pos: usize) -> usize {
        // Unlike `reset_pos`, keep the count gathered on the current line.
        self.next = self.folds.partition_point(|fold| fold.range.start < pos);
        self.next_anchor()
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        _grapheme: &FormattedGrapheme,
    ) -> usize {
        if let Some(fold) = self.folds.get(self.next) {
            self.hidden_lines += self.hidden_lines(fold);
            self.next += 1;
        }
        self.next_anchor()
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let hidden_lines = std::mem::take(&mut self.hidden_lines);
        if hidden_lines == 0 {
            return Position::new(0, 0);
        }
        let summary = if hidden_lines == 1 {
            "1 line".to_string()
        } else {
            format!("{hidden_lines} lines")
        };
        // Leave one blank cell after the line, like end-of-line diagnostics.
        let Some(col) = (virt_off.col + 1).checked_sub(renderer.offset.col) else {
            return Position::new(0, 0);
        };
        let width = renderer.viewport.width as usize;
        if col >= width {
            return Position::new(0, 0);
        }
        renderer.set_stringn(
            renderer.viewport.x + col as u16,
            pos.visual_line,
            &summary,
            width - col,
            self.style,
        );
        Position::new(0, summary.len() + 1)
    }
}
