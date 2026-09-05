//! `getDiagnostics` (PROTO §4.4): URI ↔ path conversion and the JSON shape
//! the CLI expects, which mirrors what the VS Code extension produces from
//! `languages.getDiagnostics()`.

use std::path::{Path, PathBuf};

use helix_mcp_types::{DiagnosticItem, FileDiagnostics};
use serde::Serialize;

/// Convert a `file://` URI (as sent by the CLI: `"file://" + absolute path`,
/// usually without percent-encoding) to a path.
pub fn uri_to_path(uri: &str) -> anyhow::Result<PathBuf> {
    let rest = uri
        .strip_prefix("file://")
        .ok_or_else(|| anyhow::anyhow!("unsupported URI scheme: {uri}"))?;
    // `file://host/path` is not produced by the CLI; treat anything before
    // the first `/` as a (dropped) authority so `file://localhost/x` still works.
    let rest = if !rest.starts_with('/') {
        match rest.find('/') {
            Some(idx) => &rest[idx..],
            None => rest,
        }
    } else {
        rest
    };
    let decoded = percent_decode(rest);
    #[cfg(windows)]
    let decoded = strip_drive_slash(&decoded).to_string();
    if decoded.is_empty() {
        anyhow::bail!("empty path in URI: {uri}");
    }
    Ok(PathBuf::from(decoded))
}

/// `"file://" + absolute path`, no percent-encoding — what `Uri.toString(true)`
/// yields in VS Code and what the CLI's `normalizeFileUri` strips again.
pub fn path_to_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        let text = text.replace('\\', "/");
        if text.starts_with('/') {
            format!("file://{text}")
        } else {
            format!("file:///{text}")
        }
    }
    #[cfg(not(windows))]
    {
        format!("file://{text}")
    }
}

/// `/C:/x` → `C:/x`.
#[cfg(windows)]
fn strip_drive_slash(path: &str) -> &str {
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        &path[1..]
    } else {
        path
    }
}

/// Decode `%XX` escapes; invalid sequences are kept verbatim.
fn percent_decode(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &input[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `DiagnosticSeverity` enum member names as VS Code prints them.
pub fn severity_label(severity: &str) -> &'static str {
    match severity {
        "error" | "Error" => "Error",
        "warning" | "Warning" => "Warning",
        "info" | "information" | "Information" => "Information",
        _ => "Hint",
    }
}

#[derive(Debug, Serialize)]
struct FileEntry {
    uri: String,
    #[serde(rename = "linesInFile", skip_serializing_if = "Option::is_none")]
    lines_in_file: Option<usize>,
    diagnostics: Vec<Entry>,
}

#[derive(Debug, Serialize)]
struct Entry {
    message: String,
    severity: &'static str,
    range: RangeEntry,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

#[derive(Debug, Serialize)]
struct RangeEntry {
    start: PositionEntry,
    end: PositionEntry,
}

#[derive(Debug, Serialize)]
struct PositionEntry {
    line: usize,
    character: usize,
}

fn entry(item: DiagnosticItem) -> Entry {
    Entry {
        message: item.message,
        severity: severity_label(&item.severity),
        range: RangeEntry {
            start: PositionEntry {
                line: item.line,
                character: item.col,
            },
            end: PositionEntry {
                line: item.end_line,
                character: item.end_col,
            },
        },
        source: item.source,
        code: item.code,
    }
}

/// Render the tool result text: `JSON.stringify(array, null, 2)`.
pub fn render(files: Vec<FileDiagnostics>) -> String {
    let entries: Vec<FileEntry> = files
        .into_iter()
        .map(|file| FileEntry {
            uri: path_to_uri(&file.path),
            lines_in_file: file.lines_in_file,
            diagnostics: file.items.into_iter().map(entry).collect(),
        })
        .collect();
    serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn unix_roundtrip() {
        let p = uri_to_path("file:///home/u/src/main.rs").unwrap();
        assert_eq!(p, PathBuf::from("/home/u/src/main.rs"));
        assert_eq!(path_to_uri(&p), "file:///home/u/src/main.rs");
    }

    #[test]
    fn spaces_and_unicode_are_not_encoded() {
        let p = uri_to_path("file:///tmp/my dir/файл.rs").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/my dir/файл.rs"));
        assert_eq!(path_to_uri(&p), "file:///tmp/my dir/файл.rs");
    }

    #[test]
    fn percent_encoded_input_is_decoded() {
        let p = uri_to_path("file:///tmp/my%20dir/a.rs").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/my dir/a.rs"));
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
    }

    #[test]
    fn authority_is_dropped() {
        assert_eq!(
            uri_to_path("file://localhost/etc/hosts").unwrap(),
            PathBuf::from("/etc/hosts")
        );
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(uri_to_path("untitled:Untitled-1").is_err());
        assert!(uri_to_path("file://").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_letter() {
        let p = uri_to_path("file:///C:/Users/x/a.rs").unwrap();
        assert_eq!(p, PathBuf::from("C:/Users/x/a.rs"));
        assert_eq!(
            path_to_uri(Path::new("C:\\Users\\x\\a.rs")),
            "file:///C:/Users/x/a.rs"
        );
    }

    #[test]
    fn severity_names() {
        assert_eq!(severity_label("error"), "Error");
        assert_eq!(severity_label("warning"), "Warning");
        assert_eq!(severity_label("info"), "Information");
        assert_eq!(severity_label("hint"), "Hint");
        assert_eq!(severity_label("weird"), "Hint");
    }

    #[test]
    fn render_matches_extension_shape() {
        let files = vec![
            FileDiagnostics {
                path: PathBuf::from("/w/a.rs"),
                lines_in_file: Some(12),
                items: vec![DiagnosticItem {
                    path: PathBuf::from("/w/a.rs"),
                    line: 3,
                    col: 4,
                    end_line: 3,
                    end_col: 9,
                    severity: "error".into(),
                    message: "mismatched types".into(),
                    source: Some("rust-analyzer".into()),
                    code: Some("E0308".into()),
                }],
            },
            FileDiagnostics {
                path: PathBuf::from("/w/b.rs"),
                lines_in_file: None,
                items: vec![],
            },
        ];
        let text = render(files);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            json!([
                {
                    "uri": "file:///w/a.rs",
                    "linesInFile": 12,
                    "diagnostics": [{
                        "message": "mismatched types",
                        "severity": "Error",
                        "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}},
                        "source": "rust-analyzer",
                        "code": "E0308"
                    }]
                },
                { "uri": "file:///w/b.rs", "diagnostics": [] }
            ])
        );
        // pretty-printed with two-space indent like JSON.stringify(x, null, 2)
        assert!(text.starts_with("[\n  {\n    \"uri\""));
        // key order as written by the extension (serde_json::Value sorts keys, so check the text)
        let pos = |key: &str| text.find(key).unwrap();
        assert!(pos("\"uri\"") < pos("\"linesInFile\""));
        assert!(pos("\"linesInFile\"") < pos("\"diagnostics\""));
    }
}
