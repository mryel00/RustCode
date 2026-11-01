//! Recursive file watching for the open folder.
//!
//! Mirrors VS Code's watcher: raw events are collected for a short delay,
//! coalesced (`coalesce`, a port of VS Code's `EventCoalescer`) and filtered by
//! the `files.watcherExclude` patterns before being handed out.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use notify::{
    event::{EventKind, ModifyKind, RenameMode},
    RecommendedWatcher, RecursiveMode, Watcher as _,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

/// VS Code's default `files.watcherExclude`. Patterns are matched relative to
/// the watched folder, absolute ones against the absolute path.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".git/objects/**",
    ".git/subtree-cache/**",
    ".hg/store/**",
    "*/.git/objects/**",
    "*/.git/subtree-cache/**",
    "*/.hg/store/**",
];

/// How long raw events are collected before they are coalesced and delivered
/// (VS Code's `FILE_CHANGES_HANDLER_DELAY`).
const COALESCE_DELAY: Duration = Duration::from_millis(75);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileChangeKind {
    Created,
    Changed,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: FileChangeKind,
}

impl FileChange {
    pub fn new(path: impl Into<PathBuf>, kind: FileChangeKind) -> Self {
        Self { path: path.into(), kind }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// Coalesced changes, deletes first (see [`coalesce`]).
    Changes(Vec<FileChange>),
    /// Events were lost (for example a kernel queue overflow): anything under
    /// the folder may have changed.
    Rescan,
    /// The watcher failed; changes may go unnoticed.
    Error(String),
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("invalid watcher exclude pattern: {0}")]
    Pattern(#[from] globset::Error),
    #[error("unable to watch for file changes in {path}: {message}")]
    Watch { path: PathBuf, message: String },
}

/// Watches a folder recursively until dropped.
pub struct FileWatcher {
    // Dropping the notify watcher drops the only event sender, which ends the
    // delivery thread.
    _watcher: RecommendedWatcher,
}

impl FileWatcher {
    /// Starts watching `root` (which should be canonical). `on_event` runs on a
    /// background thread. Setting up a recursive watch can take a while on
    /// large folders, so call this off the UI thread.
    pub fn start(
        root: &Path,
        excludes: &[&str],
        on_event: impl Fn(WatchEvent) + Send + 'static,
    ) -> Result<Self, WatchError> {
        let excludes = Excludes::new(excludes)?;
        let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .map_err(|e| watch_error(root, e))?;
        watcher.watch(root, RecursiveMode::Recursive).map_err(|e| watch_error(root, e))?;

        let root = root.to_owned();
        std::thread::Builder::new()
            .name("rc-file-watcher".into())
            .spawn(move || deliver(rx, &root, &excludes, on_event))
            .map_err(|e| WatchError::Watch { path: PathBuf::new(), message: e.to_string() })?;
        Ok(Self { _watcher: watcher })
    }
}

fn watch_error(root: &Path, e: notify::Error) -> WatchError {
    let message = match e.kind {
        notify::ErrorKind::MaxFilesWatch => {
            "the system limit for watched files was reached (on Linux, raise fs.inotify.max_user_watches)".to_owned()
        }
        _ => e.to_string(),
    };
    WatchError::Watch { path: root.to_owned(), message }
}

/// Compiles watcher globs the way VS Code matches them: `*` stays within one
/// path segment and matching ignores case except on Linux.
pub fn build_globset(patterns: &[&str]) -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .case_insensitive(!cfg!(target_os = "linux"))
                .build()?,
        );
    }
    builder.build()
}

fn deliver(
    rx: mpsc::Receiver<notify::Result<notify::Event>>,
    root: &Path,
    excludes: &Excludes,
    on_event: impl Fn(WatchEvent),
) {
    // Wait for a first event, then collect for COALESCE_DELAY and flush.
    while let Ok(first) = rx.recv() {
        let deadline = Instant::now() + COALESCE_DELAY;
        let mut batch = vec![first];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(event) => batch.push(event),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }

        let mut raw = Vec::new();
        let mut rescan = false;
        for event in batch {
            match event {
                Ok(event) => {
                    rescan |= event.need_rescan();
                    raw.extend(changes_from_event(&event));
                }
                Err(e) => on_event(WatchEvent::Error(e.to_string())),
            }
        }
        if rescan {
            on_event(WatchEvent::Rescan);
            continue;
        }
        raw.retain(|c| !is_excluded(root, &c.path, excludes));
        let changes = coalesce(raw);
        if !changes.is_empty() {
            on_event(WatchEvent::Changes(changes));
        }
    }
}

/// Watcher exclude patterns: relative ones match paths inside the folder,
/// absolute ones absolute paths.
struct Excludes {
    relative: GlobSet,
    absolute: GlobSet,
}

impl Excludes {
    fn new(patterns: &[&str]) -> Result<Self, globset::Error> {
        let (absolute, relative): (Vec<&str>, Vec<&str>) =
            patterns.iter().partition(|p| Path::new(p).is_absolute() || p.starts_with('/'));
        Ok(Self { relative: build_globset(&relative)?, absolute: build_globset(&absolute)? })
    }
}

/// Whether `path` or a folder above it (up to `root`) is excluded: like the
/// native watcher VS Code uses, an excluded folder hides everything in it.
fn is_excluded(root: &Path, path: &Path, excludes: &Excludes) -> bool {
    path.ancestors().take_while(|p| p.starts_with(root) && *p != root).any(|p| {
        excludes.absolute.is_match(p) || p.strip_prefix(root).is_ok_and(|rel| excludes.relative.is_match(rel))
    })
}

/// Watches one file until dropped, through its folder so that a file that is
/// replaced, created or deleted is noticed too. `on_change` runs on a
/// background thread, at most once per short burst of events.
pub struct FileWatch {
    _watcher: RecommendedWatcher,
}

pub fn watch_file(path: &Path, on_change: impl Fn() + Send + 'static) -> Result<FileWatch, WatchError> {
    let invalid = || WatchError::Watch { path: path.to_owned(), message: "not a file path".into() };
    let name = path.file_name().ok_or_else(invalid)?.to_owned();
    let dir = crate::uri::canonicalize(path.parent().ok_or_else(invalid)?).map_err(|e| WatchError::Watch {
        path: path.to_owned(),
        message: e.to_string(),
    })?;
    let target = dir.join(&name);
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })
    .map_err(|e| watch_error(&dir, e))?;
    watcher.watch(&dir, RecursiveMode::NonRecursive).map_err(|e| watch_error(&dir, e))?;
    std::thread::Builder::new()
        .name("rc-file-watch".into())
        .spawn(move || {
            // Reading the file (as `on_change` will) is not a change.
            let concerns = |event: &notify::Result<notify::Event>| match event {
                Ok(event) if matches!(event.kind, EventKind::Access(_)) => false,
                Ok(event) => event.need_rescan() || event.paths.contains(&target),
                Err(_) => false,
            };
            while let Ok(first) = rx.recv() {
                let mut relevant = concerns(&first);
                let deadline = Instant::now() + COALESCE_DELAY;
                loop {
                    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(event) => relevant |= concerns(&event),
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                if relevant {
                    on_change();
                }
            }
        })
        .map_err(|e| WatchError::Watch { path: path.to_owned(), message: e.to_string() })?;
    Ok(FileWatch { _watcher: watcher })
}

/// Translates one notify event into file changes.
fn changes_from_event(event: &notify::Event) -> Vec<FileChange> {
    use FileChangeKind::*;
    let exists_kind = |path: &Path| if path.exists() { Created } else { Deleted };
    let each = |kind: FileChangeKind| event.paths.iter().map(|p| FileChange::new(p, kind)).collect();
    match event.kind {
        EventKind::Create(_) => each(Created),
        EventKind::Remove(_) => each(Deleted),
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => each(Deleted),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => each(Created),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if event.paths.len() == 2 => vec![
            FileChange::new(&event.paths[0], Deleted),
            FileChange::new(&event.paths[1], Created),
        ],
        // Renames whose direction is unknown (macOS, Windows): look at the disk.
        EventKind::Modify(ModifyKind::Name(_)) => {
            event.paths.iter().map(|p| FileChange::new(p, exists_kind(p))).collect()
        }
        EventKind::Modify(_) => each(Changed),
        // Opening or closing a file does not change it.
        EventKind::Access(_) => Vec::new(),
        EventKind::Any | EventKind::Other => event
            .paths
            .iter()
            .map(|p| FileChange::new(p, if p.exists() { Changed } else { Deleted }))
            .collect(),
    }
}

/// Coalesces a batch of changes in order, like VS Code:
///
/// - created then deleted: dropped
/// - deleted then created: changed
/// - created then changed: created
/// - otherwise the latest kind wins
///
/// Deletes inside a deleted folder are dropped, and deletes come first in the
/// result.
pub fn coalesce(changes: Vec<FileChange>) -> Vec<FileChange> {
    use FileChangeKind::*;
    let key = |path: &Path| -> PathBuf {
        if cfg!(target_os = "linux") {
            path.to_owned()
        } else {
            PathBuf::from(path.to_string_lossy().to_lowercase())
        }
    };

    // Insertion-ordered list; `None` marks a dropped entry.
    let mut list: Vec<Option<FileChange>> = Vec::new();
    let mut index: HashMap<PathBuf, usize> = HashMap::new();
    for change in changes {
        let k = key(&change.path);
        let mut keep = true;
        if let Some(&i) = index.get(&k)
            && let Some(existing) = list[i].as_mut()
        {
            keep = false;
            if existing.path != change.path && matches!(change.kind, Deleted | Created) {
                // A rename that only changes case: keep both events.
                keep = true;
            } else if existing.kind == Created && change.kind == Deleted {
                list[i] = None;
                index.remove(&k);
            } else if existing.kind == Deleted && change.kind == Created {
                existing.kind = Changed;
            } else if existing.kind == Created && change.kind == Changed {
                // Keep "created".
            } else {
                existing.kind = change.kind;
            }
        }
        if keep {
            index.insert(k, list.len());
            list.push(Some(change));
        }
    }

    let (mut deletes, others): (Vec<_>, Vec<_>) = list.into_iter().flatten().partition(|c| c.kind == Deleted);
    deletes.sort_by_key(|c| c.path.as_os_str().len());
    let mut deleted_dirs: Vec<PathBuf> = Vec::new();
    deletes.retain(|c| {
        let k = key(&c.path);
        if deleted_dirs.iter().any(|d| k.starts_with(d) && k != *d) {
            return false;
        }
        deleted_dirs.push(k);
        true
    });
    deletes.extend(others);
    deletes
}

#[cfg(test)]
mod tests {
    use super::*;
    use FileChangeKind::*;

    fn c(path: &str, kind: FileChangeKind) -> FileChange {
        FileChange::new(path, kind)
    }

    #[test]
    fn coalesces_like_vscode() {
        let out = coalesce(vec![
            c("/w/a", Created),
            c("/w/a", Deleted), // dropped together with the create
            c("/w/b", Deleted),
            c("/w/b", Created), // becomes a change
            c("/w/c", Created),
            c("/w/c", Changed), // stays created
            c("/w/d", Changed),
            c("/w/d", Deleted), // becomes a delete
        ]);
        assert_eq!(out, vec![c("/w/d", Deleted), c("/w/b", Changed), c("/w/c", Created)]);
    }

    #[test]
    fn drops_deletes_inside_deleted_folders() {
        let out = coalesce(vec![
            c("/w/dir/x.rs", Deleted),
            c("/w/dir", Deleted),
            c("/w/dir2", Deleted),
            c("/w/dir/sub/y.rs", Deleted),
        ]);
        assert_eq!(out, vec![c("/w/dir", Deleted), c("/w/dir2", Deleted)]);
    }

    #[test]
    fn excluded_folders_hide_their_contents_and_absolute_patterns_work() {
        let set = Excludes::new(&["**/node_modules", "/abs/out/**", "*.log"]).unwrap();
        let root = Path::new("/w");
        assert!(is_excluded(root, Path::new("/w/a/node_modules/x/y.js"), &set));
        assert!(is_excluded(root, Path::new("/w/node_modules"), &set));
        assert!(is_excluded(root, Path::new("/w/x.log"), &set));
        assert!(!is_excluded(root, Path::new("/w/a/x.log"), &set));
        assert!(!is_excluded(root, Path::new("/w/src/main.rs"), &set));
        let abs = Excludes::new(&["/w/out/**"]).unwrap();
        assert!(is_excluded(root, Path::new("/w/out/a/b.js"), &abs));
        assert!(!is_excluded(root, Path::new("/w/outer.js"), &abs));
    }

    #[test]
    fn watches_a_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let (tx, rx) = mpsc::channel();
        let _watch = watch_file(&file, move || {
            let _ = tx.send(());
        })
        .unwrap();
        // Other files in the folder are not reported.
        std::fs::write(dir.path().join("other.json"), "x").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(400)).is_err());
        // Creating, changing and replacing the file are.
        std::fs::write(&file, "1").unwrap();
        rx.recv_timeout(Duration::from_secs(5)).expect("creation");
        let tmp = dir.path().join("settings.json.tmp");
        std::fs::write(&tmp, "2").unwrap();
        std::fs::rename(&tmp, &file).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).expect("replacement");
        // Reading it is not a change.
        while rx.recv_timeout(Duration::from_millis(300)).is_ok() {}
        std::fs::read_to_string(&file).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(400)).is_err());
    }

    #[test]
    fn default_excludes_match_git_objects_only() {
        let set = Excludes::new(DEFAULT_EXCLUDES).unwrap();
        let root = Path::new("/w");
        assert!(is_excluded(root, Path::new("/w/.git/objects/ab/cdef"), &set));
        assert!(is_excluded(root, Path::new("/w/sub/.git/objects/ab"), &set));
        assert!(!is_excluded(root, Path::new("/w/.git/HEAD"), &set));
        assert!(!is_excluded(root, Path::new("/w/a/b/.git/objects/ab"), &set));
        assert!(!is_excluded(root, Path::new("/w/src/main.rs"), &set));
    }

    /// Real file system round trip: create, modify and delete files and folders.
    #[test]
    fn reports_real_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::uri::canonicalize(dir.path()).unwrap();
        let (tx, rx) = mpsc::channel();
        let _watcher = FileWatcher::start(&root, DEFAULT_EXCLUDES, move |e| {
            let _ = tx.send(e);
        })
        .unwrap();

        // Collects changes until `pred` holds or a timeout expires.
        let wait_for = |pred: &dyn Fn(&[FileChange]) -> bool| -> Vec<FileChange> {
            let mut seen = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !pred(&seen) {
                let left = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(left) {
                    Ok(WatchEvent::Changes(changes)) => seen.extend(changes),
                    Ok(other) => panic!("unexpected watch event {other:?}"),
                    Err(_) => panic!("timed out; saw {seen:?}"),
                }
            }
            seen
        };
        let has = |changes: &[FileChange], path: &Path, kind: FileChangeKind| {
            changes.iter().any(|c| c.path == path && c.kind == kind)
        };

        let file = root.join("a.txt");
        std::fs::write(&file, "one").unwrap();
        wait_for(&|s| has(s, &file, Created));

        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(&file, "two").unwrap();
        wait_for(&|s| has(s, &file, Changed));

        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        wait_for(&|s| has(s, &sub, Created));
        // Files in a folder created after the watch started are reported too.
        std::thread::sleep(Duration::from_millis(200));
        let nested = sub.join("b.txt");
        std::fs::write(&nested, "x").unwrap();
        wait_for(&|s| has(s, &nested, Created));

        let renamed = root.join("renamed.txt");
        std::fs::rename(&file, &renamed).unwrap();
        wait_for(&|s| has(s, &file, Deleted) && has(s, &renamed, Created));

        std::fs::remove_dir_all(&sub).unwrap();
        wait_for(&|s| has(s, &sub, Deleted));
    }
}
