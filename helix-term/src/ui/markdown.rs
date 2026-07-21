use crate::compositor::{Component, Context};
use arc_swap::ArcSwap;
use tui::{
    buffer::Buffer as Surface,
    text::{Span, Spans, Text},
};

use std::sync::Arc;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use helix_core::{
    syntax::{self, HighlightEvent, OverlayHighlights},
    RopeSlice, Syntax,
};
use helix_view::{
    graphics::{Margin, Rect, Style},
    theme::Modifier,
    Theme,
};

fn styled_multiline_text<'a>(text: &str, style: Style) -> Text<'a> {
    let spans: Vec<_> = text
        .lines()
        .map(|line| Span::styled(line.to_string(), style))
        .map(Spans::from)
        .collect();
    Text::from(spans)
}

pub fn highlighted_code_block<'a>(
    text: &str,
    language: &str,
    theme: Option<&Theme>,
    loader: &syntax::Loader,
    // Optional overlay highlights to mix in with the syntax highlights.
    //
    // Note that `OverlayHighlights` is typically used with char indexing but the only caller
    // which passes this parameter currently passes **byte indices** instead.
    additional_highlight_spans: Option<OverlayHighlights>,
) -> Text<'a> {
    let mut spans = Vec::new();
    let mut lines = Vec::new();

    let get_theme = |key: &str| -> Style { theme.map(|t| t.get(key)).unwrap_or_default() };
    let text_style = get_theme(Markdown::TEXT_STYLE);
    let code_style = get_theme(Markdown::BLOCK_STYLE);

    let theme = match theme {
        Some(t) => t,
        None => return styled_multiline_text(text, code_style),
    };

    let ropeslice = RopeSlice::from(text);
    let Some(syntax) = loader
        .language_for_match(RopeSlice::from(language))
        .and_then(|lang| Syntax::new(ropeslice, lang, loader).ok())
    else {
        return styled_multiline_text(text, code_style);
    };

    let mut syntax_highlighter = syntax.highlighter(ropeslice, loader, ..);
    let mut syntax_highlight_stack = Vec::new();
    let mut overlay_highlight_stack = Vec::new();
    let mut overlay_highlighter = syntax::OverlayHighlighter::new(additional_highlight_spans);
    let mut pos = 0;

    while pos < ropeslice.len_bytes() as u32 {
        if pos == syntax_highlighter.next_event_offset() {
            let (event, new_highlights) = syntax_highlighter.advance();
            if event == HighlightEvent::Refresh {
                syntax_highlight_stack.clear();
            }
            syntax_highlight_stack.extend(new_highlights);
        } else if pos == overlay_highlighter.next_event_offset() as u32 {
            let (event, new_highlights) = overlay_highlighter.advance();
            if event == HighlightEvent::Refresh {
                overlay_highlight_stack.clear();
            }
            overlay_highlight_stack.extend(new_highlights)
        }

        let start = pos;
        pos = syntax_highlighter
            .next_event_offset()
            .min(overlay_highlighter.next_event_offset() as u32);
        if pos == u32::MAX {
            pos = ropeslice.len_bytes() as u32;
        }
        if pos == start {
            continue;
        }
        // The highlighter should always move forward.
        // If the highlighter malfunctions, bail on syntax highlighting and log an error.
        debug_assert!(pos > start);
        if pos < start {
            log::error!("Failed to highlight '{language}': {text:?}");
            return styled_multiline_text(text, code_style);
        }

        let style = syntax_highlight_stack
            .iter()
            .chain(overlay_highlight_stack.iter())
            .fold(text_style, |acc, highlight| {
                acc.patch(theme.highlight(*highlight))
            });

        let mut slice = &text[start as usize..pos as usize];
        // TODO: do we need to handle all unicode line endings
        // here, or is just '\n' okay?
        while let Some(end) = slice.find('\n') {
            // emit span up to newline
            let text = &slice[..end];
            let text = text.replace('\t', "    "); // replace tabs
            let span = Span::styled(text, style);
            spans.push(span);

            // truncate slice to after newline
            slice = &slice[end + 1..];

            // make a new line
            let spans = std::mem::take(&mut spans);
            lines.push(Spans::from(spans));
        }

        if !slice.is_empty() {
            let span = Span::styled(slice.replace('\t', "    "), style);
            spans.push(span);
        }
    }

    if !spans.is_empty() {
        let spans = std::mem::take(&mut spans);
        lines.push(Spans::from(spans));
    }

    Text::from(lines)
}

pub struct Markdown {
    contents: String,

    config_loader: Arc<ArcSwap<syntax::Loader>>,

    /// When set, tables wider than this many columns are shrunk (columns
    /// narrowed, cell text wrapped) so the whole table fits. `None` keeps the
    /// natural, content-sized table (the default for hover/agent panel/etc.).
    max_table_width: Option<usize>,
}

// TODO: pre-render and self reference via Pin
// better yet, just use Tendril + subtendril for references

impl Markdown {
    const TEXT_STYLE: &'static str = "ui.text";
    const BLOCK_STYLE: &'static str = "markup.raw.inline";
    const RULE_STYLE: &'static str = "punctuation.special";
    const UNNUMBERED_LIST_STYLE: &'static str = "markup.list.unnumbered";
    const NUMBERED_LIST_STYLE: &'static str = "markup.list.numbered";
    const HEADING_STYLES: [&'static str; 6] = [
        "markup.heading.1",
        "markup.heading.2",
        "markup.heading.3",
        "markup.heading.4",
        "markup.heading.5",
        "markup.heading.6",
    ];
    const INDENT: &'static str = "  ";

    pub fn new(contents: String, config_loader: Arc<ArcSwap<syntax::Loader>>) -> Self {
        Self {
            contents,
            config_loader,
            max_table_width: None,
        }
    }

    /// Constrain rendered tables to at most `width` columns, wrapping cell text
    /// across multiple rows as needed.
    pub fn with_max_table_width(mut self, width: usize) -> Self {
        self.max_table_width = Some(width);
        self
    }

    pub fn parse(&self, theme: Option<&Theme>) -> tui::text::Text<'_> {
        fn push_line<'a>(spans: &mut Vec<Span<'a>>, lines: &mut Vec<Spans<'a>>) {
            let spans = std::mem::take(spans);
            if !spans.is_empty() {
                lines.push(Spans::from(spans));
            }
        }

        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);
        let parser = Parser::new_ext(&self.contents, options);

        // TODO: if possible, render links as terminal hyperlinks: https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda
        let mut tags = Vec::new();
        let mut spans = Vec::new();
        let mut lines = Vec::new();
        let mut list_stack = Vec::new();

        let get_indent = |level: usize| {
            if level < 1 {
                String::new()
            } else {
                Self::INDENT.repeat(level - 1)
            }
        };

        let get_theme = |key: &str| -> Style { theme.map(|t| t.get(key)).unwrap_or_default() };
        let text_style = get_theme(Self::TEXT_STYLE);
        let code_style = get_theme(Self::BLOCK_STYLE);
        let numbered_list_style = get_theme(Self::NUMBERED_LIST_STYLE);
        let unnumbered_list_style = get_theme(Self::UNNUMBERED_LIST_STYLE);
        let rule_style = get_theme(Self::RULE_STYLE);
        let heading_styles: Vec<Style> = Self::HEADING_STYLES
            .iter()
            .map(|key| get_theme(key))
            .collect();

        // Transform text in `<code>` blocks into `Event::Code`
        let mut in_code = false;
        let parser = parser.filter_map(|event| match event {
            Event::Html(tag)
                if tag.starts_with("<code") && matches!(tag.chars().nth(5), Some(' ' | '>')) =>
            {
                in_code = true;
                None
            }
            Event::Html(tag) if *tag == *"</code>" => {
                in_code = false;
                None
            }
            Event::Text(text) if in_code => Some(Event::Code(text)),
            _ => Some(event),
        });

        // Table buffering state
        let mut table_alignments: Vec<Alignment> = Vec::new();
        let mut table_rows: Vec<Vec<String>> = Vec::new();
        let mut table_is_head: Vec<bool> = Vec::new();
        let mut current_row: Vec<String> = Vec::new();
        let mut current_cell = String::new();
        let mut in_table = false;

        for event in parser {
            match event {
                Event::Start(Tag::List(list)) => {
                    // if the list stack is not empty this is a sub list, in that
                    // case we need to push the current line before proceeding
                    if !list_stack.is_empty() {
                        push_line(&mut spans, &mut lines);
                    }

                    list_stack.push(list);
                }
                Event::End(TagEnd::List(_)) => {
                    list_stack.pop();

                    // whenever top-level list closes, empty line
                    if list_stack.is_empty() {
                        lines.push(Spans::default());
                    }
                }
                Event::Start(Tag::Item) => {
                    if list_stack.is_empty() {
                        log::warn!("markdown parsing error, list item without list");
                    }

                    tags.push(Tag::Item);

                    // get the appropriate bullet for the current list
                    let (bullet, bullet_style) = list_stack
                        .last()
                        .unwrap_or(&None) // use the '- ' bullet in case the list stack would be empty
                        .map_or((String::from("• "), unnumbered_list_style), |number| {
                            (format!("{}. ", number), numbered_list_style)
                        });

                    // increment the current list number if there is one
                    if let Some(v) = list_stack.last_mut().unwrap_or(&mut None).as_mut() {
                        *v += 1;
                    }

                    let prefix = get_indent(list_stack.len()) + bullet.as_str();
                    spans.push(Span::styled(prefix, bullet_style));
                }
                Event::Start(Tag::Table(aligns)) => {
                    table_alignments = aligns;
                    table_rows.clear();
                    table_is_head.clear();
                    in_table = true;
                }
                Event::Start(Tag::TableHead) => {
                    current_row.clear();
                }
                Event::Start(Tag::TableRow) => {
                    current_row.clear();
                }
                Event::Start(Tag::TableCell) => {
                    current_cell.clear();
                }
                Event::End(TagEnd::TableCell) => {
                    current_row.push(std::mem::take(&mut current_cell));
                }
                Event::End(TagEnd::TableHead) => {
                    table_is_head.push(true);
                    table_rows.push(std::mem::take(&mut current_row));
                }
                Event::End(TagEnd::TableRow) => {
                    table_is_head.push(false);
                    table_rows.push(std::mem::take(&mut current_row));
                }
                Event::End(TagEnd::Table) => {
                    in_table = false;
                    let num_cols = table_rows.iter().map(|r| r.len()).max().unwrap_or(0);
                    let mut col_widths: Vec<usize> = vec![1; num_cols];
                    for row in &table_rows {
                        for (i, cell) in row.iter().enumerate() {
                            col_widths[i] = col_widths[i].max(cell.chars().count());
                        }
                    }

                    // Optionally shrink columns so the whole table fits `max_table_width`.
                    // Frame overhead per line is `1 + 3 * num_cols` (borders + padding).
                    if let Some(max_w) = self.max_table_width {
                        if num_cols > 0 {
                            let target = max_w.saturating_sub(1 + 3 * num_cols);
                            const MIN_COL: usize = 3;
                            while col_widths.iter().sum::<usize>() > target {
                                let (idx, &widest) = col_widths
                                    .iter()
                                    .enumerate()
                                    .max_by_key(|(_, &w)| w)
                                    .unwrap();
                                if widest <= MIN_COL {
                                    break; // can't shrink further; allow overflow
                                }
                                col_widths[idx] = widest - 1;
                            }
                        }
                    }

                    let border = |left: char, mid: char, right: char| -> Spans<'static> {
                        let mut s = String::new();
                        s.push(left);
                        for (i, &w) in col_widths.iter().enumerate() {
                            s.push_str(&"─".repeat(w + 2));
                            s.push(if i + 1 < col_widths.len() { mid } else { right });
                        }
                        Spans::from(Span::styled(s, text_style))
                    };

                    lines.push(border('┌', '┬', '┐'));
                    for (row_idx, (row, &is_head)) in
                        table_rows.iter().zip(table_is_head.iter()).enumerate()
                    {
                        // Wrap every cell to its column width; the row spans as
                        // many physical lines as the tallest wrapped cell.
                        let wrapped: Vec<Vec<String>> = (0..num_cols)
                            .map(|i| {
                                let w = col_widths.get(i).copied().unwrap_or(1);
                                let cell = row.get(i).map(String::as_str).unwrap_or("");
                                wrap_text(cell, w)
                            })
                            .collect();
                        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
                        let style = if is_head {
                            text_style.add_modifier(Modifier::BOLD)
                        } else {
                            text_style
                        };
                        for k in 0..height {
                            let mut s = String::from("│");
                            for i in 0..num_cols {
                                let w = col_widths.get(i).copied().unwrap_or(1);
                                let align =
                                    table_alignments.get(i).copied().unwrap_or(Alignment::None);
                                let piece = wrapped[i].get(k).map(String::as_str).unwrap_or("");
                                s.push(' ');
                                s.push_str(&align_cell(piece, w, align));
                                s.push_str(" │");
                            }
                            lines.push(Spans::from(Span::styled(s, style)));
                        }
                        if row_idx + 1 == table_rows.len() {
                            lines.push(border('└', '┴', '┘'));
                        } else {
                            lines.push(border('├', '┼', '┤'));
                        }
                    }
                    lines.push(Spans::default()); // blank line after table
                }
                Event::Start(tag) => {
                    tags.push(tag);
                    if spans.is_empty() && !list_stack.is_empty() {
                        // TODO: could push indent + 2 or 3 spaces to align with
                        // the rest of the list.
                        spans.push(Span::from(get_indent(list_stack.len())));
                    }
                }
                Event::End(tag) => {
                    tags.pop();
                    match tag {
                        TagEnd::Heading(_)
                        | TagEnd::Paragraph
                        | TagEnd::CodeBlock
                        | TagEnd::Item => {
                            push_line(&mut spans, &mut lines);
                        }
                        _ => (),
                    }

                    // whenever heading, code block or paragraph closes, empty line
                    match tag {
                        TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::CodeBlock => {
                            lines.push(Spans::default());
                        }
                        _ => (),
                    }
                }
                Event::Text(text) => {
                    if in_table {
                        current_cell.push_str(&text);
                    } else if let Some(Tag::CodeBlock(kind)) = tags.last() {
                        let language = match kind {
                            CodeBlockKind::Fenced(language) => language,
                            CodeBlockKind::Indented => "",
                        };
                        let tui_text = highlighted_code_block(
                            &text,
                            language,
                            theme,
                            &self.config_loader.load(),
                            None,
                        );
                        lines.extend(tui_text.lines);
                    } else {
                        let style = match tags.last() {
                            Some(Tag::Heading { level, .. }) => match level {
                                HeadingLevel::H1 => heading_styles[0],
                                HeadingLevel::H2 => heading_styles[1],
                                HeadingLevel::H3 => heading_styles[2],
                                HeadingLevel::H4 => heading_styles[3],
                                HeadingLevel::H5 => heading_styles[4],
                                HeadingLevel::H6 => heading_styles[5],
                            },
                            Some(Tag::Emphasis) => text_style.add_modifier(Modifier::ITALIC),
                            Some(Tag::Strong) => text_style.add_modifier(Modifier::BOLD),
                            Some(Tag::Strikethrough) => {
                                text_style.add_modifier(Modifier::CROSSED_OUT)
                            }
                            _ => text_style,
                        };
                        spans.push(Span::styled(text, style));
                    }
                }
                Event::Code(text) | Event::Html(text) => {
                    if in_table {
                        current_cell.push_str(&text);
                    } else {
                        spans.push(Span::styled(text, code_style));
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    push_line(&mut spans, &mut lines);
                    if !list_stack.is_empty() {
                        // TODO: could push indent + 2 or 3 spaces to align with
                        // the rest of the list.
                        spans.push(Span::from(get_indent(list_stack.len())));
                    }
                }
                Event::Rule => {
                    lines.push(Spans::from(Span::styled("───", rule_style)));
                    lines.push(Spans::default());
                }
                // TaskListMarker(bool) true if checked
                _ => {
                    log::warn!("unhandled markdown event {:?}", event);
                }
            }
            // build up a vec of Paragraph tui widgets
        }

        if !spans.is_empty() {
            lines.push(Spans::from(spans));
        }

        // if last line is empty, remove it
        if let Some(line) = lines.last() {
            if line.0.is_empty() {
                lines.pop();
            }
        }

        Text::from(lines)
    }
}

impl Component for Markdown {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        use tui::widgets::{Paragraph, Widget, Wrap};

        let text = self.parse(Some(&cx.editor.theme));

        let par = Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .scroll((cx.scroll.unwrap_or_default() as u16, 0));

        let margin = Margin::all(1);
        par.render(area.inner(margin), surface);
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let padding = 2;
        let contents = self.parse(None);

        // TODO: account for tab width
        let max_text_width = (viewport.0.saturating_sub(padding)).min(120);
        let (width, height) = crate::ui::text::required_size(&contents, max_text_width);

        Some((width + padding, height + padding))
    }
}

/// Word-wrap a plain table cell to `width` columns (counted in characters, to
/// match `align_cell`). Words longer than `width` are hard-split. Always returns
/// at least one (possibly empty) line.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_len = 0usize; // in chars

    for word in s.split(' ') {
        let wlen = word.chars().count();
        if wlen > width {
            // Flush the current line, then hard-split the over-long word.
            if line_len > 0 {
                out.push(std::mem::take(&mut line));
            }
            let mut chunk = String::new();
            let mut clen = 0usize;
            for ch in word.chars() {
                if clen == width {
                    out.push(std::mem::take(&mut chunk));
                    clen = 0;
                }
                chunk.push(ch);
                clen += 1;
            }
            line = chunk;
            line_len = clen;
        } else if line_len == 0 {
            line = word.to_string();
            line_len = wlen;
        } else if line_len + 1 + wlen <= width {
            line.push(' ');
            line.push_str(word);
            line_len += 1 + wlen;
        } else {
            out.push(std::mem::take(&mut line));
            line = word.to_string();
            line_len = wlen;
        }
    }
    out.push(line);
    out
}

fn align_cell(text: &str, width: usize, align: Alignment) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_string();
    }
    let pad = width - len;
    match align {
        Alignment::Right => format!("{:>width$}", text, width = width),
        Alignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
        }
        _ => format!("{:<width$}", text, width = width),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loader() -> Arc<ArcSwap<syntax::Loader>> {
        Arc::new(ArcSwap::from_pointee(
            helix_core::config::default_lang_loader(),
        ))
    }

    fn line_texts(text: &tui::text::Text) -> Vec<String> {
        text.lines
            .iter()
            .map(|l| l.0.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    const WIDE_TABLE: &str = "\
| Column Alpha | Column Beta | Column Gamma |
| ------------ | ----------- | ------------ |
| a1 value long text here | b1 | c1 more text |
";

    #[test]
    fn wrap_text_wraps_and_hard_splits() {
        assert_eq!(wrap_text("hello world foo", 5), vec!["hello", "world", "foo"]);
        assert_eq!(wrap_text("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap_text("", 10), vec![""]);
        assert_eq!(wrap_text("anything", 0), vec![""]);
    }

    #[test]
    fn table_fits_max_width() {
        let md = Markdown::new(WIDE_TABLE.to_string(), loader()).with_max_table_width(40);
        let lines = line_texts(&md.parse(None));
        for line in &lines {
            assert!(line.chars().count() <= 40, "line too wide: {line:?}");
        }
        // Header text is preserved (wrapped across lines).
        let joined = lines.join("\n");
        assert!(joined.contains("Column") && joined.contains("Alpha"));
    }

    #[test]
    fn table_without_limit_is_unconstrained() {
        let md = Markdown::new(WIDE_TABLE.to_string(), loader());
        let lines = line_texts(&md.parse(None));
        assert!(lines.iter().any(|l| l.chars().count() > 40));
    }

    #[test]
    fn narrow_table_unchanged_by_limit() {
        let src = "\
| A | B |
| - | - |
| 1 | 2 |
";
        let plain_md = Markdown::new(src.to_string(), loader());
        let plain = line_texts(&plain_md.parse(None));
        let limited_md = Markdown::new(src.to_string(), loader()).with_max_table_width(80);
        let limited = line_texts(&limited_md.parse(None));
        assert_eq!(plain, limited);
    }
}
