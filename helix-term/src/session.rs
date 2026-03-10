
/// A session entry used by the session picker for column rendering.
#[derive(Clone)]
pub struct SessionEntry {
    pub session_id: String,
    /// Display name (from ACP `title`, or first 8 chars of session_id).
    pub slug: String,
    /// Git branch — not provided by ACP; kept for struct compatibility.
    pub git_branch: String,
    /// ISO-8601 timestamp from ACP `updatedAt`.
    pub timestamp: String,
    /// Unused; kept for struct compatibility.
    pub summary: String,
}
