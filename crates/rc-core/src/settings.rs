//! Workspace settings from `.vscode/settings.json`, so projects set up for
//! VS Code behave the same here. Only the settings RustCode uses are read:
//! `files.exclude` and `search.exclude`. Like VS Code, object settings are
//! merged key by key with their defaults, so `"**/.git": false` shows `.git`.

use crate::{
    fs::FILES_EXCLUDE,
    glob::{self, ExcludeExpression, ExcludeValue},
    search::SEARCH_EXCLUDE,
};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The settings file inside the folder.
pub fn settings_path(root: &Path) -> PathBuf {
    root.join(".vscode").join("settings.json")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSettings {
    pub files_exclude: ExcludeExpression,
    pub search_exclude: ExcludeExpression,
    /// Problems with the settings file: unreadable, invalid JSON (defaults
    /// are used), or patterns that are not valid globs (left out).
    pub warnings: Vec<String>,
}

impl Default for WorkspaceSettings {
    fn default() -> Self {
        Self {
            files_exclude: ExcludeExpression::enabled(FILES_EXCLUDE.iter().map(|name| format!("**/{name}"))),
            search_exclude: ExcludeExpression::enabled(SEARCH_EXCLUDE.iter().copied()),
            warnings: Vec::new(),
        }
    }
}

impl WorkspaceSettings {
    /// Loads the folder's settings. A missing file means defaults; a file
    /// that cannot be read or parsed gives defaults plus a warning.
    pub fn load(root: &Path) -> Self {
        let path = settings_path(root);
        let failed = |message: String| Self { warnings: vec![message], ..Self::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text)
                .unwrap_or_else(|e| failed(format!("{}: {e}. Using default settings.", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => failed(format!("Cannot read {}: {e}", path.display())),
        }
    }

    /// Parses settings JSON (with comments and trailing commas, as VS Code allows).
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(&strip_jsonc(text)).map_err(|e| e.to_string())?;
        let Value::Object(root) = value else { return Err("settings must be a JSON object".into()) };
        let mut settings = Self::default();
        for (setting, target) in
            [("files.exclude", &mut settings.files_exclude), ("search.exclude", &mut settings.search_exclude)]
        {
            if let Some(Value::Object(map)) = lookup(&root, setting) {
                for (pattern, value) in map {
                    // A broken pattern must not break the explorer or search.
                    let glob = if glob::is_exception(pattern) { &pattern.trim_start()[1..] } else { pattern.as_str() };
                    if let Err(e) = glob::compile(glob.trim()) {
                        settings.warnings.push(format!("{setting}: ignoring {e}"));
                        continue;
                    }
                    target.set(pattern.clone(), ExcludeValue::from_json(value));
                }
            }
        }
        Ok(settings)
    }

    /// The excludes of a text search: `files.exclude` with `search.exclude`
    /// laid over it (VS Code's `getExcludes`).
    pub fn search_excludes(&self) -> ExcludeExpression {
        self.files_exclude.merged(&self.search_exclude)
    }
}

/// A setting either as a dotted key (`"files.exclude"`) or nested
/// (`"files": { "exclude": … }`); the dotted form wins.
fn lookup<'a>(root: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    if let Some(value) = root.get(key) {
        return Some(value);
    }
    let (section, rest) = key.split_once('.')?;
    match root.get(section)? {
        Value::Object(map) => lookup(map, rest),
        _ => None,
    }
}

/// Turns JSON with comments and trailing commas into plain JSON.
fn strip_jsonc(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // Pass 1: drop comments, keeping strings intact.
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push(c);
                while let Some(c) = chars.next() {
                    out.push(c);
                    match c {
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                out.push(escaped);
                            }
                        }
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for c in chars.by_ref() {
                    if previous == '*' && c == '/' {
                        break;
                    }
                    previous = c;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    // Pass 2: drop commas that are followed only by whitespace and `}` or `]`.
    let bytes = out.as_bytes();
    let mut result = String::with_capacity(out.len());
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in out.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
        } else if c == ',' {
            let next = bytes[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
            if matches!(next, Some(b'}' | b']')) {
                continue;
            }
        }
        result.push(c);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonc_and_merges_with_defaults() {
        let settings = WorkspaceSettings::parse(
            r#"{
                // line comment with "quotes"
                "files.exclude": { "**/.git": false, "**/*.tmp": true, /* block */ },
                "search": { "exclude": { "!node_modules/keep": true, "**/gen": { "when": "$(basename).src" }, }, },
                "url": "http://example.com/*not a comment*/",
            }"#,
        )
        .unwrap();
        assert_eq!(
            settings.files_exclude.plain_patterns(),
            ["**/.svn", "**/.hg", "**/.jj", "**/.DS_Store", "**/Thumbs.db", "**/*.tmp"]
        );
        let search = settings.search_excludes();
        assert_eq!(search.exceptions(), ["node_modules/keep"]);
        assert!(search.plain_patterns().contains(&"**/node_modules".to_owned()));
        assert!(search.entries().contains(&("**/gen".into(), ExcludeValue::When("$(basename).src".into()))));
    }

    #[test]
    fn defaults_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(WorkspaceSettings::load(dir.path()), WorkspaceSettings::default());
        std::fs::create_dir(dir.path().join(".vscode")).unwrap();
        std::fs::write(settings_path(dir.path()), "{ \"files.exclude\": ").unwrap();
        let settings = WorkspaceSettings::load(dir.path());
        assert_eq!(settings.files_exclude, WorkspaceSettings::default().files_exclude);
        assert!(settings.warnings[0].contains("Using default settings"), "{:?}", settings.warnings);
        assert!(WorkspaceSettings::parse("[]").is_err());

        let partly =
            WorkspaceSettings::parse(r#"{ "files.exclude": { "a[": true, "!b[": true, "ok": true } }"#).unwrap();
        assert!(partly.files_exclude.plain_patterns().contains(&"ok".to_owned()));
        assert_eq!(partly.files_exclude.entries().len(), 7);
        assert_eq!(partly.warnings.len(), 2, "{:?}", partly.warnings);
    }

    #[test]
    fn strips_comments_and_trailing_commas_outside_strings() {
        assert_eq!(strip_jsonc("{\"a\": \"//,}\", // c\n \"b\": [1,],}"), "{\"a\": \"//,}\", \n \"b\": [1]}");
        assert_eq!(strip_jsonc("\u{feff}{\"a\\\"//\": 1 /* x */}"), "{\"a\\\"//\": 1  }");
    }
}
