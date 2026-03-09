use std::path::{Path, PathBuf};
use helix_acp::DisplayLine;

#[derive(Clone)]
pub struct SessionEntry {
    pub session_id: String,
    pub slug: String,
    pub git_branch: String,
    pub timestamp: String,
    pub summary: String,
}

/// Returns `~/.claude/projects/<encoded-cwd>/` for the current working directory.
pub fn sessions_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let cwd = std::env::current_dir().ok()?;
    let encoded = cwd.to_string_lossy().replace('/', "-");
    Some(PathBuf::from(home).join(".claude/projects").join(encoded))
}

fn parse_session_file(path: &Path) -> Option<SessionEntry> {
    use std::io::{BufRead, BufReader};

    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut session_id = String::new();
    let mut slug = String::new();
    let mut git_branch = String::new();
    let mut timestamp = String::new();
    let mut summary = String::new();

    for (i, line) in reader.lines().enumerate() {
        if i >= 50 {
            break;
        }
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // First line with sessionId carries the session metadata.
        if session_id.is_empty() {
            if let Some(id) = obj.get("sessionId").and_then(|v| v.as_str()) {
                session_id = id.to_owned();
                if let Some(s) = obj.get("slug").and_then(|v| v.as_str()) {
                    slug = s.to_owned();
                }
                if let Some(b) = obj.get("gitBranch").and_then(|v| v.as_str()) {
                    git_branch = b.to_owned();
                }
                if let Some(t) = obj.get("timestamp").and_then(|v| v.as_str()) {
                    timestamp = t.to_owned();
                }
            }
        }

        // First real user message becomes the summary.
        if summary.is_empty() {
            let is_user = obj.get("type").and_then(|v| v.as_str()) == Some("user");
            let is_meta = obj
                .get("isMeta")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if is_user && !is_meta {
                if let Some(content) = obj.get("message").and_then(|m| m.get("content")) {
                    if let Some(text) = content.as_str() {
                        summary = text.chars().take(80).collect();
                    } else if let Some(arr) = content.as_array() {
                        for part in arr {
                            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                    summary = text.chars().take(80).collect();
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        if !session_id.is_empty() && !summary.is_empty() {
            break;
        }
    }

    if session_id.is_empty() {
        return None;
    }
    if slug.is_empty() {
        slug = session_id[..session_id.len().min(8)].to_owned();
    }
    if summary.is_empty() {
        summary = slug.clone();
    }

    Some(SessionEntry {
        session_id,
        slug,
        git_branch,
        timestamp,
        summary,
    })
}

/// Load conversation history from a session file as `DisplayLine` entries.
///
/// Tries `<sessions_dir>/<session_id>.jsonl` first, then scans the directory.
/// Returns an empty vec if the file is not found or cannot be parsed.
pub fn load_history(session_id: &str) -> Vec<DisplayLine> {
    let dir = match sessions_dir() {
        Some(d) => d,
        None => return Vec::new(),
    };

    // Claude Code names files <session_id>.jsonl — try direct path first.
    let direct = dir.join(format!("{}.jsonl", session_id));
    if direct.exists() {
        return parse_history_file(&direct);
    }

    // Fallback: scan directory and match sessionId in the first line.
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    for entry in read_dir.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Some(first) = content.lines().next() {
                if let Ok(obj) = serde_json::from_str::<serde_json::Value>(first) {
                    if obj.get("sessionId").and_then(|v| v.as_str()) == Some(session_id) {
                        return parse_history_file(&path);
                    }
                }
            }
        }
    }

    Vec::new()
}

fn parse_history_file(path: &Path) -> Vec<DisplayLine> {
    use std::io::{BufRead, BufReader};

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);
    let mut out: Vec<DisplayLine> = Vec::new();

    for line in reader.lines().filter_map(|l| l.ok()) {
        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = obj.get("type").and_then(|v| v.as_str());
        let is_meta = obj
            .get("isMeta")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        match msg_type {
            Some("user") if !is_meta => {
                let text = extract_text_content(&obj["message"]["content"]);
                if !text.is_empty() {
                    if !out.is_empty() {
                        out.push(DisplayLine::Separator);
                    }
                    out.push(DisplayLine::UserMessage(text));
                }
            }
            Some("assistant") => {
                let text = extract_text_content(&obj["message"]["content"]);
                if !text.is_empty() {
                    out.push(DisplayLine::Text(text));
                }
            }
            _ => {}
        }
    }

    out
}

fn extract_text_content(content: &serde_json::Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    block.get("text").and_then(|t| t.as_str()).map(str::to_owned)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");
    }
    String::new()
}

/// Read all `*.jsonl` from `sessions_dir()`, sorted by timestamp descending.
pub fn list_sessions() -> Vec<SessionEntry> {
    let dir = match sessions_dir() {
        Some(d) => d,
        None => return Vec::new(),
    };

    let read_dir = match std::fs::read_dir(&dir) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    let mut entries: Vec<SessionEntry> = read_dir
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                == Some("jsonl")
        })
        .filter_map(|e| parse_session_file(&e.path()))
        .collect();

    // ISO-8601 timestamps sort correctly as strings.
    entries.sort_unstable_by(|a, b| b.timestamp.cmp(&a.timestamp));
    entries
}
