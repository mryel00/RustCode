//! `workspace/didChangeWatchedFiles` registrations of one server.
//!
//! Servers register glob patterns with `client/registerCapability`; file
//! events from the workspace watcher are matched against them, as
//! vscode-languageclient's file system watcher feature does.

use globset::{GlobBuilder, GlobMatcher};
use lsp_types::{DidChangeWatchedFilesRegistrationOptions, FileChangeType, FileEvent, GlobPattern, OneOf, WatchKind};
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf};

pub const METHOD: &str = "workspace/didChangeWatchedFiles";

struct Watcher {
    /// Set for relative patterns, which match paths below this folder.
    base: Option<PathBuf>,
    matcher: GlobMatcher,
    kind: WatchKind,
}

impl Watcher {
    fn matches(&self, path: &std::path::Path, kind: WatchKind) -> bool {
        if !self.kind.intersects(kind) {
            return false;
        }
        match &self.base {
            Some(base) => path.strip_prefix(base).is_ok_and(|rel| self.matcher.is_match(rel)),
            None => self.matcher.is_match(path),
        }
    }
}

#[derive(Default)]
pub struct WatchedFiles {
    registrations: HashMap<String, Vec<Watcher>>,
}

impl WatchedFiles {
    /// Adds the watchers of one registration. Patterns that do not compile are
    /// skipped and reported in the error, the rest still apply.
    pub fn register(&mut self, id: String, options: Value) -> Result<(), String> {
        let options: DidChangeWatchedFilesRegistrationOptions =
            serde_json::from_value(options).map_err(|e| format!("invalid {METHOD} registration: {e}"))?;
        let mut watchers = Vec::new();
        let mut errors = Vec::new();
        for w in options.watchers {
            let (base, pattern) = match w.glob_pattern {
                GlobPattern::String(pattern) => (None, pattern),
                GlobPattern::Relative(rel) => {
                    let uri = match rel.base_uri {
                        OneOf::Left(folder) => folder.uri,
                        OneOf::Right(uri) => uri,
                    };
                    match rc_core::uri::uri_to_path(&uri) {
                        Some(base) => (Some(base), rel.pattern),
                        None => {
                            errors.push(format!("unsupported base URI {uri}"));
                            continue;
                        }
                    }
                }
            };
            let glob = GlobBuilder::new(&pattern)
                .literal_separator(true)
                .case_insensitive(!cfg!(target_os = "linux"))
                .build();
            match glob {
                Ok(glob) => watchers.push(Watcher {
                    base,
                    matcher: glob.compile_matcher(),
                    kind: w.kind.unwrap_or(WatchKind::all()),
                }),
                Err(e) => errors.push(format!("{pattern}: {e}")),
            }
        }
        self.registrations.insert(id, watchers);
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }

    pub fn unregister(&mut self, id: &str) {
        self.registrations.remove(id);
    }

    pub fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }

    /// The events some registered watcher is interested in.
    pub fn filter(&self, changes: &[FileEvent]) -> Vec<FileEvent> {
        changes
            .iter()
            .filter(|change| {
                let Some(path) = rc_core::uri::uri_to_path(&change.uri) else { return false };
                let kind = match change.typ {
                    FileChangeType::CREATED => WatchKind::Create,
                    FileChangeType::DELETED => WatchKind::Delete,
                    _ => WatchKind::Change,
                };
                self.registrations.values().flatten().any(|w| w.matches(&path, kind))
            })
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use url::Url;

    fn event(path: &str, typ: FileChangeType) -> FileEvent {
        FileEvent { uri: Url::from_file_path(path).unwrap(), typ }
    }

    #[cfg(unix)]
    #[test]
    fn matches_string_and_relative_patterns_by_kind() {
        let mut w = WatchedFiles::default();
        w.register(
            "1".into(),
            json!({"watchers": [
                {"globPattern": "**/Cargo.toml"},
                {"globPattern": {"baseUri": "file:///w", "pattern": "src/*.rs"}, "kind": 4},
                {"globPattern": "/w/{a,b}.json", "kind": 1},
            ]}),
        )
        .unwrap();
        let changes = [
            event("/w/Cargo.toml", FileChangeType::CHANGED),
            event("/w/crates/x/Cargo.toml", FileChangeType::CREATED),
            event("/w/src/main.rs", FileChangeType::DELETED),
            event("/w/src/main.rs", FileChangeType::CHANGED), // kind not watched
            event("/w/src/deep/mod.rs", FileChangeType::DELETED), // `*` stays in one segment
            event("/w/a.json", FileChangeType::CREATED),
            event("/w/c.json", FileChangeType::CREATED),
        ];
        let kept: Vec<_> = w.filter(&changes).into_iter().map(|e| (e.uri.path().to_owned(), e.typ)).collect();
        assert_eq!(
            kept,
            [
                ("/w/Cargo.toml".to_owned(), FileChangeType::CHANGED),
                ("/w/crates/x/Cargo.toml".to_owned(), FileChangeType::CREATED),
                ("/w/src/main.rs".to_owned(), FileChangeType::DELETED),
                ("/w/a.json".to_owned(), FileChangeType::CREATED),
            ]
        );

        w.unregister("1");
        assert!(w.filter(&changes).is_empty());
    }

    #[test]
    fn bad_patterns_are_reported_but_others_apply() {
        let mut w = WatchedFiles::default();
        let err = w.register("1".into(), json!({"watchers": [{"globPattern": "a/[b"}, {"globPattern": "**/*.rs"}]}));
        assert!(err.is_err());
        assert!(!w.is_empty());
    }
}
