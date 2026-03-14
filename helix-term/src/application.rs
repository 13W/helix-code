use arc_swap::{access::Map, ArcSwap};
use futures_util::Stream;
use helix_core::{diagnostic::Severity, pos_at_coords, syntax, Range, Selection};
use helix_lsp::{
    lsp::{self, notification::Notification},
    util::lsp_range_to_range,
    LanguageServerId, LspProgressMap,
};
use helix_stdx::path::get_relative_path;
use helix_view::{
    align_view,
    document::{DocumentOpenError, DocumentSavedEventResult},
    editor::{ConfigEvent, EditorEvent},
    graphics::Rect,
    theme,
    tree::Layout,
    Align, Editor,
};
use serde_json::json;
use tui::backend::Backend;

use crate::{
    args::Args,
    compositor::{Compositor, Event},
    config::Config,
    handlers,
    job::Jobs,
    keymap::Keymaps,
    ui::{self, overlay::overlaid},
};

use log::{debug, error, info, warn};
use std::{
    io::{stdin, IsTerminal},
    path::Path,
    sync::Arc,
};

#[cfg_attr(windows, allow(unused_imports))]
use anyhow::{Context, Error};

#[cfg(not(windows))]
use {signal_hook::consts::signal, signal_hook_tokio::Signals};
#[cfg(windows)]
type Signals = futures_util::stream::Empty<()>;

#[cfg(all(not(windows), not(feature = "integration")))]
use tui::backend::TerminaBackend;

#[cfg(all(windows, not(feature = "integration")))]
use tui::backend::CrosstermBackend;

#[cfg(feature = "integration")]
use tui::backend::TestBackend;

#[cfg(all(not(windows), not(feature = "integration")))]
type TerminalBackend = TerminaBackend;
#[cfg(all(windows, not(feature = "integration")))]
type TerminalBackend = CrosstermBackend<std::io::Stdout>;
#[cfg(feature = "integration")]
type TerminalBackend = TestBackend;

#[cfg(not(windows))]
type TerminalEvent = termina::Event;
#[cfg(windows)]
type TerminalEvent = crossterm::event::Event;

type Terminal = tui::terminal::Terminal<TerminalBackend>;

pub struct Application {
    compositor: Compositor,
    terminal: Terminal,
    pub editor: Editor,

    config: Arc<ArcSwap<Config>>,

    signals: Signals,
    jobs: Jobs,
    lsp_progress: LspProgressMap,

    theme_mode: Option<theme::Mode>,

    mcp_rx: Option<tokio::sync::mpsc::Receiver<helix_mcp::McpCommand>>,
}


#[cfg(feature = "integration")]
fn setup_integration_logging() {
    let level = std::env::var("HELIX_LOG_LEVEL")
        .map(|lvl| lvl.parse().unwrap())
        .unwrap_or(log::LevelFilter::Info);

    // Separate file config so we can include year, month and day in file logs
    let _ = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} {} [{}] {}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f"),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(level)
        .chain(std::io::stdout())
        .apply();
}

impl Application {
    pub fn new(args: Args, config: Config, lang_loader: syntax::Loader) -> Result<Self, Error> {
        #[cfg(feature = "integration")]
        setup_integration_logging();

        use helix_view::editor::Action;

        let mut theme_parent_dirs = vec![helix_loader::config_dir()];
        theme_parent_dirs.extend(helix_loader::runtime_dirs().iter().cloned());
        let theme_loader = theme::Loader::new(&theme_parent_dirs);

        #[cfg(all(not(windows), not(feature = "integration")))]
        let backend = TerminaBackend::new((&config.editor).into())
            .context("failed to create terminal backend")?;
        #[cfg(all(windows, not(feature = "integration")))]
        let backend = CrosstermBackend::new(std::io::stdout(), (&config.editor).into());

        #[cfg(feature = "integration")]
        let backend = TestBackend::new(120, 150);

        let theme_mode = backend.get_theme_mode();
        let mut terminal = Terminal::new(backend)?;
        let area = terminal.size();
        let mut compositor = Compositor::new(area);
        let config = Arc::new(ArcSwap::from_pointee(config));
        let handlers = handlers::setup(config.clone());
        let mut editor = Editor::new(
            area,
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(lang_loader)),
            Arc::new(Map::new(Arc::clone(&config), |config: &Config| {
                &config.editor
            })),
            handlers,
        );
        Self::load_configured_theme(&mut editor, &config.load(), &mut terminal, theme_mode);

        let jobs = Jobs::new();

        let keys = Box::new(Map::new(Arc::clone(&config), |config: &Config| {
            &config.keys
        }));
        let editor_view = Box::new(ui::EditorView::new(Keymaps::new(keys)));
        compositor.push(editor_view);

        if args.load_tutor {
            let path = helix_loader::runtime_file(Path::new("tutor"));
            editor.open(&path, Action::VerticalSplit)?;
            // Unset path to prevent accidentally saving to the original tutor file.
            doc_mut!(editor).set_path(None);
        } else if !args.files.is_empty() {
            let mut files_it = args.files.into_iter().peekable();

            // If the first file is a directory, skip it and open a picker
            if let Some((first, _)) = files_it.next_if(|(p, _)| p.is_dir()) {
                let picker = ui::file_picker(&editor, first);
                compositor.push(Box::new(overlaid(picker)));
            }

            // If there are any more files specified, open them
            if files_it.peek().is_some() {
                let mut nr_of_files = 0;
                for (file, pos) in files_it {
                    nr_of_files += 1;
                    if file.is_dir() {
                        return Err(anyhow::anyhow!(
                            "expected a path to file, but found a directory: {file:?}. (to open a directory pass it as first argument)"
                        ));
                    } else {
                        // If the user passes in either `--vsplit` or
                        // `--hsplit` as a command line argument, all the given
                        // files will be opened according to the selected
                        // option. If neither of those two arguments are passed
                        // in, just load the files normally.
                        let action = match args.split {
                            _ if nr_of_files == 1 => Action::VerticalSplit,
                            Some(Layout::Vertical) => Action::VerticalSplit,
                            Some(Layout::Horizontal) => Action::HorizontalSplit,
                            None => Action::Load,
                        };
                        let old_id = editor.document_id_by_path(&file);
                        let doc_id = match editor.open(&file, action) {
                            // Ignore irregular files during application init.
                            Err(DocumentOpenError::IrregularFile) => {
                                nr_of_files -= 1;
                                continue;
                            }
                            Err(err) => return Err(anyhow::anyhow!(err)),
                            // We can't open more than 1 buffer for 1 file, in this case we already have opened this file previously
                            Ok(doc_id) if old_id == Some(doc_id) => {
                                nr_of_files -= 1;
                                doc_id
                            }
                            Ok(doc_id) => doc_id,
                        };
                        // with Action::Load all documents have the same view
                        // NOTE: this isn't necessarily true anymore. If
                        // `--vsplit` or `--hsplit` are used, the file which is
                        // opened last is focused on.
                        let view_id = editor.tree.focus;
                        let doc = doc_mut!(editor, &doc_id);
                        let selection = pos
                            .into_iter()
                            .map(|coords| {
                                Range::point(pos_at_coords(doc.text().slice(..), coords, true))
                            })
                            .collect();
                        doc.set_selection(view_id, selection);
                    }
                }

                // if all files were invalid, replace with empty buffer
                if nr_of_files == 0 {
                    editor.new_file(Action::VerticalSplit);
                } else {
                    editor.set_status(format!(
                        "Loaded {} file{}.",
                        nr_of_files,
                        if nr_of_files == 1 { "" } else { "s" } // avoid "Loaded 1 files." grammo
                    ));
                    // align the view to center after all files are loaded,
                    // does not affect views without pos since it is at the top
                    let (view, doc) = current!(editor);
                    align_view(doc, view, Align::Center);
                }
            } else {
                editor.new_file(Action::VerticalSplit);
            }
        } else if stdin().is_terminal() || cfg!(feature = "integration") {
            editor.new_file(Action::VerticalSplit);
        } else {
            editor
                .new_file_from_stdin(Action::VerticalSplit)
                .unwrap_or_else(|_| editor.new_file(Action::VerticalSplit));
        }

        #[cfg(windows)]
        let signals = futures_util::stream::empty();
        #[cfg(not(windows))]
        let signals = Signals::new([
            signal::SIGTSTP,
            signal::SIGCONT,
            signal::SIGUSR1,
            signal::SIGTERM,
            signal::SIGINT,
        ])
        .context("build signal handler")?;

        let mcp_rx = Some(helix_mcp::init_editor_channel());

        // Start the MCP HTTP server eagerly when the --mcp flag was passed.
        if args.mcp {
            use std::net::SocketAddr;
            let bind: Option<SocketAddr> = args
                .mcp_port
                .map(|p| SocketAddr::from(([127, 0, 0, 1], p)));
            let handle = tokio::runtime::Handle::current();
            match tokio::task::block_in_place(|| handle.block_on(helix_mcp::run_mcp_server(bind))) {
                Ok(addr) => {
                    // Print before the TUI takes over the terminal so scripts can capture it.
                    eprintln!("helix-mcp: MCP server listening at http://{addr}/mcp");
                    editor.mcp_addr = Some(addr);
                }
                Err(e) => {
                    log::warn!("helix-mcp: failed to start MCP server: {e}");
                }
        }

        if args.mcp_auto_approve {
            helix_mcp::set_auto_approve(true);
        }
        }

        let app = Self {
            compositor,
            terminal,
            editor,
            config,
            signals,
            jobs,
            lsp_progress: LspProgressMap::new(),
            theme_mode,
            mcp_rx,
        };

        Ok(app)
    }

    async fn render(&mut self) {
        if self.compositor.full_redraw {
            self.terminal.clear().expect("Cannot clear the terminal");
            self.compositor.full_redraw = false;
        }

        let mut cx = crate::compositor::Context {
            editor: &mut self.editor,
            jobs: &mut self.jobs,
            scroll: None,
        };

        helix_event::start_frame();
        cx.editor.needs_redraw = false;

        let area = self
            .terminal
            .autoresize()
            .expect("Unable to determine terminal size");

        // TODO: need to recalculate view tree if necessary

        let surface = self.terminal.current_buffer_mut();

        self.compositor.render(area, surface, &mut cx);
        let (pos, kind) = self.compositor.cursor(area, &self.editor);
        // reset cursor cache
        self.editor.cursor_cache.reset();

        let pos = pos.map(|pos| (pos.col as u16, pos.row as u16));
        self.terminal.draw(pos, kind).unwrap();
    }

    pub async fn event_loop<S>(&mut self, input_stream: &mut S)
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        self.render().await;

        loop {
            if !self.event_loop_until_idle(input_stream).await {
                break;
            }
        }
    }

    pub async fn event_loop_until_idle<S>(&mut self, input_stream: &mut S) -> bool
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        loop {
            if self.editor.should_close() {
                return false;
            }

            use futures_util::StreamExt;

            tokio::select! {
                biased;

                Some(signal) = self.signals.next() => {
                    if !self.handle_signals(signal).await {
                        return false;
                    };
                }
                Some(event) = input_stream.next() => {
                    self.handle_terminal_events(event).await;
                }
                Some(callback) = self.jobs.callbacks.recv() => {
                    self.jobs.handle_callback(&mut self.editor, &mut self.compositor, Ok(Some(callback)));
                    self.render().await;
                }
                Some(msg) = self.jobs.status_messages.recv() => {
                    let severity = match msg.severity{
                        helix_event::status::Severity::Hint => Severity::Hint,
                        helix_event::status::Severity::Info => Severity::Info,
                        helix_event::status::Severity::Warning => Severity::Warning,
                        helix_event::status::Severity::Error => Severity::Error,
                    };
                    // TODO: show multiple status messages at once to avoid clobbering
                    self.editor.status_msg = Some((msg.message, severity));
                    helix_event::request_redraw();
                }
                Some(callback) = self.jobs.wait_futures.next() => {
                    self.jobs.handle_callback(&mut self.editor, &mut self.compositor, callback);
                    self.render().await;
                }
                event = self.editor.wait_event() => {
                    let _idle_handled = self.handle_editor_event(event).await;

                    #[cfg(feature = "integration")]
                    {
                        if _idle_handled {
                            return true;
                        }
                    }
                }
                cmd = async {
                    match &mut self.mcp_rx {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                } => {
                    if let Some(cmd) = cmd {
                        self.handle_mcp_command(cmd).await;
                    }
                }
            }

            // for integration tests only, reset the idle timer after every
            // event to signal when test events are done processing
            #[cfg(feature = "integration")]
            {
                self.editor.reset_idle_timer();
            }
        }
    }

    pub fn handle_config_events(&mut self, config_event: ConfigEvent) {
        let old_editor_config = self.editor.config();

        match config_event {
            ConfigEvent::Refresh => self.refresh_config(),

            // Since only the Application can make changes to Editor's config,
            // the Editor must send up a new copy of a modified config so that
            // the Application can apply it.
            ConfigEvent::Update(editor_config) => {
                let mut app_config = (*self.config.load().clone()).clone();
                app_config.editor = *editor_config;
                if let Err(err) = self.terminal.reconfigure((&app_config.editor).into()) {
                    self.editor.set_error(err.to_string());
                };
                self.config.store(Arc::new(app_config));
            }
            ConfigEvent::ThemeChanged => {
                let _ = self.terminal.backend_mut().set_background_color(
                    self.editor
                        .theme
                        .try_get_exact("ui.background")
                        .and_then(|style| style.bg),
                );
                return;
            }
        }

        // Update all the relevant members in the editor after updating
        // the configuration.
        self.editor.refresh_config(&old_editor_config);

        // reset view position in case softwrap was enabled/disabled
        let scrolloff = self.editor.config().scrolloff;
        for (view, _) in self.editor.tree.views() {
            let doc = doc_mut!(self.editor, &view.doc);
            view.ensure_cursor_in_view(doc, scrolloff);
        }
    }

    fn refresh_config(&mut self) {
        let mut refresh_config = || -> Result<(), Error> {
            let default_config = Config::load_default()
                .map_err(|err| anyhow::anyhow!("Failed to load config: {}", err))?;

            // Update the syntax language loader before setting the theme. Setting the theme will
            // call `Loader::set_scopes` which must be done before the documents are re-parsed for
            // the sake of locals highlighting.
            let lang_loader = helix_core::config::user_lang_loader(default_config.editor.insecure)?;
            self.editor.syn_loader.store(Arc::new(lang_loader));
            Self::load_configured_theme(
                &mut self.editor,
                &default_config,
                &mut self.terminal,
                self.theme_mode,
            );

            // Re-parse any open documents with the new language config.
            let lang_loader = self.editor.syn_loader.load();
            for document in self.editor.documents.values_mut() {
                // Re-detect .editorconfig
                document.detect_editor_config();
                document.detect_language(&lang_loader);
                let diagnostics = Editor::doc_diagnostics(
                    &self.editor.language_servers,
                    &self.editor.diagnostics,
                    document,
                );
                document.replace_diagnostics(diagnostics, &[], None);
            }

            self.terminal.reconfigure((&default_config.editor).into())?;
            // Store new config
            self.config.store(Arc::new(default_config));
            Ok(())
        };

        match refresh_config() {
            Ok(_) => {
                self.editor.set_status("Config refreshed");
            }
            Err(err) => {
                self.editor.set_error(err.to_string());
            }
        }
    }

    /// Load the theme set in configuration
    fn load_configured_theme(
        editor: &mut Editor,
        config: &Config,
        terminal: &mut Terminal,
        mode: Option<theme::Mode>,
    ) {
        let true_color = terminal.backend().supports_true_color()
            || config.editor.true_color
            || crate::true_color();
        let theme = config
            .theme
            .as_ref()
            .and_then(|theme_config| {
                let theme = theme_config.choose(mode);
                editor
                    .theme_loader
                    .load(theme)
                    .map_err(|e| {
                        log::warn!("failed to load theme `{}` - {}", theme, e);
                        e
                    })
                    .ok()
                    .filter(|theme| {
                        let colors_ok = true_color || theme.is_16_color();
                        if !colors_ok {
                            log::warn!(
                                "loaded theme `{}` but cannot use it because true color \
                                support is not enabled",
                                theme.name()
                            );
                        }
                        colors_ok
                    })
            })
            .unwrap_or_else(|| editor.theme_loader.default_theme(true_color));
        let _ = editor.set_theme(theme);
    }

    #[cfg(windows)]
    // no signal handling available on windows
    pub async fn handle_signals(&mut self, _signal: ()) -> bool {
        true
    }

    #[cfg(not(windows))]
    pub async fn handle_signals(&mut self, signal: i32) -> bool {
        match signal {
            signal::SIGTSTP => {
                self.restore_term().unwrap();

                // SAFETY:
                //
                // - helix must have permissions to send signals to all processes in its signal
                //   group, either by already having the requisite permission, or by having the
                //   user's UID / EUID / SUID match that of the receiving process(es).
                let res = unsafe {
                    // A pid of 0 sends the signal to the entire process group, allowing the user to
                    // regain control of their terminal if the editor was spawned under another process
                    // (e.g. when running `git commit`).
                    //
                    // We have to send SIGSTOP (not SIGTSTP) to the entire process group, because,
                    // as mentioned above, the terminal will get stuck if `helix` was spawned from
                    // an external process and that process waits for `helix` to complete. This may
                    // be an issue with signal-hook-tokio, but the author of signal-hook believes it
                    // could be a tokio issue instead:
                    // https://github.com/vorner/signal-hook/issues/132
                    libc::kill(0, signal::SIGSTOP)
                };

                if res != 0 {
                    let err = std::io::Error::last_os_error();
                    eprintln!("{}", err);
                    let res = err.raw_os_error().unwrap_or(1);
                    std::process::exit(res);
                }
            }
            signal::SIGCONT => {
                // Copy/Paste from same issue from neovim:
                // https://github.com/neovim/neovim/issues/12322
                // https://github.com/neovim/neovim/pull/13084
                for retries in 1..=10 {
                    match self.terminal.claim() {
                        Ok(()) => break,
                        Err(err) if retries == 10 => panic!("Failed to claim terminal: {}", err),
                        Err(_) => continue,
                    }
                }

                // redraw the terminal
                let area = self.terminal.size();
                self.compositor.resize(area);
                self.terminal.clear().expect("couldn't clear terminal");

                self.render().await;
            }
            signal::SIGUSR1 => {
                self.refresh_config();
                self.render().await;
            }
            signal::SIGTERM | signal::SIGINT => {
                self.restore_term().unwrap();
                return false;
            }
            _ => unreachable!(),
        }

        true
    }

    pub async fn handle_idle_timeout(&mut self) {
        let mut cx = crate::compositor::Context {
            editor: &mut self.editor,
            jobs: &mut self.jobs,
            scroll: None,
        };
        let should_render = self.compositor.handle_event(&Event::IdleTimeout, &mut cx);
        if should_render || self.editor.needs_redraw {
            self.render().await;
        }
    }

    pub fn handle_document_write(&mut self, doc_save_event: DocumentSavedEventResult) {
        let doc_save_event = match doc_save_event {
            Ok(event) => event,
            Err(err) => {
                self.editor.set_error(err.to_string());
                return;
            }
        };

        let doc = match self.editor.document_mut(doc_save_event.doc_id) {
            None => {
                warn!(
                    "received document saved event for non-existent doc id: {}",
                    doc_save_event.doc_id
                );

                return;
            }
            Some(doc) => doc,
        };

        debug!(
            "document {:?} saved with revision {}",
            doc.path(),
            doc_save_event.revision
        );

        doc.set_last_saved_revision(doc_save_event.revision, doc_save_event.save_time);

        let lines = doc_save_event.text.len_lines();
        let size = doc_save_event.text.len_bytes();

        enum Size {
            Bytes(u16),
            HumanReadable(f32, &'static str),
        }

        impl std::fmt::Display for Size {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::Bytes(bytes) => write!(f, "{bytes}B"),
                    Self::HumanReadable(size, suffix) => write!(f, "{size:.1}{suffix}"),
                }
            }
        }

        let size = if size < 1024 {
            Size::Bytes(size as u16)
        } else {
            const SUFFIX: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
            let mut size = size as f32;
            let mut i = 0;
            while i < SUFFIX.len() - 1 && size >= 1024.0 {
                size /= 1024.0;
                i += 1;
            }
            Size::HumanReadable(size, SUFFIX[i])
        };

        self.editor
            .set_doc_path(doc_save_event.doc_id, &doc_save_event.path);
        // TODO: fix being overwritten by lsp
        self.editor.set_status(format!(
            "'{}' written, {lines}L {size}",
            get_relative_path(&doc_save_event.path).to_string_lossy(),
        ));
    }

    #[inline(always)]
    pub async fn handle_editor_event(&mut self, event: EditorEvent) -> bool {
        log::debug!("received editor event: {:?}", event);

        match event {
            EditorEvent::DocumentSaved(event) => {
                self.handle_document_write(event);
                self.render().await;
            }
            EditorEvent::ConfigEvent(event) => {
                self.handle_config_events(event);
                self.render().await;
            }
            EditorEvent::LanguageServerMessage((id, call)) => {
                self.handle_language_server_message(call, id).await;
                // limit render calls for fast language server messages
                helix_event::request_redraw();
            }
            EditorEvent::DebuggerEvent((id, payload)) => {
                let needs_render = self.editor.handle_debugger_message(id, payload).await;
                if needs_render {
                    self.render().await;
                }
            }
            EditorEvent::AcpMessage((agent_id, event)) => {
                self.handle_acp_message(agent_id, event).await;
                self.render().await;
            }
            EditorEvent::Redraw => {
                self.render().await;
            }
            EditorEvent::IdleTimer => {
                self.editor.clear_idle_timer();
                self.handle_idle_timeout().await;

                #[cfg(feature = "integration")]
                {
                    return true;
                }
            }
        }

        false
    }

    /// Generate a unified diff string between `old` and `new` content.
    fn bp_to_info(
        path: &std::path::Path,
        b: &helix_view::editor::Breakpoint,
    ) -> helix_mcp::BreakpointInfo {
        helix_mcp::BreakpointInfo {
            path: path.to_path_buf(),
            line: b.line,
            column: b.column,
            condition: b.condition.clone(),
            verified: b.verified,
            id: b.id,
            message: b.message.clone(),
        }
    }

    fn mcp_unified_diff(old: &str, new: &str, path: &std::path::Path) -> String {
        use similar::TextDiff;
        TextDiff::from_lines(old, new)
            .unified_diff()
            .header(
                &format!("a/{}", path.display()),
                &format!("b/{}", path.display()),
            )
            .to_string()
    }

    /// Build a truncated diff preview string for MCP approval dialogs.
    fn mcp_diff_preview(diff: &str) -> String {
        let truncated: String = diff.lines().take(20).collect::<Vec<_>>().join("\n");
        let max = truncated.len().min(400);
        let safe_len = truncated
            .char_indices()
            .map(|(i, _)| i)
            .filter(|&i| i <= max)
            .last()
            .unwrap_or(0);
        if safe_len < diff.len() {
            format!("{}\u{2026}", &truncated[..safe_len])
        } else {
            truncated
        }
    }


    /// Read the current string content of `path` — from the open buffer if available,
    /// otherwise from disk.  Returns an empty string for new (not-yet-existing) files.
    fn mcp_read_content(editor: &helix_view::Editor, path: &std::path::Path) -> String {
        if let Some(doc) = editor.document_by_path(path) {
            doc.text().to_string()
        } else {
            std::fs::read_to_string(path).unwrap_or_default()
        }
    }

    async fn handle_mcp_command(&mut self, cmd: helix_mcp::McpCommand) {
        use helix_mcp::{BufferInfo, McpCommand, WriteResult};
        match cmd {
            McpCommand::ReadFile { path, reply } => {
                let result = if let Some(doc) = self.editor.document_by_path(&path) {
                    Ok(doc.text().to_string())
                } else {
                    std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!(e))
                };
                let _ = reply.send(result);
            }
            McpCommand::ReadRange {
                path,
                start_line,
                end_line,
                reply,
            } => {
                let result = (|| -> anyhow::Result<String> {
                    let text = if let Some(doc) = self.editor.document_by_path(&path) {
                        doc.text().clone()
                    } else {
                        let s = std::fs::read_to_string(&path)?;
                        helix_core::Rope::from(s)
                    };
                    let n = text.len_lines();
                    if start_line > end_line {
                        anyhow::bail!("start_line ({start_line}) > end_line ({end_line})");
                    }
                    let start_char = text.line_to_char(start_line.min(n));
                    let end_char = text.line_to_char((end_line + 1).min(n));
                    Ok(text.slice(start_char..end_char).to_string())
                })();
                let _ = reply.send(result);
            }
            McpCommand::GetOpenBuffers { reply } => {
                let buffers = self
                    .editor
                    .documents()
                    .filter_map(|doc| {
                        doc.path().map(|p| BufferInfo {
                            path: p.clone(),
                            language: doc.language_name().map(String::from),
                            is_modified: doc.is_modified(),
                            line_count: doc.text().len_lines(),
                            lsp_servers: doc
                                .language_servers()
                                .map(|ls| ls.name().to_owned())
                                .collect(),
                        })
                    })
                    .collect();
                let _ = reply.send(buffers);
            }

            // ── write operations ─────────────────────────────────────────────

            McpCommand::RequestPermission {
                tool_name,
                diff,
                reply,
            } => {
                if helix_mcp::auto_approve() {
                    if let Some(tx) = reply.lock().unwrap().take() {
                        let _ = tx.send(true);
                    }
                    return;
                }
                use crate::ui::PromptEvent;
                let diff_preview = Self::mcp_diff_preview(&diff);
                let message = if diff_preview.is_empty() {
                    format!("MCP {tool_name}: permission required")
                } else {
                    format!("MCP {tool_name}: permission required\n\n{diff_preview}")
                };
                let select = ui::Select::new(
                    message,
                    [McpApproveAction::Apply, McpApproveAction::Cancel],
                    (),
                    move |_editor, action, event| {
                        if event == PromptEvent::Update { return; }
                        let approved = event == PromptEvent::Validate
                            && matches!(action, McpApproveAction::Apply);
                        if let Some(tx) = reply.lock().unwrap().take() {
                            let _ = tx.send(approved);
                        }
                    },
                )
                .no_auto_close()
                .with_id("mcp-permission");
                self.compositor.replace_or_push("mcp-permission", select);
            }

            McpCommand::WriteFile {
                path,
                content,
                reply,
            } => {
                use crate::ui::PromptEvent;
                use std::sync::Mutex;

                let old = Self::mcp_read_content(&self.editor, &path);
                let diff = Self::mcp_unified_diff(&old, &content, &path);
                let lines_changed = diff
                    .lines()
                    .filter(|l| {
                        (l.starts_with('+') || l.starts_with('-'))
                            && !l.starts_with("+++")
                            && !l.starts_with("---")
                    })
                    .count();

                // Auto-approve: skip prompt and immediately write.
                if helix_mcp::auto_approve() {
                    let result = std::fs::write(&path, &content)
                        .map_err(anyhow::Error::from)
                        .map(|_| {
                            if self.editor.document_by_path(&path).is_some() {
                                self.editor.acp_reload_document(&path);
                            } else if self.editor.mcp_trace {
                                if let Err(e) =
                                    self.editor.open(&path, helix_view::editor::Action::Load)
                                {
                                    log::warn!(
                                        "MCP write_file: could not open {}: {e}",
                                        path.display()
                                    );
                                }
                            }
                            Self::mcp_trace_jump(&mut self.editor, &path, 0);
                            WriteResult {
                                path: path.clone(),
                                lines_changed,
                                saved: true,
                            }
                        });
                    let _ = reply.send(result);
                    return;
                }
                let diff_preview = Self::mcp_diff_preview(&diff);
                let message = format!(
                    "write_file '{}' — {lines_changed} line(s) changed\n\n{diff_preview}",
                    path.display()
                );
                let reply = Arc::new(Mutex::new(Some(reply)));
                let path2 = path.clone();
                let content2 = content.clone();
                let select = ui::Select::new(
                    message,
                    [McpApproveAction::Apply, McpApproveAction::Cancel],
                    (),
                    move |editor, action, event| {
                        if event == PromptEvent::Update { return; }
                        let result: anyhow::Result<WriteResult> =
                            if event == PromptEvent::Validate
                                && matches!(action, McpApproveAction::Apply)
                            {
                                std::fs::write(&path2, &content2)
                                    .map_err(anyhow::Error::from)
                                    .map(|_| {
                                        if editor.document_by_path(&path2).is_some() {
                                            editor.acp_reload_document(&path2);
                                        } else if editor.mcp_trace {
                                            let _ = editor
                                                .open(&path2, helix_view::editor::Action::Load);
                                        }
                                        Self::mcp_trace_jump(editor, &path2, 0);
                                        WriteResult {
                                            path: path2.clone(),
                                            lines_changed,
                                            saved: true,
                                        }
                                    })
                            } else {
                                Err(anyhow::anyhow!("Permission denied by user"))
                            };
                        if let Some(tx) = reply.lock().unwrap().take() {
                            let _ = tx.send(result);
                        }
                    },
                )
                .no_auto_close()
                .with_id("mcp-write");
                self.compositor.replace_or_push("mcp-write", select);
            }


            McpCommand::ApplyEdits { path, edits, reply } => {
                use crate::ui::PromptEvent;
                use std::sync::Mutex;

                // Compute target line for mcp_trace jump (0-indexed, min start_line).
                let trace_target_line = edits.iter()
                    .map(|e| e.start_line.saturating_sub(1))
                    .min()
                    .unwrap_or(0);

                // Apply edits to the current content string to produce new content.
                let apply_result: anyhow::Result<String> = (|| {
                    let old = Self::mcp_read_content(&self.editor, &path);
                    let mut lines: Vec<String> =
                        old.lines().map(String::from).collect();
                    // Ensure at least one line so 1-indexed arithmetic works.
                    if lines.is_empty() {
                        lines.push(String::new());
                    }

                    // Validate and sort edits bottom-up (highest start_line first)
                    // to preserve line offsets during application.
                    let mut sorted = edits;
                    sorted.sort_by(|a, b| b.start_line.cmp(&a.start_line));

                    // Check for overlaps after sorting.
                    for w in sorted.windows(2) {
                        let upper = &w[0]; // higher line number (comes first after desc sort)
                        let lower = &w[1];
                        // upper.start_line >= lower.start_line (desc order)
                        // overlap if lower.end_line > upper.start_line (end_line is exclusive)
                        if lower.end_line > upper.start_line {
                            anyhow::bail!(
                                "overlapping edits: [{}, {}) and [{}, {})",
                                lower.start_line,
                                lower.end_line,
                                upper.start_line,
                                upper.end_line
                            );
                        }
                    }

                    for edit in &sorted {
                        let n = lines.len();
                        // Convert 1-indexed inclusive start to 0-indexed.
                        let start = edit.start_line.saturating_sub(1).min(n);
                        // end_line is 1-indexed exclusive; convert to 0-indexed exclusive.
                        // Clamp to [start, n] so invalid ranges become empty (insertion).
                        let end = edit.end_line.saturating_sub(1).min(n).max(start);
                        let replacement: Vec<String> = if edit.new_text.is_empty() {
                            vec![]
                        } else {
                            edit.new_text.lines().map(String::from).collect()
                        };
                        lines.splice(start..end, replacement);
                    }
                    let mut new_content = lines.join("\n");
                    // Preserve trailing newline if original had one.
                    if old.ends_with('\n') && !new_content.ends_with('\n') {
                        new_content.push('\n');
                    }
                    Ok(new_content)
                })();

                let new_content = match apply_result {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };

                let old = Self::mcp_read_content(&self.editor, &path);
                let diff = Self::mcp_unified_diff(&old, &new_content, &path);
                let lines_changed = diff
                    .lines()
                    .filter(|l| {
                        (l.starts_with('+') || l.starts_with('-'))
                            && !l.starts_with("+++")
                            && !l.starts_with("---")
                    })
                    .count();
                // Auto-approve: skip prompt and immediately write.
                if helix_mcp::auto_approve() {
                    let result = std::fs::write(&path, &new_content)
                        .map_err(anyhow::Error::from)
                        .map(|_| {
                            if self.editor.document_by_path(&path).is_some() {
                                self.editor.acp_reload_document(&path);
                            } else if self.editor.mcp_trace {
                                if let Err(e) =
                                    self.editor.open(&path, helix_view::editor::Action::Load)
                                {
                                    log::warn!(
                                        "MCP edit_file: could not open {}: {e}",
                                        path.display()
                                    );
                                }
                            }
                            Self::mcp_trace_jump(&mut self.editor, &path, trace_target_line);
                            WriteResult {
                                path: path.clone(),
                                lines_changed,
                                saved: true,
                            }
                        });
                    let _ = reply.send(result);
                    return;
                }
                let diff_preview = Self::mcp_diff_preview(&diff);
                let message = format!(
                    "edit_file '{}' — {lines_changed} line(s) changed\n\n{diff_preview}",
                    path.display()
                );
                let reply = Arc::new(Mutex::new(Some(reply)));
                let path2 = path.clone();
                let content2 = new_content;
                let trace_target_line2 = trace_target_line;
                let select = ui::Select::new(
                    message,
                    [McpApproveAction::Apply, McpApproveAction::Cancel],
                    (),
                    move |editor, action, event| {
                        if event == PromptEvent::Update { return; }
                        let result: anyhow::Result<WriteResult> =
                            if event == PromptEvent::Validate
                                && matches!(action, McpApproveAction::Apply)
                            {
                                std::fs::write(&path2, &content2)
                                    .map_err(anyhow::Error::from)
                                    .map(|_| {
                                        if editor.document_by_path(&path2).is_some() {
                                            editor.acp_reload_document(&path2);
                                        } else if editor.mcp_trace {
                                            let _ = editor
                                                .open(&path2, helix_view::editor::Action::Load);
                                        }
                                        Self::mcp_trace_jump(
                                            editor, &path2, trace_target_line2,
                                        );
                                        WriteResult {
                                            path: path2.clone(),
                                            lines_changed,
                                            saved: true,
                                        }
                                    })
                            } else {
                                Err(anyhow::anyhow!("Permission denied by user"))
                            };
                        if let Some(tx) = reply.lock().unwrap().take() {
                            let _ = tx.send(result);
                        }
                    },
                )
                .no_auto_close()
                .with_id("mcp-edit");
                self.compositor.replace_or_push("mcp-edit", select);
            }


            McpCommand::InsertText {
                path,
                line,
                text,
                reply,
            } => {
                use helix_mcp::TextEdit;
                // Convert insert to a single ApplyEdits call: end_line == start_line = insertion.
                let edits = vec![TextEdit {
                    start_line: line,
                    end_line: line,
                    new_text: text,
                }];
                // Re-dispatch as ApplyEdits.
                Box::pin(self.handle_mcp_command(McpCommand::ApplyEdits {
                    path,
                    edits,
                    reply,
                }))
                .await;
            }


            McpCommand::EditFile {
                path,
                old_string,
                new_string,
                start_line,
                end_line,
                replace_all,
                reply,
            } => {
                let old = Self::mcp_read_content(&self.editor, &path);

                let apply_result: anyhow::Result<String> = (|| {
                    match old_string {
                        Some(find) => {
                            // When a line range is given, verify the match exists within it.
                            let in_scope = if start_line.is_some() || end_line.is_some() {
                                let lines: Vec<&str> = old.lines().collect();
                                let n = lines.len();
                                let s = start_line
                                    .map(|l| l.saturating_sub(1).min(n))
                                    .unwrap_or(0);
                                let e = end_line
                                    .map(|l| l.saturating_sub(1).min(n))
                                    .unwrap_or(n);
                                lines[s..e].join("\n").contains(find.as_str())
                            } else {
                                old.contains(find.as_str())
                            };
                            if !in_scope {
                                anyhow::bail!(
                                    "old_string not found{}",
                                    if start_line.is_some() {
                                        " within the specified line range"
                                    } else {
                                        " in the file"
                                    }
                                );
                            }
                            // Replace first or all occurrences in the full file.
                            if replace_all {
                                Ok(old.replace(find.as_str(), &new_string))
                            } else {
                                Ok(old.replacen(find.as_str(), &new_string, 1))
                            }
                        }
                        None => {
                            // Pure line-range replacement.
                            let mut lines: Vec<String> =
                                old.lines().map(String::from).collect();
                            if lines.is_empty() {
                                lines.push(String::new());
                            }
                            let n = lines.len();
                            let s = start_line
                                .unwrap_or(1)
                                .saturating_sub(1)
                                .min(n);
                            let e = end_line
                                .unwrap_or(usize::MAX)
                                .saturating_sub(1)
                                .min(n)
                                .max(s);
                            let replacement: Vec<String> = if new_string.is_empty() {
                                vec![]
                            } else {
                                new_string.lines().map(String::from).collect()
                            };
                            lines.splice(s..e, replacement);
                            let mut new_content = lines.join("\n");
                            if old.ends_with('\n') && !new_content.ends_with('\n') {
                                new_content.push('\n');
                            }
                            Ok(new_content)
                        }
                    }
                })();

                match apply_result {
                    Ok(new_content) => {
                        // Re-dispatch as WriteFile to reuse diff-preview + approval UI.
                        Box::pin(self.handle_mcp_command(McpCommand::WriteFile {
                            path,
                            content: new_content,
                            reply,
                        }))
                        .await;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }

            McpCommand::RenameSymbol {
                path,
                line,
                col,
                new_name,
                reply,
            } => {
                use crate::ui::PromptEvent;
                use helix_core::syntax::config::LanguageServerFeature;
                use helix_lsp::block_on;
                use std::sync::Mutex;

                // Ensure the document is open so LSP can operate on it.
                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                // Borrow doc + ls to extract all needed data, then release the borrow.
                // rename_symbol() returns an owned future so we can hold it after the borrow ends.
                type RenameData = (
                    helix_lsp::OffsetEncoding,
                    std::pin::Pin<
                        Box<
                            dyn std::future::Future<
                                Output = helix_lsp::Result<Option<helix_lsp::lsp::WorkspaceEdit>>,
                            >,
                        >,
                    >,
                );
                let rename_data: anyhow::Result<RenameData> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;

                    let text = doc.text();
                    let n_lines = text.len_lines();
                    let ls = doc
                        .language_servers_with_feature(LanguageServerFeature::RenameSymbol)
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no language server with rename support for {}",
                                path.display()
                            )
                        })?;

                    let offset_encoding = ls.offset_encoding();
                    let char_idx = {
                        let l = line.min(n_lines.saturating_sub(1));
                        let line_start = text.line_to_char(l);
                        let line_len = text.line(l).len_chars();
                        line_start + col.min(line_len.saturating_sub(1))
                    };
                    let lsp_pos = helix_lsp::util::pos_to_lsp_pos(text, char_idx, offset_encoding);
                    let future = ls
                        .rename_symbol(doc.identifier(), lsp_pos, new_name.clone())
                        .ok_or_else(|| anyhow::anyhow!("LSP does not support rename"))?;
                    Ok((offset_encoding, Box::pin(future) as _))
                })();

                let (offset_encoding, future) = match rename_data {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };

                let workspace_edit = match block_on(future) {
                    Ok(Some(we)) => we,
                    Ok(None) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP returned no rename edits")));
                        return;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP rename error: {e}")));
                        return;
                    }
                };

                // Count affected changes for the prompt summary.
                let changes_count: usize = workspace_edit
                    .changes
                    .as_ref()
                    .map(|m| m.values().map(|v| v.len()).sum())
                    .unwrap_or(0)
                    + workspace_edit
                        .document_changes
                        .as_ref()
                        .map(|dc| match dc {
                            helix_lsp::lsp::DocumentChanges::Edits(edits) => {
                                edits.iter().map(|e| e.edits.len()).sum()
                            }
                            helix_lsp::lsp::DocumentChanges::Operations(ops) => ops.len(),
                        })
                        .unwrap_or(0);
                // Auto-approve: skip prompt and immediately apply.
                if helix_mcp::auto_approve() {
                    let result = self
                        .editor
                        .apply_workspace_edit(offset_encoding, &workspace_edit)
                        .map(|_| WriteResult {
                            path: path.clone(),
                            lines_changed: changes_count,
                            saved: false,
                        })
                        .map_err(|e| anyhow::anyhow!("apply_workspace_edit failed: {e:?}"));
                    let _ = reply.send(result);
                    return;
                }
                let reply = Arc::new(Mutex::new(Some(reply)));
                let message =
                    format!("rename_symbol \u{2192} '{new_name}' \u{2014} {changes_count} edit(s)");
                let select = ui::Select::new(
                    message,
                    [McpApproveAction::Apply, McpApproveAction::Cancel],
                    (),
                    move |editor, action, event| {
                        if event == PromptEvent::Update {
                            return;
                        }
                        let result: anyhow::Result<WriteResult> =
                            if event == PromptEvent::Validate
                                && matches!(action, McpApproveAction::Apply)
                            {
                                editor
                                    .apply_workspace_edit(offset_encoding, &workspace_edit)
                                    .map(|_| WriteResult {
                                        path: path.clone(),
                                        lines_changed: changes_count,
                                        saved: false,
                                    })
                                    .map_err(|e| {
                                        anyhow::anyhow!("apply_workspace_edit failed: {e:?}")
                                    })
                            } else {
                                Err(anyhow::anyhow!("Permission denied by user"))
                            };
                        if let Some(tx) = reply.lock().unwrap().take() {
                            let _ = tx.send(result);
                        }
                    },
                )
                .no_auto_close()
                .with_id("mcp-rename");
                self.compositor.replace_or_push("mcp-rename", select);
            }

            McpCommand::ReplaceSymbol {
                path,
                name_path,
                body,
                reply,
            } => {
                use helix_core::syntax::config::LanguageServerFeature;
                use helix_lsp::block_on;
                use helix_mcp::TextEdit;

                // Ensure document is open.
                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                // Find the symbol range via LSP document_symbols (same logic as ReadSymbol).
                let doc_sym_data: anyhow::Result<_> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;
                    let ls = doc
                        .language_servers_with_feature(LanguageServerFeature::DocumentSymbols)
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no LSP with document-symbols support for {}",
                                path.display()
                            )
                        })?;
                    let future = ls
                        .document_symbols(doc.identifier())
                        .ok_or_else(|| anyhow::anyhow!("LSP does not support document symbols"))?;
                    Ok(Box::pin(future) as std::pin::Pin<Box<dyn std::future::Future<Output = helix_lsp::Result<Option<helix_lsp::lsp::DocumentSymbolResponse>>>>>)
                })();

                let future = match doc_sym_data {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };

                let response = match block_on(future) {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        let _ = reply.send(Err(anyhow::anyhow!("no symbols found in file")));
                        return;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP document_symbols error: {e}")));
                        return;
                    }
                };

                // Resolve name_path to a line range (0-indexed, inclusive).
                let parts: Vec<&str> = name_path.splitn(2, '/').collect();
                let parent_name = parts[0];
                let child_name = parts.get(1).copied();

                fn find_range(
                    syms: Vec<helix_lsp::lsp::DocumentSymbol>,
                    parent: &str,
                    child: Option<&str>,
                ) -> Option<helix_lsp::lsp::Range> {
                    for sym in syms {
                        if sym.name == parent {
                            if let Some(child_name) = child {
                                let children = sym.children.clone().unwrap_or_default();
                                return children
                                    .into_iter()
                                    .find(|c| c.name == child_name)
                                    .map(|c| c.range);
                            } else {
                                return Some(sym.range);
                            }
                        }
                        if let Some(children) = sym.children.clone() {
                            if let Some(found) = find_range(children, parent, child) {
                                return Some(found);
                            }
                        }
                    }
                    None
                }

                fn find_range_flat(
                    syms: Vec<helix_lsp::lsp::SymbolInformation>,
                    name: &str,
                ) -> Option<helix_lsp::lsp::Range> {
                    syms.into_iter()
                        .find(|s| s.name == name)
                        .map(|s| s.location.range)
                }

                let lsp_range = match response {
                    helix_lsp::lsp::DocumentSymbolResponse::Nested(syms) => {
                        find_range(syms, parent_name, child_name)
                    }
                    helix_lsp::lsp::DocumentSymbolResponse::Flat(syms) => {
                        let target = child_name.unwrap_or(parent_name);
                        find_range_flat(syms, target)
                    }
                };

                let lsp_range = match lsp_range {
                    Some(r) => r,
                    None => {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "symbol '{}' not found in {}",
                            name_path,
                            path.display()
                        )));
                        return;
                    }
                };

                // Convert 0-indexed LSP range to 1-indexed TextEdit range (end_line is exclusive).
                let edits = vec![TextEdit {
                    start_line: lsp_range.start.line as usize + 1,
                    end_line: lsp_range.end.line as usize + 2, // +1 for 0→1 indexed, +1 for inclusive→exclusive
                    new_text: body,
                }];

                // Re-dispatch as ApplyEdits (handles diff preview + user approval).
                Box::pin(self.handle_mcp_command(McpCommand::ApplyEdits {
                    path,
                    edits,
                    reply,
                }))
                .await;
            }


            // ── symbol read operations ────────────────────────────────────────

            McpCommand::GetSymbolsOverview { path, depth, reply } => {
                use helix_core::syntax::config::LanguageServerFeature;
                use helix_lsp::block_on;

                // Ensure the document is open so LSP can operate on it.
                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                // Borrow doc + ls, extract future, release borrow.
                let doc_sym_data: anyhow::Result<_> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;
                    let ls = doc
                        .language_servers_with_feature(LanguageServerFeature::DocumentSymbols)
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no LSP with document-symbols support for {}",
                                path.display()
                            )
                        })?;
                    let future = ls
                        .document_symbols(doc.identifier())
                        .ok_or_else(|| anyhow::anyhow!("LSP does not support document symbols"))?;
                    Ok(Box::pin(future) as std::pin::Pin<Box<dyn std::future::Future<Output = helix_lsp::Result<Option<helix_lsp::lsp::DocumentSymbolResponse>>>>>)
                })();

                let future = match doc_sym_data {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };

                let response = match block_on(future) {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        let _ = reply.send(Ok((vec![], "lsp".to_string())));
                        return;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP document_symbols error: {e}")));
                        return;
                    }
                };

                fn lsp_kind_str(kind: helix_lsp::lsp::SymbolKind) -> &'static str {
                    use helix_lsp::lsp::SymbolKind;
                    match kind {
                        SymbolKind::FILE => "file",
                        SymbolKind::MODULE => "module",
                        SymbolKind::NAMESPACE => "namespace",
                        SymbolKind::PACKAGE => "package",
                        SymbolKind::CLASS => "class",
                        SymbolKind::METHOD => "method",
                        SymbolKind::PROPERTY => "property",
                        SymbolKind::FIELD => "field",
                        SymbolKind::CONSTRUCTOR => "constructor",
                        SymbolKind::ENUM => "enum",
                        SymbolKind::INTERFACE => "interface",
                        SymbolKind::FUNCTION => "function",
                        SymbolKind::VARIABLE => "variable",
                        SymbolKind::CONSTANT => "constant",
                        SymbolKind::STRING => "string",
                        SymbolKind::NUMBER => "number",
                        SymbolKind::BOOLEAN => "boolean",
                        SymbolKind::ARRAY => "array",
                        SymbolKind::OBJECT => "object",
                        SymbolKind::KEY => "key",
                        SymbolKind::NULL => "null",
                        SymbolKind::ENUM_MEMBER => "enum_member",
                        SymbolKind::STRUCT => "struct",
                        SymbolKind::EVENT => "event",
                        SymbolKind::OPERATOR => "operator",
                        SymbolKind::TYPE_PARAMETER => "type_parameter",
                        _ => "unknown",
                    }
                }

                fn nested_to_info(
                    sym: helix_lsp::lsp::DocumentSymbol,
                    depth_remaining: u8,
                ) -> helix_mcp::SymbolInfo {
                    let children = if depth_remaining > 0 {
                        sym.children
                            .unwrap_or_default()
                            .into_iter()
                            .map(|c| nested_to_info(c, depth_remaining - 1))
                            .collect()
                    } else {
                        vec![]
                    };
                    helix_mcp::SymbolInfo {
                        name: sym.name,
                        kind: lsp_kind_str(sym.kind).to_string(),
                        range: helix_mcp::LineRange {
                            start_line: sym.range.start.line as usize,
                            end_line: sym.range.end.line as usize,
                        },
                        children,
                    }
                }

                let symbols: Vec<helix_mcp::SymbolInfo> = match response {
                    helix_lsp::lsp::DocumentSymbolResponse::Nested(syms) => syms
                        .into_iter()
                        .map(|s| nested_to_info(s, depth))
                        .collect(),
                    helix_lsp::lsp::DocumentSymbolResponse::Flat(syms) => syms
                        .into_iter()
                        .map(|s| helix_mcp::SymbolInfo {
                            name: s.name,
                            kind: lsp_kind_str(s.kind).to_string(),
                            range: helix_mcp::LineRange {
                                start_line: s.location.range.start.line as usize,
                                end_line: s.location.range.end.line as usize,
                            },
                            children: vec![],
                        })
                        .collect(),
                };

                let _ = reply.send(Ok((symbols, "lsp".to_string())));
            }

            McpCommand::FindSymbol { query, path, include_body, reply } => {
                use helix_core::syntax::config::LanguageServerFeature;
                use helix_lsp::block_on;

                // Open file if path given, so we can find its LSP.
                if let Some(ref p) = path {
                    if self.editor.document_by_path(p).is_none() {
                        let _ = self.editor.open(p, helix_view::editor::Action::Load);
                    }
                }

                // Find any document with WorkspaceSymbols LSP support.
                // Collect needed data before releasing the borrow.
                let ws_sym_data: anyhow::Result<_> = (|| {
                    let (doc, ls) = self.editor.documents().find_map(|doc| {
                        doc.language_servers_with_feature(LanguageServerFeature::WorkspaceSymbols)
                            .next()
                            .map(|ls| (doc, ls))
                    }).ok_or_else(|| anyhow::anyhow!("no LSP with workspace-symbols support"))?;
                    let offset_encoding = ls.offset_encoding();
                    let future = ls
                        .workspace_symbols(query.clone())
                        .ok_or_else(|| anyhow::anyhow!("LSP does not support workspace symbols"))?;
                    let _ = doc; // suppress unused warning
                    Ok((offset_encoding, Box::pin(future) as std::pin::Pin<Box<dyn std::future::Future<Output = helix_lsp::Result<Option<helix_lsp::lsp::WorkspaceSymbolResponse>>>>>))
                })();

                let (_offset_encoding, future) = match ws_sym_data {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };

                let response = match block_on(future) {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        let _ = reply.send(Ok(vec![]));
                        return;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP workspace_symbols error: {e}")));
                        return;
                    }
                };

                fn lsp_sym_kind_str(kind: helix_lsp::lsp::SymbolKind) -> &'static str {
                    use helix_lsp::lsp::SymbolKind;
                    match kind {
                        SymbolKind::FILE => "file",
                        SymbolKind::MODULE => "module",
                        SymbolKind::NAMESPACE => "namespace",
                        SymbolKind::PACKAGE => "package",
                        SymbolKind::CLASS => "class",
                        SymbolKind::METHOD => "method",
                        SymbolKind::PROPERTY => "property",
                        SymbolKind::FIELD => "field",
                        SymbolKind::CONSTRUCTOR => "constructor",
                        SymbolKind::ENUM => "enum",
                        SymbolKind::INTERFACE => "interface",
                        SymbolKind::FUNCTION => "function",
                        SymbolKind::VARIABLE => "variable",
                        SymbolKind::CONSTANT => "constant",
                        SymbolKind::STRING => "string",
                        SymbolKind::NUMBER => "number",
                        SymbolKind::BOOLEAN => "boolean",
                        SymbolKind::ARRAY => "array",
                        SymbolKind::OBJECT => "object",
                        SymbolKind::KEY => "key",
                        SymbolKind::NULL => "null",
                        SymbolKind::ENUM_MEMBER => "enum_member",
                        SymbolKind::STRUCT => "struct",
                        SymbolKind::EVENT => "event",
                        SymbolKind::OPERATOR => "operator",
                        SymbolKind::TYPE_PARAMETER => "type_parameter",
                        _ => "unknown",
                    }
                }

                let raw_matches: Vec<(String, helix_lsp::lsp::SymbolKind, std::path::PathBuf, helix_lsp::lsp::Range)> =
                    match response {
                        helix_lsp::lsp::WorkspaceSymbolResponse::Flat(syms) => syms
                            .into_iter()
                            .filter_map(|s| {
                                let sym_path = s.location.uri.to_file_path().ok()?;
                                Some((s.name, s.kind, sym_path, s.location.range))
                            })
                            .collect(),
                        helix_lsp::lsp::WorkspaceSymbolResponse::Nested(syms) => syms
                            .into_iter()
                            .filter_map(|s| {
                                let loc = match s.location {
                                    helix_lsp::lsp::OneOf::Left(l) => l,
                                    helix_lsp::lsp::OneOf::Right(_) => return None,
                                };
                                let sym_path = loc.uri.to_file_path().ok()?;
                                Some((s.name, s.kind, sym_path, loc.range))
                            })
                            .collect(),
                    };

                // Filter by path prefix if given.
                let filtered: Vec<_> = if let Some(ref filter_path) = path {
                    raw_matches
                        .into_iter()
                        .filter(|(_, _, sym_path, _)| sym_path.starts_with(filter_path))
                        .collect()
                } else {
                    raw_matches
                };

                // Build SymbolMatch list, optionally reading body.
                let mut result: Vec<helix_mcp::SymbolMatch> = Vec::new();
                for (name, kind, sym_path, range) in filtered {
                    let body = if include_body {
                        let start = range.start.line as usize;
                        let end = range.end.line as usize;
                        let text = if let Some(doc) = self.editor.document_by_path(&sym_path) {
                            doc.text().clone()
                        } else if let Ok(content) = std::fs::read_to_string(&sym_path) {
                            helix_core::Rope::from(content)
                        } else {
                            helix_core::Rope::new()
                        };
                        let n = text.len_lines();
                        let start_char = text.line_to_char(start.min(n));
                        let end_char = text.line_to_char((end + 1).min(n));
                        Some(text.slice(start_char..end_char).to_string())
                    } else {
                        None
                    };
                    result.push(helix_mcp::SymbolMatch {
                        name,
                        kind: lsp_sym_kind_str(kind).to_string(),
                        path: sym_path,
                        range: helix_mcp::LineRange {
                            start_line: range.start.line as usize,
                            end_line: range.end.line as usize,
                        },
                        body,
                    });
                }

                let _ = reply.send(Ok(result));
            }

            McpCommand::FindRefs { path, line, col, reply } => {
                use helix_core::syntax::config::LanguageServerFeature;
                use helix_lsp::block_on;

                // Ensure the document is open.
                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                let refs_data: anyhow::Result<_> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;
                    let text = doc.text();
                    let n_lines = text.len_lines();
                    let ls = doc
                        .language_servers_with_feature(LanguageServerFeature::GotoReference)
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no LSP with goto-reference support for {}",
                                path.display()
                            )
                        })?;
                    let offset_encoding = ls.offset_encoding();
                    let char_idx = {
                        let l = line.min(n_lines.saturating_sub(1));
                        let line_start = text.line_to_char(l);
                        let line_len = text.line(l).len_chars();
                        line_start + col.min(line_len.saturating_sub(1))
                    };
                    let lsp_pos = helix_lsp::util::pos_to_lsp_pos(text, char_idx, offset_encoding);
                    let future = ls
                        .goto_reference(doc.identifier(), lsp_pos, true, None)
                        .ok_or_else(|| anyhow::anyhow!("LSP does not support goto-reference"))?;
                    Ok(Box::pin(future) as std::pin::Pin<Box<dyn std::future::Future<Output = helix_lsp::Result<Option<Vec<helix_lsp::lsp::Location>>>>>>)
                })();

                let future = match refs_data {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };

                let lsp_locations = match block_on(future) {
                    Ok(Some(locs)) => locs,
                    Ok(None) => vec![],
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP references error: {e}")));
                        return;
                    }
                };

                let mut ref_locations: Vec<helix_mcp::RefLocation> = Vec::new();
                for loc in lsp_locations {
                    let ref_path = match loc.uri.to_file_path() {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    let ref_line = loc.range.start.line as usize;
                    let ref_col = loc.range.start.character as usize;
                    let preview = {
                        let text = if let Some(doc) = self.editor.document_by_path(&ref_path) {
                            doc.text().clone()
                        } else if let Ok(content) = std::fs::read_to_string(&ref_path) {
                            helix_core::Rope::from(content)
                        } else {
                            helix_core::Rope::new()
                        };
                        let n = text.len_lines();
                        if ref_line < n {
                            text.line(ref_line).to_string().trim_end_matches('\n').to_string()
                        } else {
                            String::new()
                        }
                    };
                    ref_locations.push(helix_mcp::RefLocation {
                        path: ref_path,
                        line: ref_line,
                        col: ref_col,
                        preview,
                    });
                }

                let _ = reply.send(Ok(ref_locations));
            }

            McpCommand::ReadSymbol { path, name_path, reply } => {
                use helix_core::syntax::config::LanguageServerFeature;
                use helix_lsp::block_on;

                // Ensure the document is open.
                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                let doc_sym_data2: anyhow::Result<_> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;
                    let ls = doc
                        .language_servers_with_feature(LanguageServerFeature::DocumentSymbols)
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no LSP with document-symbols support for {}",
                                path.display()
                            )
                        })?;
                    let future = ls
                        .document_symbols(doc.identifier())
                        .ok_or_else(|| anyhow::anyhow!("LSP does not support document symbols"))?;
                    Ok(Box::pin(future) as std::pin::Pin<Box<dyn std::future::Future<Output = helix_lsp::Result<Option<helix_lsp::lsp::DocumentSymbolResponse>>>>>)
                })();

                let future = match doc_sym_data2 {
                    Ok(f) => f,
                    Err(_) => {
                        // No LSP — fall back to scanning the already-loaded buffer text.
                        // The file was opened with Action::Load above, so doc is in the editor.
                        let text = match self.editor.document_by_path(&path) {
                            Some(doc) => doc.text().clone(),
                            None => {
                                let _ = reply.send(Err(anyhow::anyhow!(
                                    "could not read {}", path.display()
                                )));
                                return;
                            }
                        };

                        // Parse name_path.
                        let parts: Vec<&str> = name_path.splitn(2, '/').collect();
                        let parent_name = parts[0];
                        let child_name = parts.get(1).copied();
                        let target = child_name.unwrap_or(parent_name);

                        // Returns true if `line` contains `name` as a whole word.
                        fn has_word(line: &str, name: &str) -> bool {
                            let mut s = line;
                            while let Some(pos) = s.find(name) {
                                let before_ok = pos == 0
                                    || !s.as_bytes()[pos - 1].is_ascii_alphanumeric()
                                        && s.as_bytes()[pos - 1] != b'_';
                                let end = pos + name.len();
                                let after_ok = end >= s.len()
                                    || !s.as_bytes()[end].is_ascii_alphanumeric()
                                        && s.as_bytes()[end] != b'_';
                                if before_ok && after_ok {
                                    return true;
                                }
                                s = &s[pos + 1..];
                            }
                            false
                        }

                        // Find the end line by tracking brace depth from `start`.
                        // Returns `start` for single-line / brace-less definitions.
                        fn find_extent(text: &helix_core::Rope, start: usize) -> usize {
                            let n = text.len_lines();
                            let mut depth: i32 = 0;
                            let mut seen_open = false;
                            for li in start..n {
                                let line = text.line(li).to_string();
                                for ch in line.chars() {
                                    match ch {
                                        '{' => { depth += 1; seen_open = true; }
                                        '}' => { depth -= 1; }
                                        _ => {}
                                    }
                                }
                                if seen_open && depth <= 0 {
                                    return li;
                                }
                            }
                            start
                        }

                        let n = text.len_lines();
                        let mut scan_result: Option<(usize, usize)> = None;

                        if child_name.is_some() {
                            'outer: for li in 0..n {
                                if has_word(&text.line(li).to_string(), parent_name) {
                                    let parent_end = find_extent(&text, li);
                                    for ci in (li + 1)..=parent_end.min(n.saturating_sub(1)) {
                                        if has_word(&text.line(ci).to_string(), target) {
                                            scan_result = Some((ci, find_extent(&text, ci)));
                                            break 'outer;
                                        }
                                    }
                                    break;
                                }
                            }
                        } else {
                            for li in 0..n {
                                if has_word(&text.line(li).to_string(), target) {
                                    scan_result = Some((li, find_extent(&text, li)));
                                    break;
                                }
                            }
                        }

                        let (start_line, end_line) = match scan_result {
                            Some(r) => r,
                            None => {
                                let _ = reply.send(Err(anyhow::anyhow!(
                                    "symbol '{}' not found in {} (no LSP, text scan used)",
                                    name_path,
                                    path.display()
                                )));
                                return;
                            }
                        };

                        let sym_range = helix_mcp::LineRange { start_line, end_line };
                        let start_char = text.line_to_char(start_line.min(n));
                        let end_char   = text.line_to_char((end_line + 1).min(n));
                        let body = text.slice(start_char..end_char).to_string();

                        let _ = reply.send(Ok(helix_mcp::SymbolMatch {
                            name: target.to_string(),
                            kind: "unknown".to_string(),
                            path,
                            range: sym_range,
                            body: Some(body),
                        }));
                        return;
                    }
                };


                let response = match block_on(future) {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        let _ = reply.send(Err(anyhow::anyhow!("no symbols found in file")));
                        return;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP document_symbols error: {e}")));
                        return;
                    }
                };

                // Parse name_path: split on '/'.
                let parts: Vec<&str> = name_path.splitn(2, '/').collect();
                let parent_name = parts[0];
                let child_name = parts.get(1).copied();

                fn find_nested(
                    syms: Vec<helix_lsp::lsp::DocumentSymbol>,
                    parent: &str,
                    child: Option<&str>,
                ) -> Option<(helix_lsp::lsp::DocumentSymbol, Option<helix_lsp::lsp::DocumentSymbol>)> {
                    for sym in syms {
                        if sym.name == parent {
                            if let Some(child_name) = child {
                                let children = sym.children.clone().unwrap_or_default();
                                let child_sym = children.into_iter().find(|c| c.name == child_name)?;
                                return Some((sym, Some(child_sym)));
                            } else {
                                return Some((sym, None));
                            }
                        }
                        // recurse into children
                        if let Some(children) = sym.children.clone() {
                            if let Some(found) = find_nested(children, parent, child) {
                                return Some(found);
                            }
                        }
                    }
                    None
                }

                fn find_flat(
                    syms: Vec<helix_lsp::lsp::SymbolInformation>,
                    name: &str,
                ) -> Option<helix_lsp::lsp::SymbolInformation> {
                    syms.into_iter().find(|s| s.name == name)
                }

                let found = match response {
                    helix_lsp::lsp::DocumentSymbolResponse::Nested(syms) => {
                        match find_nested(syms, parent_name, child_name) {
                            Some((parent_sym, None)) => {
                                let range = helix_mcp::LineRange {
                                    start_line: parent_sym.range.start.line as usize,
                                    end_line: parent_sym.range.end.line as usize,
                                };
                                Some((parent_sym.name, parent_sym.kind, range))
                            }
                            Some((_, Some(child_sym))) => {
                                let range = helix_mcp::LineRange {
                                    start_line: child_sym.range.start.line as usize,
                                    end_line: child_sym.range.end.line as usize,
                                };
                                Some((child_sym.name, child_sym.kind, range))
                            }
                            None => None,
                        }
                    }
                    helix_lsp::lsp::DocumentSymbolResponse::Flat(syms) => {
                        let target = child_name.unwrap_or(parent_name);
                        find_flat(syms, target).map(|s| {
                            let range = helix_mcp::LineRange {
                                start_line: s.location.range.start.line as usize,
                                end_line: s.location.range.end.line as usize,
                            };
                            (s.name, s.kind, range)
                        })
                    }
                };

                let (sym_name, sym_kind, sym_range) = match found {
                    Some(t) => t,
                    None => {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "symbol '{}' not found in {}",
                            name_path,
                            path.display()
                        )));
                        return;
                    }
                };

                // Read the body.
                let body = {
                    let text = if let Some(doc) = self.editor.document_by_path(&path) {
                        doc.text().clone()
                    } else if let Ok(content) = std::fs::read_to_string(&path) {
                        helix_core::Rope::from(content)
                    } else {
                        helix_core::Rope::new()
                    };
                    let n = text.len_lines();
                    let start_char = text.line_to_char(sym_range.start_line.min(n));
                    let end_char = text.line_to_char((sym_range.end_line + 1).min(n));
                    text.slice(start_char..end_char).to_string()
                };

                fn kind_str2(kind: helix_lsp::lsp::SymbolKind) -> &'static str {
                    use helix_lsp::lsp::SymbolKind;
                    match kind {
                        SymbolKind::FILE => "file",
                        SymbolKind::MODULE => "module",
                        SymbolKind::NAMESPACE => "namespace",
                        SymbolKind::PACKAGE => "package",
                        SymbolKind::CLASS => "class",
                        SymbolKind::METHOD => "method",
                        SymbolKind::PROPERTY => "property",
                        SymbolKind::FIELD => "field",
                        SymbolKind::CONSTRUCTOR => "constructor",
                        SymbolKind::ENUM => "enum",
                        SymbolKind::INTERFACE => "interface",
                        SymbolKind::FUNCTION => "function",
                        SymbolKind::VARIABLE => "variable",
                        SymbolKind::CONSTANT => "constant",
                        SymbolKind::STRING => "string",
                        SymbolKind::NUMBER => "number",
                        SymbolKind::BOOLEAN => "boolean",
                        SymbolKind::ARRAY => "array",
                        SymbolKind::OBJECT => "object",
                        SymbolKind::KEY => "key",
                        SymbolKind::NULL => "null",
                        SymbolKind::ENUM_MEMBER => "enum_member",
                        SymbolKind::STRUCT => "struct",
                        SymbolKind::EVENT => "event",
                        SymbolKind::OPERATOR => "operator",
                        SymbolKind::TYPE_PARAMETER => "type_parameter",
                        _ => "unknown",
                    }
                }

                let _ = reply.send(Ok(helix_mcp::SymbolMatch {
                    name: sym_name,
                    kind: kind_str2(sym_kind).to_string(),
                    path,
                    range: sym_range,
                    body: Some(body),
                }));
            }

            McpCommand::GetCursor { reply } => {
                use helix_view::document::Mode;
                let (view, doc) = helix_view::current_ref!(self.editor);
                let sel = doc.selection(view.id);
                let text = doc.text();
                let cursor = sel.primary().cursor(text.slice(..));
                let line = text.char_to_line(cursor);
                let col = cursor - text.line_to_char(line);
                let mode = match self.editor.mode() {
                    Mode::Normal => helix_mcp::EditorMode::Normal,
                    Mode::Insert => helix_mcp::EditorMode::Insert,
                    Mode::Select => helix_mcp::EditorMode::Select,
                };
                let _ = reply.send(helix_mcp::CursorState {
                    path: doc.path().map(|p| p.to_owned()),
                    line: line + 1,
                    col: col + 1,
                    mode,
                    selection_count: sel.len(),
                });
            }

            McpCommand::GetSelections { path, reply } => {
                // Ensure document is open.
                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                let result: anyhow::Result<Vec<helix_mcp::SelectionRange>> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;
                    let doc_id = doc.id();
                    // Find a view showing this document.
                    let view_id = self
                        .editor
                        .tree
                        .views()
                        .find(|(v, _)| v.doc == doc_id)
                        .map(|(v, _)| v.id)
                        .unwrap_or(self.editor.tree.focus);
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found"))?;
                    let text = doc.text();
                    let sel = doc.selection(view_id);
                    let primary_idx = sel.primary_index();
                    let ranges: Vec<helix_mcp::SelectionRange> = sel
                        .iter()
                        .enumerate()
                        .map(|(i, range)| {
                            let anchor = range.anchor;
                            let head = range.head;
                            let anchor_line = text.char_to_line(anchor);
                            let anchor_col = anchor - text.line_to_char(anchor_line);
                            let head_line = text.char_to_line(head);
                            let head_col = head - text.line_to_char(head_line);
                            let frag_start = anchor.min(head);
                            let frag_end = anchor.max(head);
                            let selected_text = text.slice(frag_start..frag_end).to_string();
                            helix_mcp::SelectionRange {
                                anchor_line,
                                anchor_col,
                                head_line,
                                head_col,
                                is_primary: i == primary_idx,
                                text: selected_text,
                            }
                        })
                        .collect();
                    Ok(ranges)
                })();
                let _ = reply.send(result);
            }

            McpCommand::GetViewport { path, reply } => {
                // Ensure document is open.
                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                let result: anyhow::Result<helix_mcp::ViewportInfo> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;
                    let doc_id = doc.id();
                    let (view_id, height) = self
                        .editor
                        .tree
                        .views()
                        .find(|(v, _)| v.doc == doc_id)
                        .map(|(v, _)| (v.id, v.inner_height()))
                        .unwrap_or_else(|| {
                            let fv = self.editor.tree.focus;
                            let h = self
                                .editor
                                .tree
                                .views()
                                .find(|(v, _)| v.id == fv)
                                .map(|(v, _)| v.inner_height())
                                .unwrap_or(24);
                            (fv, h)
                        });
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found"))?;
                    let text = doc.text();
                    let offset = doc.view_offset(view_id);
                    let first_line = text.char_to_line(offset.anchor.min(text.len_chars()));
                    Ok(helix_mcp::ViewportInfo {
                        first_visible_line: first_line + 1,
                        last_visible_line: first_line + height,
                        height_lines: height,
                        horizontal_offset: offset.horizontal_offset,
                    })
                })();
                let _ = reply.send(result);
            }

            McpCommand::GetDiagnostics { path, reply } => {
                fn diag_to_item(
                    p: std::path::PathBuf,
                    d: &helix_core::diagnostic::Diagnostic,
                ) -> helix_mcp::DiagnosticItem {
                    use helix_core::diagnostic::{NumberOrString, Severity};
                    let severity = match d.severity {
                        Some(Severity::Error) => "error",
                        Some(Severity::Warning) => "warning",
                        Some(Severity::Info) => "info",
                        _ => "hint",
                    }
                    .to_string();
                    let code = d.code.as_ref().map(|c| match c {
                        NumberOrString::Number(n) => n.to_string(),
                        NumberOrString::String(s) => s.clone(),
                    });
                    helix_mcp::DiagnosticItem {
                        path: p,
                        line: d.line,
                        col: d.range.start,
                        severity,
                        message: d.message.clone(),
                        source: d.source.clone(),
                        code,
                    }
                }

                let items = if let Some(p) = path {
                    self.editor
                        .document_by_path(&p)
                        .map(|doc| {
                            doc.diagnostics()
                                .iter()
                                .map(|d| diag_to_item(p.clone(), d))
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    self.editor
                        .documents()
                        .flat_map(|doc| {
                            let p = doc.path().map(|x| x.to_owned()).unwrap_or_default();
                            doc.diagnostics()
                                .iter()
                                .map(move |d| diag_to_item(p.clone(), d))
                                .collect::<Vec<_>>()
                        })
                        .collect()
                };
                let _ = reply.send(items);
            }

            McpCommand::Hover { path, line, col, reply } => {
                use helix_core::syntax::config::LanguageServerFeature;
                use helix_lsp::block_on;

                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                let hover_data: anyhow::Result<_> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;
                    let ls = doc
                        .language_servers_with_feature(LanguageServerFeature::Hover)
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!("no LSP with hover support for {}", path.display())
                        })?;
                    let offset_encoding = ls.offset_encoding();
                    let text = doc.text();
                    let n_lines = text.len_lines();
                    let l = line.min(n_lines.saturating_sub(1));
                    let line_start = text.line_to_char(l);
                    let line_len = text.line(l).len_chars();
                    let char_idx = line_start + col.min(line_len.saturating_sub(1));
                    let lsp_pos = helix_lsp::util::pos_to_lsp_pos(text, char_idx, offset_encoding);
                    let future = ls
                        .text_document_hover(doc.identifier(), lsp_pos, None)
                        .ok_or_else(|| anyhow::anyhow!("LSP does not support hover"))?;
                    Ok(Box::pin(future)
                        as std::pin::Pin<
                            Box<
                                dyn std::future::Future<
                                    Output = helix_lsp::Result<Option<helix_lsp::lsp::Hover>>,
                                >,
                            >,
                        >)
                })();

                let future = match hover_data {
                    Ok(f) => f,
                    Err(e) => {
                        // No LSP or other setup error — return Ok(None), not an error.
                        let _ = reply.send(Ok(None));
                        let _ = e; // suppress warning
                        return;
                    }
                };

                let hover_result = match block_on(future) {
                    Ok(Some(h)) => h,
                    Ok(None) => {
                        let _ = reply.send(Ok(None));
                        return;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP hover error: {e}")));
                        return;
                    }
                };

                fn marked_string_value(ms: helix_lsp::lsp::MarkedString) -> String {
                    match ms {
                        helix_lsp::lsp::MarkedString::String(s) => s,
                        helix_lsp::lsp::MarkedString::LanguageString(ls) => ls.value,
                    }
                }
                let text = match hover_result.contents {
                    helix_lsp::lsp::HoverContents::Markup(m) => m.value,
                    helix_lsp::lsp::HoverContents::Scalar(s) => marked_string_value(s),
                    helix_lsp::lsp::HoverContents::Array(a) => a
                        .into_iter()
                        .map(marked_string_value)
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                let _ = reply.send(Ok(Some(text)));
            }

            McpCommand::CodeActions { path, line, col, reply } => {
                use helix_core::syntax::config::LanguageServerFeature;
                use helix_lsp::block_on;
                use helix_lsp::lsp::{CodeActionContext, CodeActionTriggerKind};

                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                let ca_data: anyhow::Result<_> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;
                    let ls = doc
                        .language_servers_with_feature(LanguageServerFeature::CodeAction)
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no LSP with code-action support for {}",
                                path.display()
                            )
                        })?;
                    let offset_encoding = ls.offset_encoding();
                    let text = doc.text();
                    let n_lines = text.len_lines();
                    let l = line.min(n_lines.saturating_sub(1));
                    let line_start = text.line_to_char(l);
                    let line_len = text.line(l).len_chars();
                    let char_idx = line_start + col.min(line_len.saturating_sub(1));
                    let lsp_pos = helix_lsp::util::pos_to_lsp_pos(text, char_idx, offset_encoding);
                    let lsp_range = helix_lsp::lsp::Range {
                        start: lsp_pos,
                        end: lsp_pos,
                    };
                    let context = CodeActionContext {
                        diagnostics: vec![],
                        only: None,
                        trigger_kind: Some(CodeActionTriggerKind::INVOKED),
                    };
                    let future = ls
                        .code_actions(doc.identifier(), lsp_range, context)
                        .ok_or_else(|| anyhow::anyhow!("LSP does not support code actions"))?;
                    Ok(Box::pin(future)
                        as std::pin::Pin<
                            Box<
                                dyn std::future::Future<
                                    Output = helix_lsp::Result<
                                        Option<Vec<helix_lsp::lsp::CodeActionOrCommand>>,
                                    >,
                                >,
                            >,
                        >)
                })();

                let future = match ca_data {
                    Ok(f) => f,
                    Err(_) => {
                        let _ = reply.send(Ok(vec![]));
                        return;
                    }
                };

                let actions = match block_on(future) {
                    Ok(Some(a)) => a,
                    Ok(None) => vec![],
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP code actions error: {e}")));
                        return;
                    }
                };

                let items: Vec<helix_mcp::CodeActionItem> = actions
                    .into_iter()
                    .map(|a| match a {
                        helix_lsp::lsp::CodeActionOrCommand::CodeAction(ca) => {
                            helix_mcp::CodeActionItem {
                                title: ca.title,
                                kind: ca.kind.map(|k| k.as_str().to_string()),
                            }
                        }
                        helix_lsp::lsp::CodeActionOrCommand::Command(cmd) => {
                            helix_mcp::CodeActionItem {
                                title: cmd.title,
                                kind: None,
                            }
                        }
                    })
                    .collect();
                let _ = reply.send(Ok(items));
            }

            McpCommand::InlayHints { path, start_line, end_line, reply } => {
                use helix_core::syntax::config::LanguageServerFeature;
                use helix_lsp::block_on;

                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                let ih_data: anyhow::Result<_> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;
                    let ls = doc
                        .language_servers_with_feature(LanguageServerFeature::InlayHints)
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no LSP with inlay-hints support for {}",
                                path.display()
                            )
                        })?;
                    let offset_encoding = ls.offset_encoding();
                    let text = doc.text();
                    let n = text.len_lines();
                    let first_char = text.line_to_char(start_line.min(n));
                    let last_char = text.line_to_char(end_line.min(n));
                    let lsp_range = helix_lsp::util::range_to_lsp_range(
                        text,
                        helix_core::Range::new(first_char, last_char),
                        offset_encoding,
                    );
                    let future = ls
                        .text_document_range_inlay_hints(doc.identifier(), lsp_range, None)
                        .ok_or_else(|| anyhow::anyhow!("LSP does not support inlay hints"))?;
                    Ok((
                        Box::pin(future)
                            as std::pin::Pin<
                                Box<
                                    dyn std::future::Future<
                                        Output = helix_lsp::Result<
                                            Option<Vec<helix_lsp::lsp::InlayHint>>,
                                        >,
                                    >,
                                >,
                            >,
                        offset_encoding,
                    ))
                })();

                let (future, offset_encoding) = match ih_data {
                    Ok(p) => p,
                    Err(_) => {
                        let _ = reply.send(Ok(vec![]));
                        return;
                    }
                };

                let raw_hints = match block_on(future) {
                    Ok(Some(h)) => h,
                    Ok(None) => vec![],
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP inlay hints error: {e}")));
                        return;
                    }
                };

                let mut items: Vec<helix_mcp::InlayHintItem> = Vec::new();
                if let Some(doc) = self.editor.document_by_path(&path) {
                    let text = doc.text();
                    for hint in raw_hints {
                        let char_idx = helix_lsp::util::lsp_pos_to_pos(
                            text,
                            hint.position,
                            offset_encoding,
                        );
                        let char_idx = match char_idx {
                            Some(c) => c,
                            None => continue,
                        };
                        let hint_line = text.char_to_line(char_idx);
                        let hint_col = char_idx - text.line_to_char(hint_line);
                        let label = match &hint.label {
                            helix_lsp::lsp::InlayHintLabel::String(s) => s.clone(),
                            helix_lsp::lsp::InlayHintLabel::LabelParts(parts) => parts
                                .iter()
                                .map(|p| p.value.as_str())
                                .collect::<Vec<_>>()
                                .join(""),
                        };
                        let kind = match hint.kind {
                            Some(helix_lsp::lsp::InlayHintKind::TYPE) => "type",
                            Some(helix_lsp::lsp::InlayHintKind::PARAMETER) => "parameter",
                            _ => "other",
                        }
                        .to_string();
                        items.push(helix_mcp::InlayHintItem {
                            line: hint_line,
                            col: hint_col,
                            label,
                            kind,
                        });
                    }
                }
                let _ = reply.send(Ok(items));
            }

            McpCommand::Completions { path, line, col, reply } => {
                use helix_core::syntax::config::LanguageServerFeature;
                use helix_lsp::block_on;
                use helix_lsp::lsp::{CompletionContext, CompletionTriggerKind};

                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                let comp_data: anyhow::Result<_> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;
                    let ls = doc
                        .language_servers_with_feature(LanguageServerFeature::Completion)
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no LSP with completion support for {}",
                                path.display()
                            )
                        })?;
                    let offset_encoding = ls.offset_encoding();
                    let text = doc.text();
                    let n_lines = text.len_lines();
                    let l = line.min(n_lines.saturating_sub(1));
                    let line_start = text.line_to_char(l);
                    let line_len = text.line(l).len_chars();
                    let char_idx = line_start + col.min(line_len.saturating_sub(1));
                    let lsp_pos = helix_lsp::util::pos_to_lsp_pos(text, char_idx, offset_encoding);
                    let context = CompletionContext {
                        trigger_kind: CompletionTriggerKind::INVOKED,
                        trigger_character: None,
                    };
                    let future = ls
                        .completion(doc.identifier(), lsp_pos, None, context)
                        .ok_or_else(|| anyhow::anyhow!("LSP does not support completion"))?;
                    Ok(Box::pin(future)
                        as std::pin::Pin<
                            Box<
                                dyn std::future::Future<
                                    Output = helix_lsp::Result<
                                        Option<helix_lsp::lsp::CompletionResponse>,
                                    >,
                                >,
                            >,
                        >)
                })();

                let future = match comp_data {
                    Ok(f) => f,
                    Err(_) => {
                        let _ = reply.send(Ok(vec![]));
                        return;
                    }
                };

                let comp_items = match block_on(future) {
                    Ok(Some(helix_lsp::lsp::CompletionResponse::Array(items))) => items,
                    Ok(Some(helix_lsp::lsp::CompletionResponse::List(list))) => list.items,
                    Ok(None) => vec![],
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP completion error: {e}")));
                        return;
                    }
                };

                let items: Vec<helix_mcp::McpCompletionItem> = comp_items
                    .into_iter()
                    .map(|item| {
                        use helix_lsp::lsp::CompletionItemKind;
                        let kind = item.kind.map(|k| {
                            match k {
                                CompletionItemKind::TEXT => "text",
                                CompletionItemKind::METHOD => "method",
                                CompletionItemKind::FUNCTION => "function",
                                CompletionItemKind::CONSTRUCTOR => "constructor",
                                CompletionItemKind::FIELD => "field",
                                CompletionItemKind::VARIABLE => "variable",
                                CompletionItemKind::CLASS => "class",
                                CompletionItemKind::INTERFACE => "interface",
                                CompletionItemKind::MODULE => "module",
                                CompletionItemKind::PROPERTY => "property",
                                CompletionItemKind::UNIT => "unit",
                                CompletionItemKind::VALUE => "value",
                                CompletionItemKind::ENUM => "enum",
                                CompletionItemKind::KEYWORD => "keyword",
                                CompletionItemKind::SNIPPET => "snippet",
                                CompletionItemKind::COLOR => "color",
                                CompletionItemKind::FILE => "file",
                                CompletionItemKind::REFERENCE => "reference",
                                CompletionItemKind::FOLDER => "folder",
                                CompletionItemKind::ENUM_MEMBER => "enum_member",
                                CompletionItemKind::CONSTANT => "constant",
                                CompletionItemKind::STRUCT => "struct",
                                CompletionItemKind::EVENT => "event",
                                CompletionItemKind::OPERATOR => "operator",
                                CompletionItemKind::TYPE_PARAMETER => "type_parameter",
                                _ => "unknown",
                            }
                            .to_string()
                        });
                        let insert_text = item
                            .text_edit
                            .as_ref()
                            .map(|te| match te {
                                helix_lsp::lsp::CompletionTextEdit::Edit(e) => {
                                    e.new_text.clone()
                                }
                                helix_lsp::lsp::CompletionTextEdit::InsertAndReplace(e) => {
                                    e.new_text.clone()
                                }
                            })
                            .or_else(|| item.insert_text.clone());
                        helix_mcp::McpCompletionItem {
                            label: item.label,
                            kind,
                            detail: item.detail,
                            insert_text,
                        }
                    })
                    .collect();
                let _ = reply.send(Ok(items));
            }

            McpCommand::SignatureHelp { path, line, col, reply } => {
                use helix_core::syntax::config::LanguageServerFeature;
                use helix_lsp::block_on;

                if self.editor.document_by_path(&path).is_none() {
                    if let Err(e) = self
                        .editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))
                    {
                        let _ = reply.send(Err(e));
                        return;
                    }
                }

                let sig_data: anyhow::Result<_> = (|| {
                    let doc = self
                        .editor
                        .document_by_path(&path)
                        .ok_or_else(|| anyhow::anyhow!("document not found: {}", path.display()))?;
                    let ls = doc
                        .language_servers_with_feature(LanguageServerFeature::SignatureHelp)
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no LSP with signature-help support for {}",
                                path.display()
                            )
                        })?;
                    let offset_encoding = ls.offset_encoding();
                    let text = doc.text();
                    let n_lines = text.len_lines();
                    let l = line.min(n_lines.saturating_sub(1));
                    let line_start = text.line_to_char(l);
                    let line_len = text.line(l).len_chars();
                    let char_idx = line_start + col.min(line_len.saturating_sub(1));
                    let lsp_pos = helix_lsp::util::pos_to_lsp_pos(text, char_idx, offset_encoding);
                    let future = ls
                        .text_document_signature_help(doc.identifier(), lsp_pos, None)
                        .ok_or_else(|| anyhow::anyhow!("LSP does not support signature help"))?;
                    Ok(Box::pin(future)
                        as std::pin::Pin<
                            Box<
                                dyn std::future::Future<
                                    Output = helix_lsp::Result<
                                        Option<helix_lsp::lsp::SignatureHelp>,
                                    >,
                                >,
                            >,
                        >)
                })();

                let future = match sig_data {
                    Ok(f) => f,
                    Err(_) => {
                        // No LSP or setup error — return Ok(None), not an error.
                        let _ = reply.send(Ok(None));
                        return;
                    }
                };

                let sig_result = match block_on(future) {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        let _ = reply.send(Ok(None));
                        return;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("LSP signature_help error: {e}")));
                        return;
                    }
                };

                fn doc_to_string(doc: Option<helix_lsp::lsp::Documentation>) -> Option<String> {
                    match doc {
                        Some(helix_lsp::lsp::Documentation::String(s)) => Some(s),
                        Some(helix_lsp::lsp::Documentation::MarkupContent(m)) => Some(m.value),
                        None => None,
                    }
                }

                fn param_label_str(label: helix_lsp::lsp::ParameterLabel) -> String {
                    match label {
                        helix_lsp::lsp::ParameterLabel::Simple(s) => s,
                        helix_lsp::lsp::ParameterLabel::LabelOffsets([start, end]) => {
                            format!("[{start},{end}]")
                        }
                    }
                }

                let sigs: Vec<helix_mcp::SignatureInfo> = sig_result
                    .signatures
                    .into_iter()
                    .map(|s| helix_mcp::SignatureInfo {
                        label: s.label,
                        documentation: doc_to_string(s.documentation),
                        parameters: s
                            .parameters
                            .unwrap_or_default()
                            .into_iter()
                            .map(|p| helix_mcp::ParameterInfo {
                                label: param_label_str(p.label),
                                documentation: doc_to_string(p.documentation),
                            })
                            .collect(),
                    })
                    .collect();

                let _ = reply.send(Ok(Some(helix_mcp::SignatureHelpInfo {
                    signatures: sigs,
                    active_signature: sig_result.active_signature,
                    active_parameter: sig_result.active_parameter,
                })));
            }


            // ---------------------------------------------------------------
            // DAP: Breakpoints
            // ---------------------------------------------------------------

            McpCommand::GetBreakpoints { path, reply } => {
                let bps: Vec<helix_mcp::BreakpointInfo> = if let Some(ref p) = path {
                    self.editor
                        .breakpoints
                        .get(p)
                        .map(|v| v.iter().map(|b| Self::bp_to_info(p, b)).collect())
                        .unwrap_or_default()
                } else {
                    self.editor
                        .breakpoints
                        .iter()
                        .flat_map(|(p, v)| v.iter().map(|b| Self::bp_to_info(p, b)))
                        .collect()
                };
                let _ = reply.send(bps);
            }


            McpCommand::SetBreakpoint { path, line, condition, reply } => {
                use crate::ui::PromptEvent;
                // Auto-approve: skip prompt and immediately set breakpoint.
                if helix_mcp::auto_approve() {
                    let bp = helix_view::editor::Breakpoint {
                        line,
                        condition: condition.clone(),
                        ..Default::default()
                    };
                    let bps = self.editor.breakpoints.entry(path.clone()).or_default();
                    bps.push(bp);
                    let idx = bps.len() - 1;
                    if let Some(debugger) = self.editor.debug_adapters.get_active_client_mut() {
                        if let Err(e) = helix_view::handlers::dap::breakpoints_changed(
                            debugger,
                            path.clone(),
                            bps,
                        ) {
                            log::warn!("MCP set_breakpoint: DAP sync error: {e}");
                        }
                    }
                    let info = Self::bp_to_info(&path, &self.editor.breakpoints[&path][idx]);
                    if let Some(tx) = reply.lock().unwrap().take() {
                        let _ = tx.send(Ok(info));
                    }
                    return;
                }
                let path2 = path.clone();
                let cond_display = condition
                    .as_deref()
                    .map(|c| format!(" [cond: {c}]"))
                    .unwrap_or_default();
                let message = format!(
                    "set_breakpoint '{}:{}'{}" ,
                    path.display(),
                    line,
                    cond_display
                );
                let select = ui::Select::new(
                    message,
                    [McpApproveAction::Apply, McpApproveAction::Cancel],
                    (),
                    move |editor, action, event| {
                        if event == PromptEvent::Update {
                            return;
                        }
                        let result: anyhow::Result<helix_mcp::BreakpointInfo> =
                            if event == PromptEvent::Validate
                                && matches!(action, McpApproveAction::Apply)
                            {
                                let bp = helix_view::editor::Breakpoint {
                                    line,
                                    condition: condition.clone(),
                                    ..Default::default()
                                };
                                let bps =
                                    editor.breakpoints.entry(path2.clone()).or_default();
                                bps.push(bp);
                                let idx = bps.len() - 1;
                                if let Some(debugger) =
                                    editor.debug_adapters.get_active_client_mut()
                                {
                                    if let Err(e) =
                                        helix_view::handlers::dap::breakpoints_changed(
                                            debugger,
                                            path2.clone(),
                                            bps,
                                        )
                                    {
                                        log::warn!("MCP set_breakpoint: DAP sync error: {e}");
                                    }
                                }
                                let info = Application::bp_to_info(
                                    &path2,
                                    &editor.breakpoints[&path2][idx],
                                );
                                Ok(info)
                            } else {
                                Err(anyhow::anyhow!("Permission denied by user"))
                            };
                        if let Some(tx) = reply.lock().unwrap().take() {
                            let _ = tx.send(result);
                        }
                    },
                )
                .no_auto_close()
                .with_id("mcp-set-breakpoint");
                self.compositor.replace_or_push("mcp-set-breakpoint", select);
            }

            McpCommand::RemoveBreakpoint { path, line, reply } => {
                let result = if let Some(bps) = self.editor.breakpoints.get_mut(&path) {
                    if let Some(idx) = bps.iter().position(|b| b.line == line) {
                        bps.remove(idx);
                        // Sync with active DAP session if one exists.
                        if let Some(debugger) =
                            self.editor.debug_adapters.get_active_client_mut()
                        {
                            let _ = helix_view::handlers::dap::breakpoints_changed(
                                debugger,
                                path,
                                bps,
                            );
                        }
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!("No breakpoint at line {line}"))
                    }
                } else {
                    Err(anyhow::anyhow!("No breakpoints in that file"))
                };
                let _ = reply.send(result);
            }

            // ---------------------------------------------------------------
            // DAP: State
            // ---------------------------------------------------------------

            McpCommand::GetDapStatus { reply } => {
                let status = if let Some(dbg) =
                    self.editor.debug_adapters.get_active_client()
                {
                    helix_mcp::DapStatus {
                        active: true,
                        paused: dbg.thread_id.is_some(),
                        thread_id: dbg.thread_id.map(|t| {
                        t.to_string().parse::<i64>().unwrap_or(0) as usize
                    }),
                        active_frame: dbg.active_frame,
                    }
                } else {
                    helix_mcp::DapStatus {
                        active: false,
                        paused: false,
                        thread_id: None,
                        active_frame: None,
                    }
                };
                let _ = reply.send(status);
            }

            McpCommand::GetStackTrace { thread_id, reply } => {
                let result: anyhow::Result<Vec<helix_mcp::StackFrameInfo>> = (|| {
                    let dbg = self
                        .editor
                        .debug_adapters
                        .get_active_client()
                        .ok_or_else(|| anyhow::anyhow!("No active debugger"))?;
                    let tid = thread_id
                        .and_then(|t| {
                            serde_json::from_value(serde_json::json!(t as i64)).ok()
                        })
                        .or(dbg.thread_id)
                        .ok_or_else(|| anyhow::anyhow!("No active thread"))?;
                    let active_idx = dbg.active_frame.unwrap_or(0);
                    let frames = dbg
                        .stack_frames
                        .get(&tid)
                        .ok_or_else(|| anyhow::anyhow!("No stack frames for thread"))?;
                    Ok(frames
                        .iter()
                        .enumerate()
                        .map(|(i, f)| helix_mcp::StackFrameInfo {
                            id: f.id,
                            name: f.name.clone(),
                            path: f
                                .source
                                .as_ref()
                                .and_then(|s| s.path.as_ref())
                                .map(std::path::PathBuf::from),
                            line: f.line.saturating_sub(1),
                            col: f.column.saturating_sub(1),
                            is_active: i == active_idx,
                        })
                        .collect())
                })();
                let _ = reply.send(result);
            }

            McpCommand::GetScopes { frame_id, reply } => {
                let result: anyhow::Result<Vec<helix_mcp::ScopeInfo>> = async {
                    let dbg = self
                        .editor
                        .debug_adapters
                        .get_active_client()
                        .ok_or_else(|| anyhow::anyhow!("No active debugger"))?;
                    let scopes = dbg.scopes(frame_id).await?;
                    Ok(scopes
                        .into_iter()
                        .map(|s| helix_mcp::ScopeInfo {
                            name: s.name,
                            variables_ref: s.variables_reference,
                        })
                        .collect())
                }
                .await;
                let _ = reply.send(result);
            }

            McpCommand::GetVariables { variables_ref, reply } => {
                let result: anyhow::Result<Vec<helix_mcp::VariableInfo>> = async {
                    let dbg = self
                        .editor
                        .debug_adapters
                        .get_active_client()
                        .ok_or_else(|| anyhow::anyhow!("No active debugger"))?;
                    let scope_vars = dbg.variables(variables_ref).await?;
                    Ok(scope_vars
                        .into_iter()
                        .map(|v| helix_mcp::VariableInfo {
                            name: v.name,
                            value: v.value,
                            type_name: v.ty,
                            variables_ref: v.variables_reference,
                        })
                        .collect())
                }
                .await;
                let _ = reply.send(result);
            }

            // ---------------------------------------------------------------
            // DAP: Execution control
            // ---------------------------------------------------------------

            McpCommand::DapContinue { reply } => {
                let result: anyhow::Result<()> = {
                    let fut = {
                        let dbg = self
                            .editor
                            .debug_adapters
                            .get_active_client()
                            .ok_or_else(|| anyhow::anyhow!("No active debugger"));
                        match dbg {
                            Err(e) => {
                                let _ = reply.send(Err(e));
                                return;
                            }
                            Ok(dbg) => match dbg.thread_id {
                                None => {
                                    let _ = reply.send(Err(anyhow::anyhow!(
                                        "Debugger not paused"
                                    )));
                                    return;
                                }
                                Some(tid) => dbg.continue_thread(tid),
                            },
                        }
                    };
                    fut.await.map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
                };
                let _ = reply.send(result);
            }

            McpCommand::DapPause { reply } => {
                let result: anyhow::Result<()> = {
                    let fut = {
                        let dbg = self
                            .editor
                            .debug_adapters
                            .get_active_client()
                            .ok_or_else(|| anyhow::anyhow!("No active debugger"));
                        match dbg {
                            Err(e) => {
                                let _ = reply.send(Err(e));
                                return;
                            }
                            Ok(dbg) => match dbg.thread_id {
                                None => {
                                    let _ = reply.send(Err(anyhow::anyhow!(
                                        "Debugger not paused"
                                    )));
                                    return;
                                }
                                Some(tid) => dbg.pause(tid),
                            },
                        }
                    };
                    fut.await.map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
                };
                let _ = reply.send(result);
            }

            McpCommand::DapStepOver { reply } => {
                let result: anyhow::Result<()> = {
                    let fut = {
                        let dbg = self
                            .editor
                            .debug_adapters
                            .get_active_client()
                            .ok_or_else(|| anyhow::anyhow!("No active debugger"));
                        match dbg {
                            Err(e) => {
                                let _ = reply.send(Err(e));
                                return;
                            }
                            Ok(dbg) => match dbg.thread_id {
                                None => {
                                    let _ = reply.send(Err(anyhow::anyhow!(
                                        "Debugger not paused"
                                    )));
                                    return;
                                }
                                Some(tid) => dbg.next(tid),
                            },
                        }
                    };
                    fut.await.map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
                };
                let _ = reply.send(result);
            }

            McpCommand::DapStepIn { reply } => {
                let result: anyhow::Result<()> = {
                    let fut = {
                        let dbg = self
                            .editor
                            .debug_adapters
                            .get_active_client()
                            .ok_or_else(|| anyhow::anyhow!("No active debugger"));
                        match dbg {
                            Err(e) => {
                                let _ = reply.send(Err(e));
                                return;
                            }
                            Ok(dbg) => match dbg.thread_id {
                                None => {
                                    let _ = reply.send(Err(anyhow::anyhow!(
                                        "Debugger not paused"
                                    )));
                                    return;
                                }
                                Some(tid) => dbg.step_in(tid),
                            },
                        }
                    };
                    fut.await.map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
                };
                let _ = reply.send(result);
            }

            McpCommand::DapStepOut { reply } => {
                let result: anyhow::Result<()> = {
                    let fut = {
                        let dbg = self
                            .editor
                            .debug_adapters
                            .get_active_client()
                            .ok_or_else(|| anyhow::anyhow!("No active debugger"));
                        match dbg {
                            Err(e) => {
                                let _ = reply.send(Err(e));
                                return;
                            }
                            Ok(dbg) => match dbg.thread_id {
                                None => {
                                    let _ = reply.send(Err(anyhow::anyhow!(
                                        "Debugger not paused"
                                    )));
                                    return;
                                }
                                Some(tid) => dbg.step_out(tid),
                            },
                        }
                    };
                    fut.await.map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
                };
                let _ = reply.send(result);
            }

            // --- DAP: Session lifecycle ---

            McpCommand::DapListTemplates { reply } => {
                use helix_core::syntax::config::DebugConfigCompletion;
                use helix_mcp::{DapParamInfo, DapTemplateInfo};
                let result: anyhow::Result<Vec<DapTemplateInfo>> = (|| {
                    let view = self.editor.tree.get(self.editor.tree.focus);
                    let doc = &self.editor.documents[&view.doc];
                    let config = doc
                        .language_config()
                        .and_then(|c| c.debugger.as_ref())
                        .ok_or_else(|| {
                            anyhow::anyhow!("no debug adapter configured for this language")
                        })?;
                    Ok(config
                        .templates
                        .iter()
                        .map(|t| DapTemplateInfo {
                            name: t.name.clone(),
                            request: t.request.clone(),
                            params: t
                                .completion
                                .iter()
                                .map(|c| match c {
                                    DebugConfigCompletion::Named(n) => DapParamInfo {
                                        name: n.clone(),
                                        completion: None,
                                        default: None,
                                    },
                                    DebugConfigCompletion::Advanced(a) => DapParamInfo {
                                        name: a.name.clone().unwrap_or_default(),
                                        completion: a.completion.clone(),
                                        default: a.default.clone(),
                                    },
                                })
                                .collect(),
                        })
                        .collect())
                })();
                let _ = reply.send(result);
            }

            McpCommand::DapLaunch {
                template_name,
                params,
                reply,
            } => {
                use helix_core::syntax::config::DebugConfigCompletion;
                use serde_json::{to_value, Value};
                use std::collections::HashMap;

                fn map_dap_value(value: &Value, params: &[String]) -> Value {
                    match value {
                        Value::String(s) => {
                            let mut s = s.clone();
                            for (i, x) in params.iter().enumerate() {
                                s = s.replace(&format!("{{{}}}", i), x);
                            }
                            if let Ok(n) = s.parse::<usize>() {
                                to_value(n).unwrap()
                            } else {
                                to_value(s).unwrap()
                            }
                        }
                        Value::Array(a) => {
                            Value::Array(a.iter().map(|v| map_dap_value(v, params)).collect())
                        }
                        Value::Object(o) => Value::Object(
                            o.iter()
                                .map(|(k, v)| (k.clone(), map_dap_value(v, params)))
                                .collect(),
                        ),
                        _ => value.clone(),
                    }
                }

                let result: anyhow::Result<()> = async {
                    if self.editor.debug_adapters.get_active_client().is_some() {
                        anyhow::bail!("debugger is already running");
                    }
                    let view = self.editor.tree.get(self.editor.tree.focus);
                    let doc = &self.editor.documents[&view.doc];
                    let config = doc
                        .language_config()
                        .and_then(|c| c.debugger.as_ref())
                        .ok_or_else(|| {
                            anyhow::anyhow!("no debug adapter configured for this language")
                        })?;
                    // Clone to release the immutable borrow on editor before start_client.
                    let config = config.clone();

                    let id = self
                        .editor
                        .debug_adapters
                        .start_client(None, &config)
                        .map_err(|e| anyhow::anyhow!("failed to start debug client: {e}"))?;

                    let template = match template_name.as_deref() {
                        Some(name) => config.templates.iter().find(|t| t.name == name),
                        None => config.templates.first(),
                    }
                    .ok_or_else(|| anyhow::anyhow!("no matching debug template"))?;

                    // Canonicalize filename/directory params.
                    let preprocessed: Vec<String> = params
                        .iter()
                        .enumerate()
                        .map(|(i, x)| {
                            if let Some(DebugConfigCompletion::Advanced(cfg)) =
                                template.completion.get(i)
                            {
                                if matches!(
                                    cfg.completion.as_deref(),
                                    Some("filename" | "directory")
                                ) {
                                    return std::fs::canonicalize(x)
                                        .ok()
                                        .and_then(|p| p.into_os_string().into_string().ok())
                                        .unwrap_or_else(|| x.clone());
                                }
                            }
                            x.clone()
                        })
                        .collect();

                    let mut args: HashMap<&str, Value> = if params.is_empty() {
                        template
                            .args
                            .iter()
                            .map(|(k, v)| (k.as_str(), v.clone()))
                            .collect()
                    } else {
                        template
                            .args
                            .iter()
                            .map(|(k, v)| (k.as_str(), map_dap_value(v, &preprocessed)))
                            .collect()
                    };
                    args.insert("cwd", to_value(helix_stdx::env::current_working_dir())?);
                    let args = to_value(args)?;

                    let debugger = self
                        .editor
                        .debug_adapters
                        .get_client_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("failed to get debug client"))?;

                    match &template.request[..] {
                        "launch" => {
                            tokio::spawn(debugger.launch(args));
                            Ok(())
                        }
                        "attach" => {
                            tokio::spawn(debugger.attach(args));
                            Ok(())
                        }
                        r => anyhow::bail!("unsupported DAP request type: {r}"),
                    }
                }
                .await;
                let _ = reply.send(result);
            }

            McpCommand::DapTerminate { reply } => {
                use helix_dap::requests::TerminateArguments;
                let result: anyhow::Result<()> = async {
                    let debugger = self
                        .editor
                        .debug_adapters
                        .get_active_client_mut()
                        .ok_or_else(|| anyhow::anyhow!("no active debugger"))?;
                    if debugger
                        .caps
                        .as_ref()
                        .is_some_and(|c| c.supports_terminate_request.unwrap_or_default())
                    {
                        let fut = debugger
                            .terminate(Some(TerminateArguments { restart: Some(false) }));
                        fut.await.map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
                    } else {
                        Ok(())
                    }
                }
                .await;
                self.editor.debug_adapters.unset_active_client();
                let _ = reply.send(result);
            }

            // --- VCS: Diff ---

            // --- VCS: Diff ---

            McpCommand::GetDiffHunks { path, reply } => {
                if let Some(doc) = self.editor.document_by_path(&path) {
                    if let Some(diff_handle) = doc.diff_handle() {
                        let diff = diff_handle.load();
                        let head_ref = doc.version_control_head().map(|h| h.to_string());
                        let hunks: Vec<helix_mcp::DiffHunk> = (0..diff.len())
                            .map(|i| diff.nth_hunk(i))
                            .map(|h| {
                                let kind = if h.before.is_empty() {
                                    helix_mcp::HunkKind::Added
                                } else if h.after.is_empty() {
                                    helix_mcp::HunkKind::Deleted
                                } else {
                                    helix_mcp::HunkKind::Modified
                                };
                                helix_mcp::DiffHunk {
                                    kind,
                                    before_start: h.before.start as usize,
                                    before_end: h.before.end as usize,
                                    after_start: h.after.start as usize,
                                    after_end: h.after.end as usize,
                                }
                            })
                            .collect();
                        let _ = reply.send(Ok(helix_mcp::DiffResult { path, hunks, head_ref }));
                        return;
                    }
                    // File is open but has no diff handle (not tracked or no changes yet).
                    let _ = reply.send(Ok(helix_mcp::DiffResult {
                        path,
                        hunks: vec![],
                        head_ref: None,
                    }));
                } else {
                    let _ = reply.send(Err(anyhow::anyhow!(
                        "File not open in editor — open the file to enable diff tracking"
                    )));
                }
            }

            McpCommand::GetDiffBase { path, reply } => {
                let result = self
                    .editor
                    .diff_providers
                    .get_diff_base(&path)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .ok_or_else(|| {
                        anyhow::anyhow!("No diff base available for: {}", path.display())
                    });
                let _ = reply.send(result);
            }

            // --- Registers & Jumplist ---

            McpCommand::ReadRegister { name, reply } => {
                let values: Vec<String> = self
                    .editor
                    .registers
                    .read(name, &self.editor)
                    .map(|rv| rv.map(|s| s.to_string()).collect())
                    .unwrap_or_default();
                let _ = reply.send(Ok(values));
            }

            McpCommand::WriteRegister { name, values, reply } => {
                if !name.is_alphabetic() && name != '+' && name != '*' {
                    let _ = reply.send(Err(anyhow::anyhow!("Register '{}' is read-only", name)));
                    return;
                }
                match self.editor.registers.write(name, values) {
                    Ok(_) => {
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(anyhow::anyhow!("{e}")));
                    }
                }
            }

            McpCommand::GetJumplist { reply } => {
                // Collect jump data first so the view borrow ends before we access
                // editor.document() below.
                let jump_data = {
                    let (view, _) = helix_view::current_ref!(self.editor);
                    view.jumps.iter().cloned().collect::<Vec<_>>()
                };
                let entries: Vec<helix_mcp::JumpEntry> = jump_data
                    .iter()
                    .filter_map(|(doc_id, selection)| {
                        self.editor
                            .document(*doc_id)
                            .and_then(|doc| doc.path())
                            .map(|path| {
                                let doc = self.editor.document(*doc_id).unwrap();
                                let text = doc.text().slice(..);
                                let cursor = selection.primary().cursor(text);
                                let line = text.char_to_line(cursor);
                                helix_mcp::JumpEntry {
                                    path: path.to_path_buf(),
                                    line: line + 1,
                                    col: 1,
                                }
                            })
                    })
                    .collect();
                let _ = reply.send(entries);
            }

            // --- Buffer management ---

            McpCommand::LoadFile { path, reply } => {
                let result = if self.editor.document_by_path(&path).is_some() {
                    Ok(format!("File already loaded: {}", path.display()))
                } else {
                    self.editor
                        .open(&path, helix_view::editor::Action::Load)
                        .map(|_| format!("Loaded: {}", path.display()))
                        .map_err(|e| anyhow::anyhow!("{e}"))
                };
                let _ = reply.send(result);
            }

            McpCommand::UnloadFile { path, reply } => {
                let result = if let Some(doc) = self.editor.document_by_path(&path) {
                    let doc_id = doc.id();
                    let is_visible = self
                        .editor
                        .tree
                        .views()
                        .any(|(view, _)| view.doc == doc_id);
                    if is_visible {
                        Err(anyhow::anyhow!(
                            "Cannot unload: file is currently visible in the editor"
                        ))
                    } else {
                        match self.editor.close_document(doc_id, false) {
                            Ok(()) => Ok(format!("Unloaded: {}", path.display())),
                            Err(helix_view::editor::CloseError::DoesNotExist) => Ok(format!("File was not loaded: {}", path.display())),
                            Err(helix_view::editor::CloseError::BufferModified(name)) => Err(anyhow::anyhow!("Cannot unload modified buffer: {name}")),
                            Err(helix_view::editor::CloseError::SaveError(e)) => Err(anyhow::anyhow!("Save error while unloading: {e}")),
                        }
                    }
                } else {
                    Ok(format!("File was not loaded: {}", path.display()))
                };
                let _ = reply.send(result);
            }
        }
    }

    async fn handle_acp_message(
        &mut self,
        agent_id: helix_acp::AgentId,
        event: helix_acp::AcpEvent,
    ) {
        use helix_view::handlers::acp::AcpSideEffect;

        let side_effect = self.editor.handle_acp_event(agent_id, event);

        match side_effect {
            AcpSideEffect::None => {}
            AcpSideEffect::PermissionDialog {
                agent_id,
                params,
                reply,
            } => {
                self.show_acp_permission_dialog(agent_id, params, reply);
            }
        }
    }

    /// Build and push the permission-request Select dialog onto the compositor.
    fn show_acp_permission_dialog(
        &mut self,
        agent_id: helix_acp::AgentId,
        params: helix_acp::sdk::RequestPermissionRequest,
        reply: helix_acp::ReplyChannel<helix_acp::sdk::RequestPermissionResponse>,
    ) {
        use helix_acp::sdk::{
            PermissionOptionKind, RequestPermissionOutcome, RequestPermissionResponse,
            SelectedPermissionOutcome,
        };

        if params.options.is_empty() {
            let _ = reply.lock().unwrap().take().map(|tx| {
                let _ = tx.send(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ));
            });
            return;
        }

        // Clone the Arc so the Select callback can set auto_continue without
        // holding a borrow on the editor.
        let auto_continue_arc = self
            .editor
            .acp
            .state(agent_id)
            .map(|s| s.auto_continue.clone());

        let title = params
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| "Permission required".to_string());

        // Check for plan text to offer "Apply (clean context)" option.
        let plan_text = params
            .tool_call
            .fields
            .raw_input
            .as_ref()
            .and_then(|ri| ri["plan"].as_str())
            .is_some();

        let allow_always_id = params
            .options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowAlways)
            .or_else(|| params.options.first())
            .map(|o| o.option_id.to_string())
            .unwrap_or_default();
        let mut items: Vec<PermOption> = params
            .options
            .into_iter()
            .map(PermOption::Agent)
            .collect();
        if plan_text {
            items.insert(0, PermOption::CleanContext {
                allow_always_id: allow_always_id.clone(),
            });
        }
        // Reorder: CleanContext first, then Allow, then AllowAlways.
        items.sort_by_key(|item| match item {
            PermOption::CleanContext { .. } => 0,
            PermOption::Agent(opt)
                if opt.kind == PermissionOptionKind::AllowAlways =>
            {
                2
            }
            PermOption::Agent(_) => 1,
        });

        let select = ui::Select::new(
            title,
            items,
            (),
            move |editor, option: &PermOption, event| {
                use crate::ui::PromptEvent;
                let response = match event {
                    PromptEvent::Update => return,
                    PromptEvent::Validate => match option {
                        PermOption::CleanContext { allow_always_id } => {
                            if let Some(ref ac) = auto_continue_arc {
                                ac.store(
                                    true,
                                    std::sync::atomic::Ordering::SeqCst,
                                );
                            }
                            if let Some(state) = editor.acp.state_mut(agent_id) {
                                state.auto_accept_edits = true;
                                state.pending_clean_context_reply =
                                    Some((reply.clone(), allow_always_id.clone()));
                            }
                            return;
                        }
                        PermOption::Agent(opt) => {
                            if opt.kind == PermissionOptionKind::AllowAlways {
                                if let Some(ref ac) = auto_continue_arc {
                                    ac.store(
                                        true,
                                        std::sync::atomic::Ordering::SeqCst,
                                    );
                                }
                                if let Some(state) = editor.acp.state_mut(agent_id) {
                                    state.auto_accept_edits = true;
                                }
                            }
                            RequestPermissionResponse::new(
                                RequestPermissionOutcome::Selected(
                                    SelectedPermissionOutcome::new(
                                        opt.option_id.clone(),
                                    ),
                                ),
                            )
                        }
                    },
                    PromptEvent::Abort => RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ),
                };
                let _ = reply.lock().unwrap().take().map(|tx| tx.send(response));
            },
        )
        .no_auto_close()
        .with_id("acp-permission");

        self.compositor.replace_or_push("acp-permission", select);
    }


    /// If MCP trace mode is enabled, switch to the document at `path` and
    /// jump the cursor to `target_line` (0-indexed), centering the view.
    fn mcp_trace_jump(editor: &mut helix_view::Editor, path: &std::path::Path, target_line: usize) {
        if !editor.mcp_trace {
            return;
        }
        let Some(doc_id) = editor.document_id_by_path(path) else {
            return;
        };
        editor.switch(doc_id, helix_view::editor::Action::Replace);
        let scrolloff = editor.config().scrolloff;
        let view_id = editor.tree.focus;
        let view = editor.tree.get_mut(view_id);
        let doc = editor.documents.get_mut(&doc_id).unwrap();
        let line = target_line.min(doc.text().len_lines().saturating_sub(1));
        let char_pos = doc.text().line_to_char(line);
        doc.set_selection(view_id, Selection::single(char_pos, char_pos));
        view.ensure_cursor_in_view_center(doc, scrolloff);
    }

    pub async fn handle_terminal_events(&mut self, event: std::io::Result<TerminalEvent>) {
        #[cfg(not(windows))]
        use termina::escape::csi;

        let mut cx = crate::compositor::Context {
            editor: &mut self.editor,
            jobs: &mut self.jobs,
            scroll: None,
        };
        // Handle key events
        let should_redraw = match event.unwrap() {
            #[cfg(not(windows))]
            termina::Event::WindowResized(termina::WindowSize { rows, cols, .. }) => {
                self.terminal
                    .resize(Rect::new(0, 0, cols, rows))
                    .expect("Unable to resize terminal");

                let area = self.terminal.size();

                self.compositor.resize(area);

                self.compositor
                    .handle_event(&Event::Resize(cols, rows), &mut cx)
            }
            #[cfg(not(windows))]
            // Ignore keyboard release events.
            termina::Event::Key(termina::event::KeyEvent {
                kind: termina::event::KeyEventKind::Release,
                ..
            }) => false,
            #[cfg(not(windows))]
            termina::Event::Csi(csi::Csi::Mode(csi::Mode::ReportTheme(mode))) => {
                self.theme_mode = Some(mode.into());
                Self::load_configured_theme(
                    &mut self.editor,
                    &self.config.load(),
                    &mut self.terminal,
                    self.theme_mode,
                );
                true
            }
            #[cfg(windows)]
            TerminalEvent::Resize(width, height) => {
                self.terminal
                    .resize(Rect::new(0, 0, width, height))
                    .expect("Unable to resize terminal");

                let area = self.terminal.size();

                self.compositor.resize(area);

                self.compositor
                    .handle_event(&Event::Resize(width, height), &mut cx)
            }
            #[cfg(windows)]
            // Ignore keyboard release events.
            crossterm::event::Event::Key(crossterm::event::KeyEvent {
                kind: crossterm::event::KeyEventKind::Release,
                ..
            }) => false,
            #[cfg(not(windows))]
            event if event.is_escape() => false,
            event => self.compositor.handle_event(&event.into(), &mut cx),
        };

        // After compositor event handling, flush any pending clean-context permission replies
        // (set by the permission dialog callback when user selects "Apply (clean context)").
        {
            use helix_acp::sdk::{
                RequestPermissionOutcome, RequestPermissionResponse, SelectedPermissionOutcome,
            };
            // Collect IDs with pending replies (Registry only exposes iter(), not iter_mut()).
            let pending_ids: Vec<helix_acp::AgentId> = self
                .editor
                .acp
                .iter_states()
                .filter_map(|(id, s)| {
                    if s.pending_clean_context_reply.is_some() {
                        Some(id)
                    } else {
                        None
                    }
                })
                .collect();
            let pending: Vec<_> = pending_ids
                .into_iter()
                .filter_map(|id| {
                    self.editor
                        .acp
                        .state_mut(id)
                        .and_then(|s| s.pending_clean_context_reply.take())
                        .map(|(reply, allow_id)| (id, reply, allow_id))
                })
                .collect();

for (agent_id, reply, allow_always_id) in pending {
                // Clear local display only (no RPC to agent — the ACP protocol
                // has no session/clear method, and sending "/clear" as a prompt
                // triggers "Unknown skill" in the agent SDK).
                if let Some(state) = self.editor.acp.state_mut(agent_id) {
                    state.display.clear();
                    state.pending_edits.clear();
                }
                // Send the permission response immediately.
                let _ = reply
                    .lock()
                    .unwrap()
                    .take()
                    .map(|tx: tokio::sync::oneshot::Sender<RequestPermissionResponse>| {
                        tx.send(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(
                                SelectedPermissionOutcome::new(allow_always_id),
                            ),
                        ))
                    });
            }
        }
        if should_redraw && !self.editor.should_close() {
            self.render().await;
        }
    }

    pub async fn handle_language_server_message(
        &mut self,
        call: helix_lsp::Call,
        server_id: LanguageServerId,
    ) {
        use helix_lsp::{Call, MethodCall, Notification};

        macro_rules! language_server {
            () => {
                match self.editor.language_server_by_id(server_id) {
                    Some(language_server) => language_server,
                    None => {
                        warn!("can't find language server with id `{}`", server_id);
                        return;
                    }
                }
            };
        }

        match call {
            Call::Notification(helix_lsp::jsonrpc::Notification { method, params, .. }) => {
                let notification = match Notification::parse(&method, params) {
                    Ok(notification) => notification,
                    Err(helix_lsp::Error::Unhandled) => {
                        info!("Ignoring Unhandled notification from Language Server");
                        return;
                    }
                    Err(err) => {
                        error!(
                            "Ignoring unknown notification from Language Server: {}",
                            err
                        );
                        return;
                    }
                };

                match notification {
                    Notification::Initialized => {
                        let language_server = language_server!();

                        // Trigger a workspace/didChangeConfiguration notification after initialization.
                        // This might not be required by the spec but Neovim does this as well, so it's
                        // probably a good idea for compatibility.
                        if let Some(config) = language_server.config() {
                            language_server.did_change_configuration(config.clone());
                        }

                        helix_event::dispatch(helix_view::events::LanguageServerInitialized {
                            editor: &mut self.editor,
                            server_id,
                        });
                    }
                    Notification::PublishDiagnostics(params) => {
                        let uri = match helix_core::Uri::try_from(params.uri) {
                            Ok(uri) => uri,
                            Err(err) => {
                                log::error!("{err}");
                                return;
                            }
                        };
                        let language_server = language_server!();
                        if !language_server.is_initialized() {
                            log::error!("Discarding publishDiagnostic notification sent by an uninitialized server: {}", language_server.name());
                            return;
                        }
                        let provider = helix_core::diagnostic::DiagnosticProvider::Lsp {
                            server_id,
                            identifier: None,
                        };
                        self.editor.handle_lsp_diagnostics(
                            &provider,
                            uri,
                            params.version,
                            params.diagnostics,
                        );
                    }
                    Notification::ShowMessage(params) => {
                        self.handle_show_message(params.typ, params.message);
                    }
                    Notification::LogMessage(params) => {
                        log::info!("window/logMessage: {:?}", params);
                    }
                    Notification::ProgressMessage(params)
                        if !self
                            .compositor
                            .has_component(std::any::type_name::<ui::Prompt>()) =>
                    {
                        let editor_view = self
                            .compositor
                            .find::<ui::EditorView>()
                            .expect("expected at least one EditorView");
                        let lsp::ProgressParams {
                            token,
                            value: lsp::ProgressParamsValue::WorkDone(work),
                        } = params;
                        let (title, message, percentage) = match &work {
                            lsp::WorkDoneProgress::Begin(lsp::WorkDoneProgressBegin {
                                title,
                                message,
                                percentage,
                                ..
                            }) => (Some(title), message, percentage),
                            lsp::WorkDoneProgress::Report(lsp::WorkDoneProgressReport {
                                message,
                                percentage,
                                ..
                            }) => (None, message, percentage),
                            lsp::WorkDoneProgress::End(lsp::WorkDoneProgressEnd { message }) => {
                                if message.is_some() {
                                    (None, message, &None)
                                } else {
                                    self.lsp_progress.end_progress(server_id, &token);
                                    if !self.lsp_progress.is_progressing(server_id) {
                                        editor_view.spinners_mut().get_or_create(server_id).stop();
                                    }
                                    self.editor.clear_status();

                                    // we want to render to clear any leftover spinners or messages
                                    return;
                                }
                            }
                        };

                        if self.editor.config().lsp.display_progress_messages {
                            let title =
                                title.or_else(|| self.lsp_progress.title(server_id, &token));
                            if title.is_some() || percentage.is_some() || message.is_some() {
                                use std::fmt::Write as _;
                                let mut status = format!("{}: ", language_server!().name());
                                if let Some(percentage) = percentage {
                                    write!(status, "{percentage:>2}% ").unwrap();
                                }
                                if let Some(title) = title {
                                    status.push_str(title);
                                }
                                if title.is_some() && message.is_some() {
                                    status.push_str(" ⋅ ");
                                }
                                if let Some(message) = message {
                                    status.push_str(message);
                                }
                                self.editor.set_status(status);
                            }
                        }

                        match work {
                            lsp::WorkDoneProgress::Begin(begin_status) => {
                                self.lsp_progress
                                    .begin(server_id, token.clone(), begin_status);
                            }
                            lsp::WorkDoneProgress::Report(report_status) => {
                                self.lsp_progress
                                    .update(server_id, token.clone(), report_status);
                            }
                            lsp::WorkDoneProgress::End(_) => {
                                self.lsp_progress.end_progress(server_id, &token);
                                if !self.lsp_progress.is_progressing(server_id) {
                                    editor_view.spinners_mut().get_or_create(server_id).stop();
                                };
                            }
                        }
                    }
                    Notification::ProgressMessage(_params) => {
                        // do nothing
                    }
                    Notification::Exit => {
                        self.editor.set_status("Language server exited");

                        // LSPs may produce diagnostics for files that haven't been opened in helix,
                        // we need to clear those and remove the entries from the list if this leads to
                        // an empty diagnostic list for said files
                        for diags in self.editor.diagnostics.values_mut() {
                            diags.retain(|(_, provider)| {
                                provider.language_server_id() != Some(server_id)
                            });
                        }

                        self.editor.diagnostics.retain(|_, diags| !diags.is_empty());

                        // Clear any diagnostics for documents with this server open.
                        for doc in self.editor.documents_mut() {
                            doc.clear_diagnostics_for_language_server(server_id);
                        }

                        helix_event::dispatch(helix_view::events::LanguageServerExited {
                            editor: &mut self.editor,
                            server_id,
                        });

                        // Remove the language server from the registry.
                        self.editor.language_servers.remove_by_id(server_id);
                    }
                }
            }
            Call::MethodCall(helix_lsp::jsonrpc::MethodCall {
                method, params, id, ..
            }) => {
                let reply = match MethodCall::parse(&method, params) {
                    Err(helix_lsp::Error::Unhandled) => {
                        error!(
                            "Language Server: Method {} not found in request {}",
                            method, id
                        );
                        Err(helix_lsp::jsonrpc::Error {
                            code: helix_lsp::jsonrpc::ErrorCode::MethodNotFound,
                            message: format!("Method not found: {}", method),
                            data: None,
                        })
                    }
                    Err(err) => {
                        log::error!(
                            "Language Server: Received malformed method call {} in request {}: {}",
                            method,
                            id,
                            err
                        );
                        Err(helix_lsp::jsonrpc::Error {
                            code: helix_lsp::jsonrpc::ErrorCode::ParseError,
                            message: format!("Malformed method call: {}", method),
                            data: None,
                        })
                    }
                    Ok(MethodCall::WorkDoneProgressCreate(params)) => {
                        self.lsp_progress.create(server_id, params.token);

                        let editor_view = self
                            .compositor
                            .find::<ui::EditorView>()
                            .expect("expected at least one EditorView");
                        let spinner = editor_view.spinners_mut().get_or_create(server_id);
                        if spinner.is_stopped() {
                            spinner.start();
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ApplyWorkspaceEdit(params)) => {
                        let language_server = language_server!();
                        if language_server.is_initialized() {
                            let offset_encoding = language_server.offset_encoding();
                            let res = self
                                .editor
                                .apply_workspace_edit(offset_encoding, &params.edit);

                            Ok(json!(lsp::ApplyWorkspaceEditResponse {
                                applied: res.is_ok(),
                                failure_reason: res.as_ref().err().map(|err| err.kind.to_string()),
                                failed_change: res
                                    .as_ref()
                                    .err()
                                    .map(|err| err.failed_change_idx as u32),
                            }))
                        } else {
                            Err(helix_lsp::jsonrpc::Error {
                                code: helix_lsp::jsonrpc::ErrorCode::InvalidRequest,
                                message: "Server must be initialized to request workspace edits"
                                    .to_string(),
                                data: None,
                            })
                        }
                    }
                    Ok(MethodCall::WorkspaceFolders) => {
                        Ok(json!(&*language_server!().workspace_folders().await))
                    }
                    Ok(MethodCall::WorkspaceConfiguration(params)) => {
                        let language_server = language_server!();
                        let result: Vec<_> = params
                            .items
                            .iter()
                            .map(|item| {
                                let mut config = language_server.config()?;
                                if let Some(section) = item.section.as_ref() {
                                    // for some reason some lsps send an empty string (observed in 'vscode-eslint-language-server')
                                    if !section.is_empty() {
                                        for part in section.split('.') {
                                            config = config.get(part)?;
                                        }
                                    }
                                }
                                Some(config)
                            })
                            .collect();
                        Ok(json!(result))
                    }
                    Ok(MethodCall::RegisterCapability(params)) => {
                        if let Some(client) = self.editor.language_servers.get_by_id(server_id) {
                            for reg in params.registrations {
                                match reg.method.as_str() {
                                    lsp::notification::DidChangeWatchedFiles::METHOD => {
                                        let Some(options) = reg.register_options else {
                                            continue;
                                        };
                                        let ops: lsp::DidChangeWatchedFilesRegistrationOptions =
                                            match serde_json::from_value(options) {
                                                Ok(ops) => ops,
                                                Err(err) => {
                                                    log::warn!("Failed to deserialize DidChangeWatchedFilesRegistrationOptions: {err}");
                                                    continue;
                                                }
                                            };
                                        self.editor.language_servers.file_event_handler.register(
                                            client.id(),
                                            Arc::downgrade(client),
                                            reg.id,
                                            ops,
                                        )
                                    }
                                    _ => {
                                        // Language Servers based on the `vscode-languageserver-node` library often send
                                        // client/registerCapability even though we do not enable dynamic registration
                                        // for most capabilities. We should send a MethodNotFound JSONRPC error in this
                                        // case but that rejects the registration promise in the server which causes an
                                        // exit. So we work around this by ignoring the request and sending back an OK
                                        // response.
                                        log::warn!("Ignoring a client/registerCapability request because dynamic capability registration is not enabled. Please report this upstream to the language server");
                                    }
                                }
                            }
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::UnregisterCapability(params)) => {
                        for unreg in params.unregisterations {
                            match unreg.method.as_str() {
                                lsp::notification::DidChangeWatchedFiles::METHOD => {
                                    self.editor
                                        .language_servers
                                        .file_event_handler
                                        .unregister(server_id, unreg.id);
                                }
                                _ => {
                                    log::warn!("Received unregistration request for unsupported method: {}", unreg.method);
                                }
                            }
                        }
                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ShowDocument(params)) => {
                        let language_server = language_server!();
                        let offset_encoding = language_server.offset_encoding();

                        let result = self.handle_show_document(params, offset_encoding);
                        Ok(json!(result))
                    }
                    Ok(MethodCall::WorkspaceDiagnosticRefresh) => {
                        let language_server = language_server!().id();

                        let documents: Vec<_> = self
                            .editor
                            .documents
                            .values()
                            .filter(|x| x.supports_language_server(language_server))
                            .map(|x| x.id())
                            .collect();

                        for document in documents {
                            handlers::diagnostics::request_document_diagnostics(
                                &mut self.editor,
                                document,
                            );
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::InlayHintRefresh) => {
                        let language_server_id = language_server!().id();
                        for doc in self.editor.documents.values_mut() {
                            if doc.supports_language_server(language_server_id) {
                                doc.inlay_hints_oudated = true;
                            }
                        }
                        crate::commands::lsp::compute_inlay_hints_for_all_views(
                            &mut self.editor,
                            &mut self.jobs,
                        );
                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::SemanticTokensRefresh) => {
                        // Semantic tokens are not yet supported, acknowledge the refresh
                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::CodeLensRefresh) => {
                        // Code lens is not yet supported, acknowledge the refresh
                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ShowMessageRequest(params)) => {
                        if let Some(actions) = params.actions.filter(|a| !a.is_empty()) {
                            let id = id.clone();
                            let select = ui::Select::new(
                                params.message,
                                actions,
                                (),
                                move |editor, action, event| {
                                    let reply = match event {
                                        ui::PromptEvent::Update => return,
                                        ui::PromptEvent::Validate => Some(action.clone()),
                                        ui::PromptEvent::Abort => None,
                                    };
                                    if let Some(language_server) =
                                        editor.language_server_by_id(server_id)
                                    {
                                        if let Err(err) =
                                            language_server.reply(id.clone(), Ok(json!(reply)))
                                        {
                                            log::error!(
                                                "Failed to send reply to server '{}' request {id}: {err}",
                                                language_server.name()
                                            );
                                        }
                                    }
                                },
                            );
                            self.compositor
                                .replace_or_push("lsp-show-message-request", select);
                            // Avoid sending a reply. The `Select` callback above sends the reply.
                            return;
                        } else {
                            self.handle_show_message(params.typ, params.message);
                            Ok(serde_json::Value::Null)
                        }
                    }
                };

                let language_server = language_server!();
                if let Err(err) = language_server.reply(id.clone(), reply) {
                    log::error!(
                        "Failed to send reply to server '{}' request {id}: {err}",
                        language_server.name()
                    );
                }
            }
            Call::Invalid { id } => log::error!("LSP invalid method call id={:?}", id),
        }
    }

    fn handle_show_message(&mut self, message_type: lsp::MessageType, message: String) {
        if self.config.load().editor.lsp.display_messages {
            match message_type {
                lsp::MessageType::ERROR => self.editor.set_error(message),
                lsp::MessageType::WARNING => self.editor.set_warning(message),
                _ => self.editor.set_status(message),
            }
        }
    }

    fn handle_show_document(
        &mut self,
        params: lsp::ShowDocumentParams,
        offset_encoding: helix_lsp::OffsetEncoding,
    ) -> lsp::ShowDocumentResult {
        if let lsp::ShowDocumentParams {
            external: Some(true),
            uri,
            ..
        } = params
        {
            self.jobs.callback(crate::open_external_url_callback(uri));
            return lsp::ShowDocumentResult { success: true };
        };

        let lsp::ShowDocumentParams {
            uri,
            selection,
            take_focus,
            ..
        } = params;

        let uri = match helix_core::Uri::try_from(uri) {
            Ok(uri) => uri,
            Err(err) => {
                log::error!("{err}");
                return lsp::ShowDocumentResult { success: false };
            }
        };
        // If `Uri` gets another variant other than `Path` this may not be valid.
        let path = uri.as_path().expect("URIs are valid paths");

        let action = match take_focus {
            Some(true) => helix_view::editor::Action::Replace,
            _ => helix_view::editor::Action::VerticalSplit,
        };

        let doc_id = match self.editor.open(path, action) {
            Ok(id) => id,
            Err(err) => {
                log::error!("failed to open path: {:?}: {:?}", uri, err);
                return lsp::ShowDocumentResult { success: false };
            }
        };

        let doc = doc_mut!(self.editor, &doc_id);
        if let Some(range) = selection {
            // TODO: convert inside server
            if let Some(new_range) = lsp_range_to_range(doc.text(), range, offset_encoding) {
                let view = view_mut!(self.editor);

                // we flip the range so that the cursor sits on the start of the symbol
                // (for example start of the function).
                doc.set_selection(view.id, Selection::single(new_range.head, new_range.anchor));
                if action.align_view(view, doc.id()) {
                    align_view(doc, view, Align::Center);
                }
            } else {
                log::warn!("lsp position out of bounds - {:?}", range);
            };
        };
        lsp::ShowDocumentResult { success: true }
    }

    fn restore_term(&mut self) -> std::io::Result<()> {
        use helix_view::graphics::CursorKind;
        self.terminal
            .backend_mut()
            .show_cursor(CursorKind::Block)
            .ok();
        self.terminal.restore()
    }

    #[cfg(all(not(feature = "integration"), not(windows)))]
    pub fn event_stream(&self) -> impl Stream<Item = std::io::Result<TerminalEvent>> + Unpin {
        use termina::{escape::csi, Terminal as _};
        let reader = self.terminal.backend().terminal().event_reader();
        termina::EventStream::new(reader, |event| {
            // Accept either non-escape sequences or theme mode updates.
            !event.is_escape()
                || matches!(
                    event,
                    termina::Event::Csi(csi::Csi::Mode(csi::Mode::ReportTheme(_)))
                )
        })
    }

    #[cfg(all(not(feature = "integration"), windows))]
    pub fn event_stream(&self) -> impl Stream<Item = std::io::Result<TerminalEvent>> + Unpin {
        crossterm::event::EventStream::new()
    }

    #[cfg(feature = "integration")]
    pub fn event_stream(&self) -> impl Stream<Item = std::io::Result<TerminalEvent>> + Unpin {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };

        /// A dummy stream that never polls as ready.
        pub struct DummyEventStream;

        impl Stream for DummyEventStream {
            type Item = std::io::Result<TerminalEvent>;

            fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                Poll::Pending
            }
        }

        DummyEventStream
    }

    pub async fn run<S>(&mut self, input_stream: &mut S) -> Result<i32, Error>
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        self.terminal.claim()?;

        self.event_loop(input_stream).await;

        let close_errs = self.close().await;

        self.restore_term()?;

        for err in close_errs {
            self.editor.exit_code = 1;
            eprintln!("Error: {}", err);
        }

        Ok(self.editor.exit_code)
    }

    pub async fn close(&mut self) -> Vec<anyhow::Error> {
        // [NOTE] we intentionally do not return early for errors because we
        //        want to try to run as much cleanup as we can, regardless of
        //        errors along the way
        let mut errs = Vec::new();

        if let Err(err) = self
            .jobs
            .finish(&mut self.editor, Some(&mut self.compositor))
            .await
        {
            log::error!("Error executing job: {}", err);
            errs.push(err);
        };

        if let Err(err) = self.editor.flush_writes().await {
            log::error!("Error writing: {}", err);
            errs.push(err);
        }

        if self.editor.close_language_servers(None).await.is_err() {
            log::error!("Timed out waiting for language servers to shutdown");
            errs.push(anyhow::format_err!(
                "Timed out waiting for language servers to shutdown"
            ));
        }

        errs
    }
}

impl ui::menu::Item for lsp::MessageActionItem {
    type Data = ();
    fn format(&self, _data: &Self::Data) -> tui::widgets::Row<'_> {
        self.title.as_str().into()
    }
}

impl ui::menu::Item for helix_acp::sdk::PermissionOption {
    type Data = ();
    fn format(&self, _: &Self::Data) -> tui::widgets::Row<'_> {
        self.name.as_str().into()
    }
}

/// Wraps agent-provided permission options plus Helix-injected extras.
enum PermOption {
    Agent(helix_acp::sdk::PermissionOption),
    CleanContext { allow_always_id: String },
}

impl ui::menu::Item for PermOption {
    type Data = ();
    fn format(&self, _: &Self::Data) -> tui::widgets::Row<'_> {
        match self {
            PermOption::Agent(opt) => opt.name.as_str().into(),
            PermOption::CleanContext { .. } => "Apply (clean context)".into(),
        }
    }
}

/// Two-option enum for MCP write-operation approval popups.
#[derive(Clone)]
enum McpApproveAction {
    Apply,
    Cancel,
}

impl ui::menu::Item for McpApproveAction {
    type Data = ();
    fn format(&self, _: &Self::Data) -> tui::widgets::Row<'_> {
        match self {
            McpApproveAction::Apply  => "Apply".into(),
            McpApproveAction::Cancel => "Cancel".into(),
        }
    }
}
