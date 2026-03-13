use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Top-level structure for `.helix/launch.toml`.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct LaunchConfig {
    #[serde(default)]
    pub launch: Vec<LaunchEntry>,
}

/// A single named debug launch configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LaunchEntry {
    /// Display name shown in the picker.
    pub name: String,
    /// Language ID (e.g. "rust", "go", "javascript") — used to find the DebugAdapterConfig.
    pub language: String,
    /// Template name within that language's debugger config (e.g. "binary", "source").
    pub template: String,
    /// Pre-filled positional parameters substituted into {0}, {1}, … in the template args.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra DAP arguments merged on top of template args (e.g. env, stopOnEntry, sourceMaps).
    #[serde(default)]
    pub extra: HashMap<String, Value>,
}
