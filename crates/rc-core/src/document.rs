//! Open documents: the backend's source of truth for buffer contents.

use crate::{language::language_id_for_path, text, uri};
use lsp_types::TextDocumentContentChangeEvent;
use ropey::Rope;
use serde::Serialize;
use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hasher},
    io,
    path::{Path, PathBuf},
    time::SystemTime,
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

/// Identifies the content of a file on disk.
///
/// Size and modification time are a cheap first check; the content hash
/// decides, so touching a file or our own saves are not mistaken for changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskStamp {
    len: u64,
    modified: Option<SystemTime>,
    hash: u64,
}

impl DiskStamp {
    fn of_bytes(bytes: &[u8], modified: Option<SystemTime>) -> Self {
        let mut hasher = DefaultHasher::new();
        hasher.write(bytes);
        Self { len: bytes.len() as u64, modified, hash: hasher.finish() }
    }

    fn same_content(&self, other: &Self) -> bool {
        self.len == other.len && self.hash == other.hash
    }

    /// The current state of `path`, or `None` if there is no file. When size
    /// and modification time match `known`, the file is not read again.
    pub fn read(path: &Path, known: Option<&DiskStamp>) -> io::Result<Option<DiskStamp>> {
        let meta = match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let modified = meta.modified().ok();
        if let Some(known) = known
            && known.len == meta.len()
            && known.modified.is_some()
            && known.modified == modified
        {
            return Ok(Some(known.clone()));
        }
        match read_stamped(path) {
            Ok((bytes, modified)) => Ok(Some(Self::of_bytes(&bytes, modified))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Reads a file together with its modification time, taken before reading so
/// a write racing with the read shows up as a newer time later.
fn read_stamped(path: &Path) -> io::Result<(Vec<u8>, Option<SystemTime>)> {
    let modified = std::fs::metadata(path)?.modified().ok();
    Ok((std::fs::read(path)?, modified))
}

/// Decodes file bytes as editor text: UTF-8 without NULs, BOM split off.
fn decode(path: &Path, bytes: Vec<u8>) -> Result<(String, bool), DocumentError> {
    let mut content = String::from_utf8(bytes).map_err(|_| DocumentError::NotText(path.into()))?;
    if content.contains('\0') {
        return Err(DocumentError::NotText(path.into()));
    }
    let has_bom = content.starts_with(UTF8_BOM);
    if has_bom {
        content.drain(..UTF8_BOM.len());
    }
    Ok((content, has_bom))
}

/// A change in how an open document relates to its file on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskChange {
    pub uri: Url,
    /// Whether the file exists. A document whose file is gone stays open.
    pub exists: bool,
    /// Whether the file now differs from what the document was last loaded
    /// from or saved to.
    pub content_changed: bool,
}

/// The result of [`DocumentStore::save`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SaveOutcome {
    Saved,
    /// The file changed on disk since the document last synced with it.
    /// Nothing was written; save again with `overwrite` to replace it.
    Conflict,
}

/// File content read by [`DocumentStore::read_from_disk`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskRead {
    /// Pass to [`DocumentStore::accept_disk_read`] once the buffer holds `text`.
    pub read_id: u64,
    pub text: String,
}

#[derive(Debug, Clone)]
struct PendingRead {
    id: u64,
    stamp: DiskStamp,
    has_bom: bool,
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
    /// The disk content this document was last loaded from or saved to.
    synced: DiskStamp,
    /// Whether the file was last seen deleted.
    missing: bool,
    pending_read: Option<PendingRead>,
}

impl Document {
    fn load(path: &Path) -> Result<Self, DocumentError> {
        let path = uri::canonicalize(path)
            .map_err(|source| DocumentError::Read { path: path.into(), source })?;
        let (bytes, modified) =
            read_stamped(&path).map_err(|source| DocumentError::Read { path: path.clone(), source })?;
        let synced = DiskStamp::of_bytes(&bytes, modified);
        let (body, has_bom) = decode(&path, bytes)?;
        let uri = uri::path_to_uri(&path)
            .ok_or_else(|| DocumentError::InvalidPath(path.display().to_string()))?;
        Ok(Self {
            uri,
            language_id: language_id_for_path(&path).to_owned(),
            text: Rope::from_str(&body),
            version: 0,
            has_bom,
            synced,
            missing: false,
            pending_read: None,
            path,
        })
    }

    fn save(&mut self) -> Result<(), DocumentError> {
        let mut out = String::with_capacity(self.text.len_bytes() + 3);
        if self.has_bom {
            out.push_str(UTF8_BOM);
        }
        for chunk in self.text.chunks() {
            out.push_str(chunk);
        }
        let write_err = |source| DocumentError::Write { path: self.path.clone(), source };
        std::fs::write(&self.path, &out).map_err(write_err)?;
        let modified = std::fs::metadata(&self.path).ok().and_then(|m| m.modified().ok());
        self.synced = DiskStamp::of_bytes(out.as_bytes(), modified);
        self.missing = false;
        Ok(())
    }

    /// Whether the file was last seen deleted.
    pub fn is_missing(&self) -> bool {
        self.missing
    }

    pub fn synced_stamp(&self) -> &DiskStamp {
        &self.synced
    }
}

/// All documents currently open in the editor, keyed by URI.
#[derive(Debug, Default)]
pub struct DocumentStore {
    docs: HashMap<Url, Document>,
    next_read_id: u64,
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

    /// Writes the document to disk. Unless `overwrite` is set, refuses with
    /// [`SaveOutcome::Conflict`] when the file changed on disk since the
    /// document last synced with it. A deleted file is simply recreated.
    pub fn save(&mut self, uri: &Url, overwrite: bool) -> Result<SaveOutcome, DocumentError> {
        let doc = self.docs.get_mut(uri).ok_or_else(|| DocumentError::NotOpen(uri.clone()))?;
        if !overwrite {
            let current = DiskStamp::read(&doc.path, Some(&doc.synced))
                .map_err(|source| DocumentError::Read { path: doc.path.clone(), source })?;
            if current.is_some_and(|c| !c.same_content(&doc.synced)) {
                return Ok(SaveOutcome::Conflict);
            }
        }
        doc.save()?;
        doc.pending_read = None;
        Ok(SaveOutcome::Saved)
    }

    /// Reads the document's file without changing the buffer. The caller
    /// replaces the buffer contents through normal edits, then confirms with
    /// [`accept_disk_read`](Self::accept_disk_read).
    pub fn read_from_disk(&mut self, uri: &Url) -> Result<DiskRead, DocumentError> {
        let doc = self.docs.get_mut(uri).ok_or_else(|| DocumentError::NotOpen(uri.clone()))?;
        let (bytes, modified) =
            read_stamped(&doc.path).map_err(|source| DocumentError::Read { path: doc.path.clone(), source })?;
        let stamp = DiskStamp::of_bytes(&bytes, modified);
        let (text, has_bom) = decode(&doc.path, bytes)?;
        self.next_read_id += 1;
        let id = self.next_read_id;
        doc.pending_read = Some(PendingRead { id, stamp, has_bom });
        Ok(DiskRead { read_id: id, text })
    }

    /// Marks the document as synced with the disk content returned by
    /// `read_from_disk`. Ignored if another read or a save happened since.
    pub fn accept_disk_read(&mut self, uri: &Url, read_id: u64) -> Result<(), DocumentError> {
        let doc = self.docs.get_mut(uri).ok_or_else(|| DocumentError::NotOpen(uri.clone()))?;
        if doc.pending_read.as_ref().is_some_and(|p| p.id == read_id) {
            let read = doc.pending_read.take().expect("checked above");
            doc.synced = read.stamp;
            doc.has_bom = read.has_bom;
            doc.missing = false;
        }
        Ok(())
    }

    /// Open documents at or below `path` (a file, or a folder that changed),
    /// with the disk state each was last synced with.
    pub fn affected_by(&self, path: &Path) -> Vec<(Url, PathBuf, DiskStamp)> {
        self.docs
            .values()
            .filter(|d| d.path.starts_with(path))
            .map(|d| (d.uri.clone(), d.path.clone(), d.synced.clone()))
            .collect()
    }

    /// Every open document, as [`affected_by`](Self::affected_by) returns them.
    pub fn all_disk_states(&self) -> Vec<(Url, PathBuf, DiskStamp)> {
        self.docs.values().map(|d| (d.uri.clone(), d.path.clone(), d.synced.clone())).collect()
    }

    /// Records what is now on disk for `uri` (`None`: no file) and reports how
    /// that changed the document's relation to its file. Unchanged content
    /// with a new modification time just refreshes the stored stamp.
    pub fn observe_disk(&mut self, uri: &Url, observed: Option<DiskStamp>) -> Option<DiskChange> {
        let doc = self.docs.get_mut(uri)?;
        let Some(observed) = observed else {
            if doc.missing {
                return None;
            }
            doc.missing = true;
            return Some(DiskChange { uri: uri.clone(), exists: false, content_changed: false });
        };
        let reappeared = std::mem::replace(&mut doc.missing, false);
        let content_changed = !observed.same_content(&doc.synced);
        if !content_changed {
            doc.synced = observed;
        }
        (reappeared || content_changed).then(|| DiskChange { uri: uri.clone(), exists: true, content_changed })
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
        assert_eq!(store.save(&uri, false).unwrap(), SaveOutcome::Saved);
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

    /// Sets a file's modification time a few seconds into the future so the
    /// cheap size+mtime check cannot mistake it for the previous content.
    fn bump_mtime(path: &Path) {
        let t = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn tracks_external_changes_deletes_and_touches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").unwrap();
        let mut store = DocumentStore::new();
        let uri = store.open(&path).unwrap().0.uri.clone();
        let path = store.get(&uri).unwrap().path.clone();
        let observe = |store: &mut DocumentStore| {
            let (_, p, known) = store.affected_by(&path).pop().unwrap();
            let now = DiskStamp::read(&p, Some(&known)).unwrap();
            store.observe_disk(&uri, now)
        };

        // Touching without changing content is not a change.
        bump_mtime(&path);
        assert_eq!(observe(&mut store), None);

        // Same size, different content.
        std::fs::write(&path, "two").unwrap();
        bump_mtime(&path);
        let change = observe(&mut store).unwrap();
        assert!(change.exists && change.content_changed);

        std::fs::remove_file(&path).unwrap();
        assert_eq!(observe(&mut store).unwrap(), DiskChange { uri: uri.clone(), exists: false, content_changed: false });
        assert_eq!(observe(&mut store), None, "reported once");

        // Restored with the content the document was loaded from.
        std::fs::write(&path, "one").unwrap();
        let change = observe(&mut store).unwrap();
        assert!(change.exists && !change.content_changed);
    }

    #[test]
    fn save_detects_conflicts_and_reload_resyncs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").unwrap();
        let mut store = DocumentStore::new();
        let uri = store.open(&path).unwrap().0.uri.clone();

        store.change(&uri, 1, &[edit("mine ", (0, 0))]).unwrap();
        std::fs::write(&path, "theirs").unwrap();
        bump_mtime(&path);
        assert_eq!(store.save(&uri, false).unwrap(), SaveOutcome::Conflict);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs", "nothing written on conflict");

        // Reload: read, replace the buffer, accept.
        let read = store.read_from_disk(&uri).unwrap();
        assert_eq!(read.text, "theirs");
        let full = TextDocumentContentChangeEvent { range: None, range_length: None, text: read.text };
        store.change(&uri, 2, &[full]).unwrap();
        store.accept_disk_read(&uri, read.read_id).unwrap();
        store.change(&uri, 3, &[edit("x", (0, 0))]).unwrap();
        assert_eq!(store.save(&uri, false).unwrap(), SaveOutcome::Saved);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xtheirs");

        // Overwrite wins over a conflicting change; a deleted file is recreated.
        std::fs::write(&path, "other").unwrap();
        bump_mtime(&path);
        assert_eq!(store.save(&uri, true).unwrap(), SaveOutcome::Saved);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xtheirs");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(store.save(&uri, false).unwrap(), SaveOutcome::Saved);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xtheirs");
    }

    #[test]
    fn stale_disk_reads_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").unwrap();
        let mut store = DocumentStore::new();
        let uri = store.open(&path).unwrap().0.uri.clone();
        let first = store.read_from_disk(&uri).unwrap();
        std::fs::write(&path, "two").unwrap();
        bump_mtime(&path);
        let _second = store.read_from_disk(&uri).unwrap();
        store.accept_disk_read(&uri, first.read_id).unwrap();
        // Still synced with "one": the file now differs.
        let (_, p, known) = store.all_disk_states().pop().unwrap();
        let now = DiskStamp::read(&p, Some(&known)).unwrap();
        assert!(store.observe_disk(&uri, now).unwrap().content_changed);
    }
}
