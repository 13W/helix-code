//! Folding commands. Folds belong to the current view and never change the text.
//!
//! Ported from Mitos (mitos-editor/mitos#651).

use helix_view::document::FoldAction;

use super::Context;

fn fold(cx: &mut Context, action: FoldAction) {
    let count = match action {
        FoldAction::Close | FoldAction::Open | FoldAction::Toggle => cx.count(),
        _ => 1,
    };
    let (view, doc) = current!(cx.editor);
    for _ in 0..count {
        doc.fold(view.id, action);
    }
}

pub fn fold_close(cx: &mut Context) {
    fold(cx, FoldAction::Close);
}

pub fn fold_open(cx: &mut Context) {
    fold(cx, FoldAction::Open);
}

pub fn fold_toggle(cx: &mut Context) {
    fold(cx, FoldAction::Toggle);
}

pub fn fold_selection(cx: &mut Context) {
    fold(cx, FoldAction::Selection);
}

pub fn fold_all(cx: &mut Context) {
    fold(cx, FoldAction::CloseAll);
}

pub fn unfold_all(cx: &mut Context) {
    fold(cx, FoldAction::OpenAll);
}
