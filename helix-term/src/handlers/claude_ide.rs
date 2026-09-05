//! Feeds editor selection changes to the Claude Code IDE server
//! (`selection_changed`, PROTO §6.1).

use helix_claude_ide::notify::{Position, SelectionInfo};
use helix_core::RopeSlice;
use helix_event::register_hook;
use helix_view::events::{DocumentDidClose, DocumentFocusLost, SelectionDidChange};
use helix_view::{Document, Editor, ViewId};

use crate::handlers::Handlers;

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |event: &mut SelectionDidChange<'_>| {
        if let Some(handler) = crate::claude_ide::current_handler() {
            if event.doc.config.load().claude_ide.notify_selection {
                if let Some(info) = selection_info(event.doc, event.view) {
                    handler.selection_changed(info);
                }
            }
        }
        Ok(())
    });

    // Closing the proposal *buffer* (`:bc`, not `:q`, which only closes a
    // window) without a decision rejects the proposal (PROTO §5.3, "tab closed").
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        let closed = event.doc.id();
        let editor: &mut Editor = event.editor;
        let Some(diff) = editor
            .claude_diff_view_for_doc(closed)
            .filter(|v| v.right == closed)
            .cloned()
        else {
            return Ok(());
        };
        if let Some(tx) = diff.reply.lock().unwrap().take() {
            let _ = tx.send(helix_mcp::DiffOutcome::Rejected);
        }
        // Tear the rest down outside of `close_document`.
        let tab_name = diff.tab_name.clone();
        crate::job::dispatch_blocking(move |editor, _| {
            crate::application::Application::claude_close_diff_split(editor, &tab_name);
        });
        Ok(())
    });

    // Switching windows/buffers does not move a selection, but the active
    // file changes; report the selection of the newly focused document.
    register_hook!(move |event: &mut DocumentFocusLost<'_>| {
        if let Some(handler) = crate::claude_ide::current_handler() {
            if let Some(info) = focused_selection(event.editor) {
                handler.selection_changed(info);
            }
        }
        Ok(())
    });
}

/// Selection of the currently focused view, if its document has a path.
pub fn focused_selection(editor: &Editor) -> Option<SelectionInfo> {
    if !editor.config().claude_ide.notify_selection {
        return None;
    }
    let view = editor.tree.get(editor.tree.focus);
    let doc = editor.documents.get(&view.doc)?;
    selection_info(doc, view.id)
}

/// Primary selection of `doc` in `view` as the CLI expects it.
///
/// Helix has no empty selections: a bare cursor is a one-character range.
/// Such ranges are reported as an empty selection at the cursor, so that the
/// CLI does not show "1 line selected" while the user merely moves around.
/// Scratch buffers (no path) are skipped.
pub fn selection_info(doc: &Document, view: ViewId) -> Option<SelectionInfo> {
    let path = doc.path()?;
    let selection = doc.selections().get(&view)?;
    let text = doc.text().slice(..);
    let range = selection.primary();
    let (from, to, fragment) = if range.to().saturating_sub(range.from()) <= 1 {
        let cursor = range.cursor(text);
        (cursor, cursor, String::new())
    } else {
        (range.from(), range.to(), range.fragment(text).into_owned())
    };
    Some(SelectionInfo {
        file_path: path.to_path_buf(),
        text: fragment,
        start: position(text, from),
        end: position(text, to),
    })
}

fn position(text: RopeSlice, char_idx: usize) -> Position {
    let char_idx = char_idx.min(text.len_chars());
    let line = text.char_to_line(char_idx);
    Position {
        line,
        character: char_idx - text.line_to_char(line),
    }
}
