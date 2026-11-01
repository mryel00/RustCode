//! Directory listing for the explorer.

use crate::glob::{self, evaluate_tree_entry, ParsedExclude};
use serde::Serialize;
use std::{io, path::Path};

/// Names in VS Code's default `files.exclude` (`**/.git` and so on): hidden
/// from the explorer and skipped by search. Settings can change the list.
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
///
/// Entries of folder `root` that `files_exclude` hides are left out, like
/// VS Code's explorer: an entry is hidden when it or its folder is excluded
/// and no exception covers it, but an excluded folder that an exception
/// could match beneath is still listed.
pub fn list_dir(dir: &Path, root: &Path, files_exclude: &ParsedExclude) -> io::Result<Vec<DirEntry>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let is_dir = std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        found.push(DirEntry { name, path: path.to_string_lossy().into_owned(), is_dir });
    }
    let mut entries = Vec::with_capacity(found.len());
    if let Ok(rel_dir) = dir.strip_prefix(root) {
        let rel_dir = rel_dir.to_string_lossy().replace('\\', "/");
        let parent_excluded = folder_excluded(root, &rel_dir, files_exclude);
        let has_sibling = |name: &str| found.iter().any(|e| same_name(&e.name, name));
        for entry in &found {
            let rel = if rel_dir.is_empty() { entry.name.clone() } else { format!("{rel_dir}/{}", entry.name) };
            if evaluate_tree_entry(files_exclude, &rel, entry.is_dir, parent_excluded, &has_sibling).visited {
                entries.push(entry.clone());
            }
        }
    } else {
        entries = found;
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

/// Whether the folder at `rel_dir` (relative to `root`) is excluded, i.e.
/// listed only because it leads to an exception.
fn folder_excluded(root: &Path, rel_dir: &str, files_exclude: &ParsedExclude) -> bool {
    let mut excluded = false;
    let mut parent = root.to_path_buf();
    let mut rel = String::new();
    for name in rel_dir.split('/').filter(|s| !s.is_empty()) {
        if !rel.is_empty() {
            rel.push('/');
        }
        rel.push_str(name);
        let dir = parent.clone();
        excluded = evaluate_tree_entry(files_exclude, &rel, true, excluded, &|sibling| dir.join(sibling).exists()).excluded;
        parent.push(name);
    }
    excluded
}

fn same_name(a: &str, b: &str) -> bool {
    if glob::CASE_INSENSITIVE { a.eq_ignore_ascii_case(b) } else { a == b }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;

    #[test]
    fn folders_first_and_excludes_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("A.txt"), "").unwrap();
        let excludes = Settings::default().files_exclude.parse().unwrap();
        let names: Vec<_> = list_dir(dir.path(), dir.path(), &excludes).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["src", "A.txt", "b.txt"]);
    }

    #[test]
    fn files_exclude_settings_with_exceptions_and_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for file in ["build/keep/a.txt", "build/other/b.txt", "build/c.txt", "src/x.ts", "src/x.js", "src/y.js", ".git/HEAD"] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let settings = Settings::parse(
            r#"{ "files.exclude": { "**/.git": false, "**/build": true, "!build/keep": true,
                 "**/*.js": { "when": "$(basename).ts" } } }"#,
        )
        .unwrap();
        let excludes = settings.files_exclude.parse().unwrap();
        let names = |dir: &str| -> Vec<String> {
            list_dir(&root.join(dir), root, &excludes).unwrap().into_iter().map(|e| e.name).collect()
        };
        // `build` is excluded but leads to an exception, so it is listed with only that path in it.
        assert_eq!(names(""), [".git", "build", "src"]);
        assert_eq!(names("build"), ["keep"]);
        assert_eq!(names("build/keep"), ["a.txt"]);
        // x.js has a sibling x.ts and is hidden; y.js is not.
        assert_eq!(names("src"), ["x.ts", "y.js"]);
    }
}
