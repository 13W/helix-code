use std::sync::Arc;

use arc_swap::ArcSwap;
use diagnostics::PullAllDocumentsDiagnosticHandler;
use helix_event::AsyncHook;

use crate::config::Config;
use crate::events;
use crate::handlers::auto_save::AutoSaveHandler;
use crate::handlers::diagnostics::PullDiagnosticsHandler;
use crate::handlers::signature_help::SignatureHelpHandler;

pub use helix_view::handlers::{word_index, Handlers};

use self::document_colors::DocumentColorsHandler;
use self::document_links::DocumentLinksHandler;

mod auto_save;
pub mod completion;
pub mod diagnostics;
mod document_colors;
mod document_highlight;
mod document_links;
mod prompt;
mod signature_help;
mod snippet;
mod workspace_trust;

pub fn setup(config: Arc<ArcSwap<Config>>) -> Handlers {
    events::register();

    let event_tx = completion::CompletionHandler::new(config).spawn();
    let signature_hints = SignatureHelpHandler::new().spawn();
    let auto_save = AutoSaveHandler::new().spawn();
    let document_colors = DocumentColorsHandler::default().spawn();
    let document_links = DocumentLinksHandler::default().spawn();
    let word_index = word_index::Handler::spawn();
    let pull_diagnostics = PullDiagnosticsHandler::default().spawn();
    let pull_all_documents_diagnostics = PullAllDocumentsDiagnosticHandler::default().spawn();

    let handlers = Handlers {
        completions: helix_view::handlers::completion::CompletionHandler::new(event_tx),
        signature_hints,
        auto_save,
        document_colors,
        document_links,
        word_index,
        pull_diagnostics,
        pull_all_documents_diagnostics,
    };

    helix_view::handlers::register_hooks(&handlers);
    completion::register_hooks(&handlers);
    signature_help::register_hooks(&handlers);
    document_highlight::register_hooks(&handlers);
    auto_save::register_hooks(&handlers);
    diagnostics::register_hooks(&handlers);
    snippet::register_hooks(&handlers);
    document_colors::register_hooks(&handlers);
    document_links::register_hooks(&handlers);
    prompt::register_hooks(&handlers);
    workspace_trust::register_hooks(&handlers);
    on_type_formatting::register_hooks();
    handlers
}

mod on_type_formatting {
    use helix_core::{indent::IndentStyle, syntax::config::LanguageServerFeature};
    use helix_event::register_hook;

    use crate::events::PostInsertChar;

    pub(super) fn register_hooks() {
        register_hook!(move |event: &mut PostInsertChar<'_, '_>| {
            let PostInsertChar { c, cx } = event;
            let (view, doc) = helix_view::current_ref!(cx.editor);
            let view_id = view.id;

            // Collect trigger-matching servers to avoid borrowing issues
            let servers: Vec<_> = doc
                .language_servers_with_feature(LanguageServerFeature::OnTypeFormatting)
                .filter_map(|ls| {
                    let provider = ls
                        .capabilities()
                        .document_on_type_formatting_provider
                        .as_ref()?;
                    let is_trigger = provider.first_trigger_character == c.to_string()
                        || provider
                            .more_trigger_character
                            .as_ref()
                            .is_some_and(|chars| chars.iter().any(|tc| tc == &c.to_string()));
                    if !is_trigger {
                        return None;
                    }
                    let future = ls.on_type_formatting(
                        doc.identifier(),
                        doc.position(view_id, ls.offset_encoding()),
                        *c,
                        doc.tab_width() as u32,
                        matches!(doc.indent_style, IndentStyle::Spaces(_)),
                    );
                    let offset_encoding = ls.offset_encoding();
                    Some((future, offset_encoding))
                })
                .collect();

            if servers.is_empty() {
                return Ok(());
            }

            let doc_id = doc.id();
            let doc_version = doc.version();
            let text = doc.text().clone();

            for (future, offset_encoding) in servers {
                let text = text.clone();
                tokio::spawn(async move {
                    match future.await {
                        Ok(Some(edits)) if !edits.is_empty() => {
                            let transaction = helix_lsp::util::generate_transaction_from_edits(
                                &text,
                                edits,
                                offset_encoding,
                            );
                            crate::job::dispatch(move |editor, _compositor| {
                                let Some(doc) = editor.documents.get_mut(&doc_id) else {
                                    return;
                                };
                                if doc.version() != doc_version {
                                    return;
                                }
                                doc.apply(&transaction, view_id);
                                let view = editor.tree.get_mut(view_id);
                                doc.append_changes_to_history(view);
                            })
                            .await;
                        }
                        Ok(_) => {}
                        Err(e) => log::error!("on_type_formatting error: {e}"),
                    }
                });
            }

            Ok(())
        });
    }
}
