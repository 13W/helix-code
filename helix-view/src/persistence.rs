//! Editor state kept between sessions (`[editor.persistence]`): the command (`:`) and search
//! (`/`) history and the cursor and scroll position each file was last closed with.
//!
//! Each kind of state is a JSON Lines file in [`helix_loader::state_dir`]. While Helix runs,
//! entries are only appended, so several running instances can share the files. On startup each
//! file is read once and compacted down to the configured limit.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use helix_core::{Range, Selection};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{view::ViewPosition, DocumentId, Editor, ViewId};

const COMMAND_HISTORY: &str = "command-history.jsonl";
const SEARCH_HISTORY: &str = "search-history.jsonl";
const FILE_HISTORY: &str = "file-history.jsonl";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct PersistenceConfig {
    /// Reopen files at the cursor and scroll position they were closed with.
    pub old_files: bool,
    /// Keep the command history (`:` register).
    pub commands: bool,
    /// Keep the search history (`/` register).
    pub search: bool,
    /// Files whose absolute path matches one of these globs are never remembered.
    pub old_files_exclusions: PathGlobs,
    /// Number of files whose position is kept.
    pub old_files_trim: usize,
    /// Number of commands kept.
    pub commands_trim: usize,
    /// Number of searches kept.
    pub search_trim: usize,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            old_files: false,
            commands: false,
            search: false,
            old_files_exclusions: PathGlobs::new(["**/.git/**", "**/.jj/**", "**/*.jjdescription"])
                .expect("default exclusions are valid globs"),
            old_files_trim: 100,
            commands_trim: 100,
            search_trim: 100,
        }
    }
}

/// Glob patterns matched against absolute paths. Compares and serializes as its list of patterns.
#[derive(Debug, Clone)]
pub struct PathGlobs {
    patterns: Vec<String>,
    set: globset::GlobSet,
}

impl PathGlobs {
    pub fn new<I, S>(patterns: I) -> Result<Self, globset::Error>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let patterns: Vec<String> = patterns.into_iter().map(Into::into).collect();
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &patterns {
            builder.add(globset::Glob::new(pattern)?);
        }
        Ok(Self {
            set: builder.build()?,
            patterns,
        })
    }

    pub fn is_match(&self, path: &Path) -> bool {
        self.set.is_match(path)
    }
}

impl PartialEq for PathGlobs {
    fn eq(&self, other: &Self) -> bool {
        self.patterns == other.patterns
    }
}

impl Eq for PathGlobs {}

impl Serialize for PathGlobs {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.patterns.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PathGlobs {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let patterns = Vec::<String>::deserialize(deserializer)?;
        Self::new(patterns).map_err(serde::de::Error::custom)
    }
}

/// Scroll position and selection of a view on a file, in char offsets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePosition {
    anchor: usize,
    horizontal_offset: usize,
    vertical_offset: usize,
    ranges: Vec<(usize, usize)>,
    primary: usize,
}

impl FilePosition {
    fn new(view_position: ViewPosition, selection: &Selection) -> Self {
        Self {
            anchor: view_position.anchor,
            horizontal_offset: view_position.horizontal_offset,
            vertical_offset: view_position.vertical_offset,
            ranges: selection
                .ranges()
                .iter()
                .map(|range| (range.anchor, range.head))
                .collect(),
            primary: selection.primary_index(),
        }
    }

    fn view_position(&self) -> ViewPosition {
        ViewPosition {
            anchor: self.anchor,
            horizontal_offset: self.horizontal_offset,
            vertical_offset: self.vertical_offset,
        }
    }

    /// The selection, unless it no longer fits a document of `len_chars` chars.
    fn selection(&self, len_chars: usize) -> Option<Selection> {
        let fits = self.primary < self.ranges.len()
            && self.anchor <= len_chars
            && self
                .ranges
                .iter()
                .all(|&(anchor, head)| anchor.max(head) <= len_chars);
        if !fits {
            return None;
        }
        let ranges = self
            .ranges
            .iter()
            .map(|&(anchor, head)| Range::new(anchor, head))
            .collect();
        Some(Selection::new(ranges, self.primary))
    }
}

#[derive(Serialize, Deserialize)]
struct FileEntry {
    path: PathBuf,
    #[serde(flatten)]
    position: FilePosition,
}

/// Loads the state enabled in `[editor.persistence]` into `editor`. Called once on startup,
/// before any file is opened.
pub fn load(editor: &mut Editor) {
    let config = editor.config().persistence.clone();
    let dir = helix_loader::state_dir();
    if config.commands {
        load_register_history(
            editor,
            ':',
            &dir.join(COMMAND_HISTORY),
            config.commands_trim,
        );
    }
    if config.search {
        load_register_history(editor, '/', &dir.join(SEARCH_HISTORY), config.search_trim);
    }
    if config.old_files {
        editor.file_positions = load_file_positions(&dir.join(FILE_HISTORY), config.old_files_trim);
    }
}

fn load_register_history(editor: &mut Editor, register: char, path: &Path, limit: usize) {
    let mut lines: Vec<String> = read_entries(path);
    if lines.len() > limit {
        lines.drain(..lines.len() - limit);
        rewrite(path, &lines);
    }
    // `Registers::write` takes the values newest first.
    lines.reverse();
    if let Err(err) = editor.registers.write(register, lines) {
        log::warn!("failed to restore history of register {register}: {err}");
    }
}

/// Reads the newest position of each of the `limit` most recently closed files.
fn load_file_positions(path: &Path, limit: usize) -> HashMap<PathBuf, FilePosition> {
    let entries: Vec<FileEntry> = read_entries(path);
    let total = entries.len();
    let newest: HashMap<PathBuf, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| (entry.path.clone(), i))
        .collect();
    let mut kept: Vec<FileEntry> = entries
        .into_iter()
        .enumerate()
        .filter(|(i, entry)| newest[&entry.path] == *i)
        .map(|(_, entry)| entry)
        .collect();
    if kept.len() > limit {
        kept.drain(..kept.len() - limit);
    }
    if kept.len() < total {
        rewrite(path, &kept);
    }
    kept.into_iter()
        .map(|entry| (entry.path, entry.position))
        .collect()
}

/// Appends `line`, just pushed to the history `register`, to that register's history file.
pub fn push_history(editor: &Editor, register: char, line: &str) {
    let file = {
        let config = editor.config();
        let config = &config.persistence;
        match register {
            ':' if config.commands => COMMAND_HISTORY,
            '/' if config.search => SEARCH_HISTORY,
            _ => return,
        }
    };
    append(&helix_loader::state_dir().join(file), &line);
}

/// Restores the position `doc_id` was last closed with in the focused view. Called when the
/// document is opened.
pub fn restore_position(editor: &mut Editor, doc_id: DocumentId) {
    let view_id = editor.tree.focus;
    let Some(position) = editor.document(doc_id).and_then(|doc| {
        let path = doc.path()?;
        let config = editor.config();
        let config = &config.persistence;
        if !config.old_files
            || config.old_files_exclusions.is_match(path)
            || !doc.selections().contains_key(&view_id)
        {
            return None;
        }
        editor.file_positions.get(path).cloned()
    }) else {
        return;
    };
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    if let Some(selection) = position.selection(doc.text().len_chars()) {
        doc.set_selection(view_id, selection);
        doc.set_view_offset(view_id, position.view_position());
    }
}

/// Remembers the position of every document shown in `view_id`. Called before the view closes.
pub fn remember_view(editor: &mut Editor, view_id: ViewId) {
    let docs: Vec<DocumentId> = editor
        .documents()
        .filter(|doc| doc.selections().contains_key(&view_id))
        .map(|doc| doc.id())
        .collect();
    for doc_id in docs {
        remember(editor, doc_id, view_id);
    }
}

/// Remembers the position of `doc_id`, preferring the focused view. Called before the document
/// closes.
pub fn remember_document(editor: &mut Editor, doc_id: DocumentId) {
    let focus = editor.tree.focus;
    let view_id = editor.document(doc_id).and_then(|doc| {
        let views = doc.selections();
        if views.contains_key(&focus) {
            Some(focus)
        } else {
            views.keys().next().copied()
        }
    });
    if let Some(view_id) = view_id {
        remember(editor, doc_id, view_id);
    }
}

/// Remembers the position of every open document. Called when Helix exits.
pub fn remember_all(editor: &mut Editor) {
    let focus = editor.tree.focus;
    let mut views: Vec<ViewId> = editor.tree.views().map(|(view, _)| view.id).collect();
    // A document shown in several views keeps the focused view's position.
    views.sort_by_key(|&view_id| view_id == focus);
    for view_id in views {
        remember_view(editor, view_id);
    }
}

fn remember(editor: &mut Editor, doc_id: DocumentId, view_id: ViewId) {
    let entry = editor.document(doc_id).and_then(|doc| {
        let path = doc.path()?;
        let config = editor.config();
        let config = &config.persistence;
        if !config.old_files || config.old_files_exclusions.is_match(path) {
            return None;
        }
        let position = FilePosition::new(
            doc.get_view_offset(view_id)?,
            doc.selections().get(&view_id)?,
        );
        if editor.file_positions.get(path) == Some(&position) {
            return None;
        }
        Some(FileEntry {
            path: path.to_path_buf(),
            position,
        })
    });
    if let Some(entry) = entry {
        append(&helix_loader::state_dir().join(FILE_HISTORY), &entry);
        editor.file_positions.insert(entry.path, entry.position);
    }
}

/// Reads one entry per line, skipping lines that do not parse (e.g. a write cut short).
fn read_entries<T: DeserializeOwned>(path: &Path) -> Vec<T> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            log::warn!("failed to read {}: {err}", path.display());
            return Vec::new();
        }
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

fn append<T: Serialize>(path: &Path, entry: &T) {
    let result = (|| -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        // A single write per entry keeps lines appended by other instances whole.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(line.as_bytes())
    })();
    if let Err(err) = result {
        log::warn!("failed to write {}: {err}", path.display());
    }
}

/// Replaces the file with `entries` through a temporary file, so it is never left half written.
fn rewrite<T: Serialize>(path: &Path, entries: &[T]) {
    let result = (|| -> io::Result<()> {
        let dir = path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        let mut writer = BufWriter::new(file.as_file_mut());
        for entry in entries {
            serde_json::to_writer(&mut writer, entry)?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        drop(writer);
        file.persist(path).map_err(|err| err.error)?;
        Ok(())
    })();
    if let Err(err) = result {
        log::warn!("failed to rewrite {}: {err}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, cursor: usize) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path),
            position: FilePosition::new(ViewPosition::default(), &Selection::point(cursor)),
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn append_and_read_skip_broken_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join(SEARCH_HISTORY);
        append(&path, &"foo");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\"cut sho\n")
            .unwrap();
        append(&path, &"bar \"baz\"");
        let read: Vec<String> = read_entries(&path);
        assert_eq!(read, ["foo", "bar \"baz\""]);
        assert!(read_entries::<String>(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn file_positions_keep_newest_entry_of_recent_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_HISTORY);
        for entry in [
            entry("/a", 1),
            entry("/b", 2),
            entry("/a", 3),
            entry("/c", 4),
        ] {
            append(&path, &entry);
        }

        let positions = load_file_positions(&path, 2);
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[Path::new("/a")], entry("/a", 3).position);
        assert_eq!(positions[Path::new("/c")], entry("/c", 4).position);
        // The file is compacted to the kept entries, oldest first.
        let compacted = lines(&path);
        assert_eq!(compacted.len(), 2);
        assert!(compacted[0].starts_with(r#"{"path":"/a""#));
        assert!(compacted[1].starts_with(r#"{"path":"/c""#));

        // Nothing to drop: the file is not replaced.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let inode = fs::metadata(&path).unwrap().ino();
            load_file_positions(&path, 2);
            assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        }
    }

    #[test]
    fn selection_must_fit_document() {
        let selection =
            Selection::new(helix_core::smallvec![Range::new(2, 5), Range::new(8, 7)], 1);
        let position = FilePosition::new(ViewPosition::default(), &selection);
        assert_eq!(position.selection(8), Some(selection));
        assert_eq!(position.selection(7), None);

        let mut broken = position.clone();
        broken.primary = 2;
        assert_eq!(broken.selection(100), None);
    }

    #[test]
    fn default_exclusions() {
        let exclusions = PersistenceConfig::default().old_files_exclusions;
        assert!(exclusions.is_match(Path::new("/repo/.git/COMMIT_EDITMSG")));
        assert!(exclusions.is_match(Path::new("/repo/.git/worktrees/x/COMMIT_EDITMSG")));
        assert!(exclusions.is_match(Path::new("/repo/.jj/repo/op_heads")));
        assert!(exclusions.is_match(Path::new("/tmp/editor-1a2b.jjdescription")));
        assert!(!exclusions.is_match(Path::new("/repo/src/main.rs")));
        assert!(!exclusions.is_match(Path::new("/repo/.gitignore")));
    }

    #[test]
    fn config_from_toml() {
        let config: PersistenceConfig =
            toml::from_str("search = true\nold-files-exclusions = ['/tmp/**']\nsearch-trim = 5")
                .unwrap();
        assert!(config.search && !config.commands);
        assert_eq!(config.search_trim, 5);
        assert!(config.old_files_exclusions.is_match(Path::new("/tmp/x/y")));
        assert!(!config
            .old_files_exclusions
            .is_match(Path::new("/repo/.git/HEAD")));

        assert!(toml::from_str::<PersistenceConfig>("old-files-exclusions = ['a{']").is_err());
    }
}
