//! Closed folds belong to a document/view pair, never to the text or syntax tree.
//!
//! Ported from Mitos (mitos-editor/mitos#651).

use super::Document;
use crate::ViewId;
use helix_core::{
    line_ending::line_end_char_index, text_annotations::Conceal, Assoc, ChangeSet, Range,
    RopeSlice, Selection,
};

#[derive(Debug, Clone, Default)]
pub(super) struct Folds {
    /// Sorted outermost first. Closed descendants survive reopening a parent.
    closed: Vec<Conceal>,
    visible: Vec<Conceal>,
}

impl Folds {
    fn rebuild(&mut self) {
        self.closed
            .sort_unstable_by_key(|c| (c.range.start, std::cmp::Reverse(c.range.end)));
        self.closed.dedup();
        // Keep a laminar set even for crossing injection/selection ranges. Sorting
        // and a stack make folding an entire large file O(n log n).
        let mut ends = Vec::new();
        self.closed.retain(|fold| {
            while ends.last().is_some_and(|end| *end < fold.range.start) {
                ends.pop();
            }
            if ends.last().is_some_and(|end| *end < fold.range.end) {
                return false;
            }
            ends.push(fold.range.end);
            true
        });
        self.visible.clear();
        for fold in &self.closed {
            if self
                .visible
                .last()
                .is_none_or(|parent| parent.range.end < fold.range.start)
            {
                self.visible.push(fold.clone());
            }
        }
    }

    /// Open every fold that would hide a cursor of `selection`.
    pub(super) fn reveal(&mut self, selection: &Selection, text: RopeSlice) {
        let len = self.closed.len();
        self.closed.retain(|fold| {
            !selection.iter().any(|r| {
                let cursor = r.cursor(text);
                fold.range.start < cursor && cursor < fold.range.end
            })
        });
        if len != self.closed.len() {
            self.rebuild();
        }
    }

    pub(super) fn apply_changes(&mut self, changes: &ChangeSet) {
        self.closed.retain_mut(|fold| {
            // An edit touching concealed text opens it in every view. Changes
            // elsewhere track the same text, including undo and external edits.
            if changes
                .changes_iter()
                .any(|(start, end, _)| start <= fold.range.end && end >= fold.range.start)
            {
                return false;
            }
            fold.range.start = changes.map_pos(fold.range.start, Assoc::After);
            fold.range.end = changes.map_pos(fold.range.end, Assoc::Before);
            fold.range.start < fold.range.end
        });
        self.rebuild();
    }
}

/// A folding operation applied to each cursor in the current view.
#[derive(Debug, Clone, Copy)]
pub enum FoldAction {
    /// Close the innermost open syntax region containing each cursor.
    Close,
    /// Open the outermost closed region containing each cursor's line.
    Open,
    Toggle,
    /// Fold the lines covered by each selection, keeping its first line visible.
    Selection,
    /// Close all syntax regions, retaining nested closed state.
    CloseAll,
    OpenAll,
}

/// Convert a syntax or selection range to a whole-line conceal. The final line
/// ending is kept, so the next document line starts a new display row.
fn conceal(text: RopeSlice, start_line: usize, end_line: usize) -> Option<Conceal> {
    (start_line < end_line)
        .then(|| Conceal {
            range: line_end_char_index(&text, start_line)..line_end_char_index(&text, end_line),
        })
        .filter(|c| c.range.start < c.range.end)
}

impl Document {
    pub fn folded_ranges(&self, view: ViewId) -> &[Conceal] {
        self.view_data
            .get(&view)
            .map_or(&[], |data| &data.folds.visible)
    }

    /// Change folding without modifying document text or undo history.
    pub fn fold(&mut self, view: ViewId, action: FoldAction) {
        let text = self.text.slice(..);
        let selection = self.selection(view).clone();
        let candidates: Vec<_> = if matches!(
            action,
            FoldAction::Close | FoldAction::Toggle | FoldAction::CloseAll
        ) {
            let loader = self.syn_loader.load();
            self.syntax
                .as_ref()
                .map(|syntax| {
                    if matches!(action, FoldAction::CloseAll) {
                        syntax.fold_ranges(text, &loader, ..)
                    } else {
                        selection
                            .iter()
                            .flat_map(|range| {
                                let line = text.char_to_line(range.cursor(text));
                                let bytes = text.line_to_byte(line)..text.line_to_byte(line + 1);
                                syntax.fold_ranges(
                                    text,
                                    &loader,
                                    bytes.start as u32..bytes.end as u32,
                                )
                            })
                            .collect()
                    }
                })
                .unwrap_or_default()
                .into_iter()
                .filter_map(|fold| {
                    if fold.range.is_empty() {
                        return None;
                    }
                    let start_line = text.char_to_line(fold.range.start);
                    let Some(end) = fold.closing_delimiter else {
                        return conceal(text, start_line, text.char_to_line(fold.range.end - 1));
                    };
                    let start = line_end_char_index(&text, start_line);
                    (text.char_to_line(end) > start_line && start < end)
                        .then_some(Conceal { range: start..end })
                })
                .collect()
        } else {
            Vec::new()
        };
        let data = self.view_data.entry(view).or_default();
        match action {
            FoldAction::OpenAll => data.folds = Folds::default(),
            FoldAction::CloseAll => {
                data.folds.closed.extend(candidates);
                data.folds.rebuild();
            }
            _ => {
                // Resolve all cursors against one snapshot. Two cursors on the
                // same fold must toggle it once, rather than undo each other.
                let mut close = Vec::new();
                let mut open = Vec::new();
                for range in &selection {
                    let cursor = range.cursor(text);
                    let line = text.char_to_line(cursor);
                    let closed = data
                        .folds
                        .visible
                        .iter()
                        .find(|c| {
                            text.char_to_line(c.range.start) <= line
                                && line <= text.char_to_line(c.range.end)
                        })
                        .cloned();
                    if matches!(action, FoldAction::Open | FoldAction::Toggle) {
                        if let Some(closed) = &closed {
                            open.push(closed.clone());
                            continue;
                        }
                    }
                    match action {
                        FoldAction::Selection => {
                            let (start, end) = range.line_range(text);
                            if let Some(fold) = conceal(text, start, end) {
                                close.push(fold);
                            }
                        }
                        FoldAction::Close | FoldAction::Toggle => {
                            let candidate = candidates
                                .iter()
                                .filter(|candidate| {
                                    let contains_cursor = text.char_to_line(candidate.range.start)
                                        <= line
                                        && line <= text.char_to_line(candidate.range.end);
                                    let encloses_closed = closed.as_ref().is_none_or(|closed| {
                                        candidate.range.start <= closed.range.start
                                            && candidate.range.end >= closed.range.end
                                    });
                                    contains_cursor
                                        && encloses_closed
                                        && !data.folds.closed.contains(candidate)
                                })
                                .min_by_key(|candidate| {
                                    candidate.range.end - candidate.range.start
                                });
                            if let Some(candidate) = candidate {
                                close.push(candidate.clone());
                            }
                        }
                        _ => {}
                    }
                }
                data.folds.closed.retain(|fold| !open.contains(fold));
                data.folds.closed.extend(close);
                data.folds.rebuild();
            }
        }
        // Closing a region moves any hidden cursors to its visible header.
        // Retain other selections and the primary range in multi-cursor mode.
        let selection = selection.transform(|range| {
            let cursor = range.cursor(text);
            if let Some(fold) = data
                .folds
                .visible
                .iter()
                .find(|c| c.range.start < cursor && cursor < c.range.end)
            {
                Range::point(text.line_to_char(text.char_to_line(fold.range.start)))
            } else {
                range
            }
        });
        // A viewport anchored inside a newly closed fold must use its header too.
        let position = &mut data.view_position;
        if let Some(fold) = data
            .folds
            .visible
            .iter()
            .find(|c| c.range.contains(&position.anchor))
        {
            position.anchor = text.line_to_char(text.char_to_line(fold.range.start));
            position.vertical_offset = 0;
        }
        self.set_selection(view, selection);
    }

    /// Open the folds of `view` that hide one of its cursors.
    pub(super) fn reveal_folds(&mut self, view: ViewId) {
        if let (Some(data), Some(selection)) =
            (self.view_data.get_mut(&view), self.selections.get(&view))
        {
            data.folds.reveal(selection, self.text.slice(..));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{editor::Config, graphics::Rect, View};
    use arc_swap::ArcSwap;
    use helix_core::{syntax, Rope, Transaction};
    use std::sync::Arc;

    fn fixture(text: &str, language: Option<&str>) -> (Document, View, View) {
        let loader = if language.is_some() {
            helix_core::config::default_lang_loader()
        } else {
            syntax::Loader::default()
        };
        let loader = Arc::new(ArcSwap::from_pointee(loader));
        let mut doc = Document::from(
            Rope::from_str(text),
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            loader.clone(),
        );
        if let Some(name) = language {
            let loader = loader.load();
            let language = loader.language_for_name(name).unwrap();
            doc.set_language(Some(loader.language(language).config().clone()), &loader);
            assert!(doc.syntax.is_some(), "grammar required for {name}");
        }
        let mut ids = slotmap::SlotMap::<ViewId, ()>::with_key();
        let mut make_view = || {
            let mut view = View::new(doc.id(), Default::default());
            view.id = ids.insert(());
            view.area = Rect::new(0, 0, 80, 10);
            view
        };
        let first = make_view();
        let second = make_view();
        doc.ensure_view_init(first.id);
        doc.ensure_view_init(second.id);
        (doc, first, second)
    }

    #[test]
    fn syntax_folds_nest_and_reopening_a_parent_keeps_closed_children() {
        let (mut doc, view, other) = fixture(
            "fn main() {\n    if true {\n        work();\n    }\n}\nnext();",
            Some("rust"),
        );
        let text = doc.text().clone();
        let inner = text.line_to_char(1);
        let inner_fold = Conceal {
            range: line_end_char_index(&text.slice(..), 1)..text.line_to_char(3) + 4,
        };
        let outer_fold = Conceal {
            range: line_end_char_index(&text.slice(..), 0)..text.line_to_char(4),
        };
        doc.set_selection(view.id, Selection::point(inner));
        doc.fold(view.id, FoldAction::Close);
        assert_eq!(
            doc.folded_ranges(view.id),
            std::slice::from_ref(&inner_fold)
        );
        doc.fold(view.id, FoldAction::Close);
        assert_eq!(
            doc.folded_ranges(view.id),
            std::slice::from_ref(&outer_fold)
        );
        assert_eq!(
            doc.selection(view.id)
                .primary()
                .cursor(doc.text().slice(..)),
            0
        );
        assert!(doc.folded_ranges(other.id).is_empty());
        doc.fold(view.id, FoldAction::Open);
        assert_eq!(
            doc.folded_ranges(view.id),
            std::slice::from_ref(&inner_fold)
        );
        assert_eq!(doc.text(), &text);
        assert!(!doc.is_modified());
        doc.fold(view.id, FoldAction::OpenAll);
        assert!(doc.folded_ranges(view.id).is_empty());
        doc.fold(view.id, FoldAction::CloseAll);
        assert_eq!(
            doc.folded_ranges(view.id),
            std::slice::from_ref(&outer_fold)
        );
        doc.fold(view.id, FoldAction::Open);
        assert_eq!(
            doc.folded_ranges(view.id),
            std::slice::from_ref(&inner_fold)
        );
    }

    #[test]
    fn closing_a_syntax_fold_keeps_a_cursor_on_its_real_closing_token() {
        let (mut doc, view, _) = fixture("fn main() {\n    work();\n}\n", Some("rust"));
        let text = doc.text().clone();
        let closer = text.line_to_char(2);
        doc.set_selection(view.id, Selection::point(closer));
        doc.fold(view.id, FoldAction::CloseAll);
        assert_eq!(doc.folded_ranges(view.id).len(), 1);
        assert_eq!(
            doc.selection(view.id).primary().cursor(text.slice(..)),
            closer
        );
    }

    #[test]
    fn manual_folds_support_plain_text_and_reveal_jump_destinations_per_view() {
        let (mut doc, view, other) = fixture("header\nhidden\nlast\nnext", None);
        let selected = Selection::single(0, doc.text().line_to_char(3));
        for id in [view.id, other.id] {
            doc.set_selection(id, selected.clone());
            doc.fold(id, FoldAction::Selection);
        }
        let fold = conceal(doc.text().slice(..), 0, 2).unwrap();
        assert_eq!(doc.folded_ranges(view.id), std::slice::from_ref(&fold));
        assert_eq!(
            view.pos_at_visual_coords(&doc, 1, 0, true),
            Some(doc.text().line_to_char(3))
        );
        doc.set_selection(view.id, Selection::point(doc.text().line_to_char(1)));
        assert!(doc.folded_ranges(view.id).is_empty());
        assert_eq!(doc.folded_ranges(other.id), &[fold]);
        doc.remove_view(other.id);
        doc.ensure_view_init(other.id);
        assert!(doc.folded_ranges(other.id).is_empty());
    }

    #[test]
    fn edits_map_unaffected_folds_and_open_touched_folds_in_all_views() {
        for newline in ["\n", "\r\n"] {
            let (mut doc, mut view, other) = fixture(
                &format!("before{newline}header{newline}hidden{newline}last{newline}after"),
                None,
            );
            for id in [view.id, other.id] {
                doc.set_selection(
                    id,
                    Selection::single(doc.text().line_to_char(1), doc.text().line_to_char(4)),
                );
                doc.fold(id, FoldAction::Selection);
            }
            let insert = Transaction::change(
                doc.text(),
                [(0, 0, Some(format!("extra{newline}").into()))].into_iter(),
            );
            doc.apply(&insert, view.id);
            doc.append_changes_to_history(&mut view);
            for id in [view.id, other.id] {
                assert_eq!(
                    doc.folded_ranges(id),
                    &[conceal(doc.text().slice(..), 2, 4).unwrap()]
                );
            }
            assert!(doc.undo(&mut view));
            assert_eq!(
                doc.folded_ranges(other.id),
                &[conceal(doc.text().slice(..), 1, 3).unwrap()]
            );
            assert!(doc.redo(&mut view));
            let hidden = doc.text().line_to_char(3);
            let edit = Transaction::change(
                doc.text(),
                [(hidden, hidden + 1, Some("X".into()))].into_iter(),
            );
            doc.apply(&edit, view.id);
            assert!(doc.folded_ranges(view.id).is_empty());
            assert!(doc.folded_ranges(other.id).is_empty());
        }
    }

    #[test]
    fn multiple_cursors_toggle_a_shared_fold_once_and_keep_the_primary_range() {
        let (mut doc, view, _) = fixture("{\n  \"value\": [\n    1, 2\n  ]\n}\n", Some("json"));
        doc.set_selection(
            view.id,
            Selection::new(vec![Range::point(0), Range::point(1)].into(), 1),
        );
        doc.fold(view.id, FoldAction::Toggle);
        assert_eq!(doc.folded_ranges(view.id).len(), 1);
        assert_eq!(doc.selection(view.id).primary_index(), 1);
        doc.fold(view.id, FoldAction::Toggle);
        assert!(doc.folded_ranges(view.id).is_empty());
    }

    #[test]
    fn changing_the_language_drops_folds() {
        let (mut doc, view, _) = fixture("{\n  \"key\": 1\n}\n", Some("json"));
        doc.set_selection(view.id, Selection::single(0, doc.text().len_chars()));
        doc.fold(view.id, FoldAction::Selection);
        assert_eq!(doc.folded_ranges(view.id).len(), 1);
        let loader = doc.syn_loader.load_full();
        doc.set_language(None, &loader);
        assert!(doc.folded_ranges(view.id).is_empty());
    }

    #[test]
    fn crossing_regions_are_ignored_and_nested_regions_remain_available() {
        let mut folds = Folds {
            closed: [1..10, 4..7, 6..15, 16..20, 20..25]
                .map(|range| Conceal { range })
                .to_vec(),
            visible: Vec::new(),
        };
        folds.rebuild();
        assert_eq!(
            folds
                .closed
                .iter()
                .map(|c| c.range.clone())
                .collect::<Vec<_>>(),
            [1..10, 4..7, 16..20]
        );
        assert_eq!(
            folds
                .visible
                .iter()
                .map(|c| c.range.clone())
                .collect::<Vec<_>>(),
            [1..10, 16..20]
        );
    }
}
