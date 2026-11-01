//! RustCode core: everything the editor knows about files and text that does
//! not depend on the UI or on language servers.

pub mod document;
pub mod fs;
pub mod language;
pub mod text;
pub mod uri;

pub use document::{Document, DocumentError, DocumentStore};
pub use fs::{DirEntry, list_dir};
pub use language::language_id_for_path;
