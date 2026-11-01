//! Which language server handles which language.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Stable identifier, e.g. "rust-analyzer".
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// LSP language identifiers this server handles.
    pub languages: Vec<String>,
}

impl ServerConfig {
    fn new(id: &str, command: &str, args: &[&str], languages: &[&str]) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            languages: languages.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Built-in defaults. Servers are found on `PATH`; missing ones are reported, not fatal.
pub fn default_servers() -> Vec<ServerConfig> {
    vec![
        ServerConfig::new("rust-analyzer", "rust-analyzer", &[], &["rust"]),
        ServerConfig::new(
            "typescript-language-server",
            "typescript-language-server",
            &["--stdio"],
            &["typescript", "typescriptreact", "javascript", "javascriptreact"],
        ),
        ServerConfig::new("pyright", "pyright-langserver", &["--stdio"], &["python"]),
        ServerConfig::new("clangd", "clangd", &[], &["c", "cpp"]),
        ServerConfig::new("gopls", "gopls", &[], &["go"]),
    ]
}
