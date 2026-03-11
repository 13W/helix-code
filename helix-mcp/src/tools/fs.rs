//! Filesystem tools: list_dir, find_files, search.

use anyhow::Result;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::WalkBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::serde_lenient;

// ---------------------------------------------------------------------------
// Parameter structs
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct ListDirParams {
    /// Directory path to list (absolute or relative to CWD).
    pub path: String,
    /// Whether to recurse into subdirectories. Default: false.
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct FindFilesParams {
    /// Glob pattern, e.g. `**/*.rs`.
    pub glob: String,
    /// Root directory to search. Default: `.`.
    #[serde(default = "default_dot")]
    pub path: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Regex pattern to search for.
    pub pattern: String,
    /// Root directory to search. Default: `.`.
    #[serde(default = "default_dot")]
    pub path: String,
    /// Lines of context before and after each match. Default: 0.
    #[serde(default, deserialize_with = "serde_lenient::string_or_usize")]
    pub context_lines: usize,
    /// Case-sensitive matching. Default: true.
    #[serde(default = "default_true")]
    pub case_sensitive: bool,
}

fn default_dot() -> String {
    ".".to_string()
}
fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct DirEntry {
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Serialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: u64,
    pub text: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub fn handle_list_dir(params: ListDirParams) -> serde_json::Value {
    let root = PathBuf::from(&params.path);
    let mut builder = WalkBuilder::new(&root);
    builder.hidden(false).git_ignore(true);
    if !params.recursive {
        builder.max_depth(Some(1));
    }

    let entries: Vec<DirEntry> = builder
        .build()
        .filter_map(|e| e.ok())
        .skip(1) // skip the root entry itself
        .map(|e| {
            let path = e.path().to_string_lossy().into_owned();
            let is_dir = e.path().is_dir();
            let size = if is_dir {
                None
            } else {
                e.metadata().ok().map(|m| m.len())
            };
            DirEntry {
                path,
                kind: if is_dir {
                    "dir".to_string()
                } else {
                    "file".to_string()
                },
                size,
            }
        })
        .collect();

    serde_json::json!({ "entries": entries })
}

pub fn handle_find_files(params: FindFilesParams) -> Result<serde_json::Value> {
    let matcher = globset::GlobBuilder::new(&params.glob)
        .case_insensitive(false)
        .build()
        .map_err(|e| anyhow::anyhow!("invalid glob: {e}"))?
        .compile_matcher();

    let root = PathBuf::from(&params.path);
    let files: Vec<String> = WalkBuilder::new(&root)
        .git_ignore(true)
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file() && matcher.is_match(e.path()))
        .map(|e| e.path().to_string_lossy().into_owned())
        .collect();

    Ok(serde_json::json!({ "files": files }))
}

pub fn handle_search(params: SearchParams) -> Result<serde_json::Value> {
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(!params.case_sensitive)
        .build(&params.pattern)
    {
        Ok(m) => m,
        Err(e) => {
            return Ok(serde_json::json!({ "error": format!("invalid regex: {e}") }));
        }
    };

    let ctx = params.context_lines;
    let searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .before_context(ctx)
        .after_context(ctx)
        .line_number(true)
        .build();

    let root = PathBuf::from(&params.path);
    let mut matches: Vec<SearchMatch> = Vec::new();
    let mut truncated = false;

    'walk: for entry in WalkBuilder::new(&root).git_ignore(true).build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.path().is_file() {
            continue;
        }

        let path_str = entry.path().to_string_lossy().into_owned();
        let mut sink = ContextSink {
            path: path_str,
            results: &mut matches,
            pending_before: Vec::new(),
            last_match_idx: None,
            max_matches: 500,
            truncated: false,
        };

        let _ = searcher.clone().search_path(&matcher, entry.path(), &mut sink);

        if sink.truncated {
            truncated = true;
            break 'walk;
        }
    }

    Ok(serde_json::json!({ "matches": matches, "truncated": truncated }))
}

// ---------------------------------------------------------------------------
// Custom Sink for context support
// ---------------------------------------------------------------------------

struct ContextSink<'a> {
    path: String,
    results: &'a mut Vec<SearchMatch>,
    pending_before: Vec<String>,
    last_match_idx: Option<usize>,
    max_matches: usize,
    truncated: bool,
}

impl Sink for ContextSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.results.len() >= self.max_matches {
            self.truncated = true;
            return Ok(false);
        }

        let text = std::str::from_utf8(mat.bytes())
            .unwrap_or("")
            .trim_end_matches(['\n', '\r'])
            .to_string();
        let line = mat.line_number().unwrap_or(0);

        let m = SearchMatch {
            path: self.path.clone(),
            line,
            text,
            context_before: std::mem::take(&mut self.pending_before),
            context_after: Vec::new(),
        };
        self.last_match_idx = Some(self.results.len());
        self.results.push(m);
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        let line = std::str::from_utf8(ctx.bytes())
            .unwrap_or("")
            .trim_end_matches(['\n', '\r'])
            .to_string();

        match ctx.kind() {
            SinkContextKind::Before => {
                self.pending_before.push(line);
            }
            SinkContextKind::After => {
                if let Some(idx) = self.last_match_idx {
                    self.results[idx].context_after.push(line);
                }
            }
            SinkContextKind::Other => {}
        }
        Ok(true)
    }

    fn context_break(
        &mut self,
        _searcher: &grep_searcher::Searcher,
    ) -> Result<bool, Self::Error> {
        self.pending_before.clear();
        self.last_match_idx = None;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Error conversion helper
// ---------------------------------------------------------------------------

pub fn to_mcp_err(e: impl std::fmt::Display) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(e.to_string(), None)
}
