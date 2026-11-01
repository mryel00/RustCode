//! RustCode core: everything the editor knows about files and text that does
//! not depend on the UI or on language servers.

pub mod document;
pub mod fs;
pub mod language;
pub mod search;
pub mod text;
pub mod uri;
pub mod watcher;

pub use document::{DiskChange, DiskRead, DiskStamp, Document, DocumentError, DocumentStore, SaveOutcome};
pub use fs::{DirEntry, list_dir};
pub use language::language_id_for_path;
pub use search::{FileMatches, SearchComplete, SearchError, TextMatch, TextSearch, TextSearchQuery};
pub use watcher::{FileChange, FileChangeKind, FileWatcher, WatchEvent};
