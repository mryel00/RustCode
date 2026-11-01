//! Directory listing for the explorer.

use serde::Serialize;
use std::{io, path::Path};

/// Names in VS Code's default `files.exclude` (`**/.git` and so on): hidden
/// from the explorer and skipped by search.
pub const FILES_EXCLUDE: &[&str] = &[".git", ".svn", ".hg", ".jj", ".DS_Store", "Thumbs.db"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// Lists one directory level: folders first, then files, each sorted
/// case-insensitively. Symlinks are followed; broken ones show as files.
pub fn list_dir(dir: &Path) -> io::Result<Vec<DirEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if FILES_EXCLUDE.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        let is_dir = std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        entries.push(DirEntry { name, path: path.to_string_lossy().into_owned(), is_dir });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_first_and_excludes_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("A.txt"), "").unwrap();
        let names: Vec<_> = list_dir(dir.path()).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["src", "A.txt", "b.txt"]);
    }
}
