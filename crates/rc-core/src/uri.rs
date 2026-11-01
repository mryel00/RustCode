//! File path <-> `file://` URI conversion.

use std::path::{Path, PathBuf};
use url::Url;

/// Canonicalizes `path` (resolving symlinks and `..`) without the `\\?\` prefix on Windows.
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    dunce::canonicalize(path)
}

/// Converts an absolute path to a `file://` URI.
pub fn path_to_uri(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

/// Converts a `file://` URI to a path. Returns `None` for other schemes.
pub fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    if uri.scheme() != "file" {
        return None;
    }
    uri.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_paths_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("my file #1.rs");
        std::fs::write(&file, "").unwrap();
        let path = canonicalize(&file).unwrap();
        let uri = path_to_uri(&path).unwrap();
        assert!(uri.as_str().contains("my%20file%20%231.rs"));
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn rejects_non_file_uris() {
        assert!(uri_to_path(&Url::parse("https://example.com/a").unwrap()).is_none());
    }
}
