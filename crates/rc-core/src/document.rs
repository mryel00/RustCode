//! Open documents: the backend's source of truth for buffer contents.

use crate::{language::language_id_for_path, text, uri};
use lsp_types::TextDocumentContentChangeEvent;
use ropey::Rope;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use url::Url;

const UTF8_BOM: &str = "\u{feff}";

#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("cannot write {path}: {source}")]
    Write { path: PathBuf, source: std::io::Error },
    #[error("{0} is not a UTF-8 text file")]
    NotText(PathBuf),
    #[error("{0} is not a valid file path or URI")]
    InvalidPath(String),
    #[error("document {0} is not open")]
    NotOpen(Url),
    #[error("stale edit for {uri}: version {got} is not newer than {current}")]
    StaleVersion { uri: Url, got: i32, current: i32 },
}

#[derive(Debug, Clone)]
pub struct Document {
    pub uri: Url,
    pub path: PathBuf,
    pub language_id: String,
    pub text: Rope,
    /// Monotonically increasing; sent to language servers with every change.
    pub version: i32,
    /// Whether the file on disk started with a UTF-8 BOM, so saving preserves it.
    has_bom: bool,
}

impl Document {
    fn load(path: &Path) -> Result<Self, DocumentError> {
        let path = uri::canonicalize(path)
            .map_err(|source| DocumentError::Read { path: path.into(), source })?;
        let bytes = std::fs::read(&path)
            .map_err(|source| DocumentError::Read { path: path.clone(), source })?;
        let content = String::from_utf8(bytes).map_err(|_| DocumentError::NotText(path.clone()))?;
        if content.contains('\0') {
            return Err(DocumentError::NotText(path));
        }
        let (has_bom, body) = match content.strip_prefix(UTF8_BOM) {
            Some(rest) => (true, rest),
            None => (false, content.as_str()),
        };
        let uri = uri::path_to_uri(&path)
            .ok_or_else(|| DocumentError::InvalidPath(path.display().to_string()))?;
        Ok(Self {
            uri,
            language_id: language_id_for_path(&path).to_owned(),
            text: Rope::from_str(body),
            version: 0,
            has_bom,
            path,
        })
    }

    fn save(&self) -> Result<(), DocumentError> {
        let mut out = String::with_capacity(self.text.len_bytes() + 3);
        if self.has_bom {
            out.push_str(UTF8_BOM);
        }
        for chunk in self.text.chunks() {
            out.push_str(chunk);
        }
        std::fs::write(&self.path, out)
            .map_err(|source| DocumentError::Write { path: self.path.clone(), source })
    }
}

/// All documents currently open in the editor, keyed by URI.
#[derive(Debug, Default)]
pub struct DocumentStore {
    docs: HashMap<Url, Document>,
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens `path`, or returns the already-open document. The bool is true
    /// when the document was newly opened.
    pub fn open(&mut self, path: &Path) -> Result<(&Document, bool), DocumentError> {
        let canonical = uri::canonicalize(path)
            .map_err(|source| DocumentError::Read { path: path.into(), source })?;
        let key = uri::path_to_uri(&canonical)
            .ok_or_else(|| DocumentError::InvalidPath(path.display().to_string()))?;
        let newly_opened = !self.docs.contains_key(&key);
        if newly_opened {
            let doc = Document::load(&canonical)?;
            self.docs.insert(key.clone(), doc);
        }
        Ok((&self.docs[&key], newly_opened))
    }

    pub fn get(&self, uri: &Url) -> Option<&Document> {
        self.docs.get(uri)
    }

    /// Applies edits that move the document to `version`.
    pub fn change(
        &mut self,
        uri: &Url,
        version: i32,
        changes: &[TextDocumentContentChangeEvent],
    ) -> Result<&Document, DocumentError> {
        let doc = self.docs.get_mut(uri).ok_or_else(|| DocumentError::NotOpen(uri.clone()))?;
        if version <= doc.version {
            return Err(DocumentError::StaleVersion { uri: uri.clone(), got: version, current: doc.version });
        }
        text::apply_changes(&mut doc.text, changes);
        doc.version = version;
        Ok(doc)
    }

    pub fn save(&self, uri: &Url) -> Result<&Document, DocumentError> {
        let doc = self.docs.get(uri).ok_or_else(|| DocumentError::NotOpen(uri.clone()))?;
        doc.save()?;
        Ok(doc)
    }

    pub fn close(&mut self, uri: &Url) -> Option<Document> {
        self.docs.remove(uri)
    }

    pub fn uris(&self) -> impl Iterator<Item = &Url> {
        self.docs.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn edit(text: &str, at: (u32, u32)) -> TextDocumentContentChangeEvent {
        let pos = Position::new(at.0, at.1);
        TextDocumentContentChangeEvent { range: Some(Range::new(pos, pos)), range_length: None, text: text.into() }
    }

    #[test]
    fn open_edit_save_preserves_bom_and_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "\u{feff}fn a() {}\r\n").unwrap();

        let mut store = DocumentStore::new();
        let (doc, newly) = store.open(&path).unwrap();
        assert!(newly);
        assert_eq!(doc.language_id, "rust");
        assert_eq!(doc.text.to_string(), "fn a() {}\r\n");
        let uri = doc.uri.clone();

        store.change(&uri, 1, &[edit("pub ", (0, 0))]).unwrap();
        store.save(&uri).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{feff}pub fn a() {}\r\n");

        // Reopening returns the same live document.
        let (again, newly) = store.open(&path).unwrap();
        assert!(!newly);
        assert_eq!(again.version, 1);
    }

    #[test]
    fn rejects_stale_versions_and_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x").unwrap();
        let mut store = DocumentStore::new();
        let uri = store.open(&path).unwrap().0.uri.clone();
        store.change(&uri, 5, &[]).unwrap();
        assert!(matches!(store.change(&uri, 5, &[]), Err(DocumentError::StaleVersion { .. })));

        let bin = dir.path().join("b.bin");
        std::fs::write(&bin, [0u8, 159, 146, 150]).unwrap();
        assert!(matches!(store.open(&bin), Err(DocumentError::NotText(_))));
    }
}
