//! The Tauri shell: exposes rc-core and rc-lsp to the web UI as commands and
//! forwards language server events to it.
//!
//! The command/event surface is the editor's internal API. It is kept to plain
//! JSON (LSP shapes where possible) so a future extension host can sit on the
//! same boundary as the UI.

use lsp_types::{FileChangeType, FileEvent, TextDocumentContentChangeEvent};
use rc_core::{
    glob::{ExcludeExpression, ParsedExclude},
    settings::{settings_path, WorkspaceSettings},
    watcher::DEFAULT_EXCLUDES,
    DirEntry, DiskChange, DiskRead, DiskStamp, DocumentStore, FileChange, FileChangeKind, FileMatches, FileWatcher,
    SaveOutcome, TextSearch, TextSearchQuery, WatchEvent,
};
use rc_lsp::{LspManager, ManagerEvent};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager, RunEvent, Runtime, State};
use url::Url;

/// LSP requests the UI may send. Anything else is rejected.
const ALLOWED_LSP_METHODS: &[&str] = &[
    "textDocument/completion",
    "completionItem/resolve",
    "textDocument/hover",
    "textDocument/definition",
    "textDocument/signatureHelp",
];

pub const LSP_EVENT: &str = "lsp-event";
pub const FS_EVENT: &str = "fs-event";
pub const SEARCH_EVENT: &str = "search-event";

/// How long search results are collected before they are sent to the UI.
const SEARCH_BATCH_INTERVAL: Duration = Duration::from_millis(50);
const SEARCH_BATCH_FILES: usize = 200;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct AppState {
    session: Mutex<Option<Session>>,
}

/// Everything tied to the open folder.
struct Session {
    /// Distinguishes this session from later ones in background callbacks.
    id: u64,
    root: PathBuf,
    docs: DocumentStore,
    lsp: Arc<LspManager>,
    /// Set once the recursive watch is established; dropping it stops watching.
    watcher: Option<FileWatcher>,
    /// The running text search, if any. Starting another one cancels it.
    search: Option<ActiveSearch>,
    /// From `.vscode/settings.json`; reloaded when that file changes.
    settings: Arc<Settings>,
}

/// Workspace settings, prepared for the explorer and search.
struct Settings {
    files_exclude: ParsedExclude,
    search_excludes: ExcludeExpression,
    warnings: Vec<String>,
}

impl Settings {
    fn load(root: &std::path::Path) -> Self {
        let settings = WorkspaceSettings::load(root);
        // Settings patterns are validated while loading, so parsing succeeds.
        let files_exclude = settings
            .files_exclude
            .parse()
            .or_else(|_| WorkspaceSettings::default().files_exclude.parse())
            .expect("default excludes are valid");
        Self { files_exclude, search_excludes: settings.search_excludes(), warnings: settings.warnings }
    }
}

struct ActiveSearch {
    id: u64,
    cancel: Arc<AtomicBool>,
}

/// Text search progress for the UI. `search_id` is the id the UI chose.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
enum SearchEvent {
    Results { search_id: u64, files: Vec<FileMatches> },
    Done { search_id: u64, limit_hit: bool, cancelled: bool },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchStarted {
    /// Open documents the query covers. The disk search skips every open
    /// document; the UI searches these in their editors, like VS Code.
    open_documents: Vec<String>,
}

/// File system notifications for the UI.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
enum FsEvent {
    /// Files changed in the folder. `documents` lists open documents whose
    /// relation to their file changed (deleted, restored or modified).
    Changes { changes: Vec<FsChange>, documents: Vec<DiskChange> },
    /// Events were lost: reload everything that is shown.
    Rescan { documents: Vec<DiskChange> },
    /// Watching failed; external changes may go unnoticed.
    Error { message: String },
    /// `.vscode/settings.json` changed and was loaded again: the explorer
    /// shows what `files.exclude` now hides.
    SettingsChanged { warnings: Vec<String> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FsChange {
    path: String,
    kind: FileChangeKind,
}

type CmdResult<T> = Result<T, String>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceInfo {
    root: String,
    name: String,
    /// Problems with `.vscode/settings.json`.
    settings_warnings: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenedDocument {
    uri: String,
    path: String,
    language_id: String,
    text: String,
    version: i32,
}

fn with_session<T>(state: &AppState, f: impl FnOnce(&mut Session) -> CmdResult<T>) -> CmdResult<T> {
    let mut guard = state.session.lock().unwrap();
    let session = guard.as_mut().ok_or("no folder is open")?;
    f(session)
}

fn parse_uri(uri: &str) -> CmdResult<Url> {
    Url::parse(uri).map_err(|e| format!("invalid URI {uri}: {e}"))
}

/// A folder passed on the command line (`rustcode path/to/project`).
#[tauri::command]
fn initial_path() -> Option<String> {
    std::env::args().skip(1).find(|a| !a.starts_with('-'))
}

#[tauri::command]
async fn open_workspace<R: Runtime>(app: AppHandle<R>, state: State<'_, AppState>, path: String) -> CmdResult<WorkspaceInfo> {
    let root = rc_core::uri::canonicalize(PathBuf::from(&path).as_path()).map_err(|e| format!("{path}: {e}"))?;
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let root_uri = rc_core::uri::path_to_uri(&root).ok_or("folder path cannot be expressed as a URI")?;

    let previous = state.session.lock().unwrap().take();
    if let Some(previous) = previous {
        if let Some(search) = &previous.search {
            search.cancel.store(true, Ordering::Relaxed);
        }
        previous.lsp.shutdown().await;
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ManagerEvent>();
    let lsp = Arc::new(LspManager::new(&root, root_uri, rc_lsp::default_servers(), tx));
    let events_app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let ManagerEvent::Log { server, level, message } = &event {
                log::debug!("[{server}] {level}: {message}");
            }
            let _ = events_app.emit(LSP_EVENT, &event);
        }
    });

    let settings = Arc::new(Settings::load(&root));
    let info = WorkspaceInfo {
        root: root.to_string_lossy().into_owned(),
        name: root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string()),
        settings_warnings: settings.warnings.clone(),
    };
    let id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    *state.session.lock().unwrap() = Some(Session {
        id,
        root: root.clone(),
        docs: DocumentStore::new(),
        lsp,
        watcher: None,
        search: None,
        settings,
    });
    start_watcher(app, id, root);
    Ok(info)
}

/// Starts watching the folder in the background: a recursive watch can take a
/// while to set up on large folders.
fn start_watcher<R: Runtime>(app: AppHandle<R>, session_id: u64, root: PathBuf) {
    tauri::async_runtime::spawn_blocking(move || {
        let handler_app = app.clone();
        let result = FileWatcher::start(&root, DEFAULT_EXCLUDES, move |event| {
            handle_watch_event(&handler_app, session_id, event);
        });
        match result {
            Ok(watcher) => {
                let state = app.state::<AppState>();
                let mut guard = state.session.lock().unwrap();
                if let Some(session) = guard.as_mut().filter(|s| s.id == session_id) {
                    session.watcher = Some(watcher);
                }
            }
            Err(e) => {
                log::warn!("{e}");
                let _ = app.emit(FS_EVENT, &FsEvent::Error { message: e.to_string() });
            }
        }
    });
}

/// Runs on the watcher thread.
fn handle_watch_event<R: Runtime>(app: &AppHandle<R>, session_id: u64, event: WatchEvent) {
    let state = app.state::<AppState>();
    let mut settings_changed = false;
    let event = match event {
        WatchEvent::Error(message) => {
            log::warn!("file watcher: {message}");
            FsEvent::Error { message }
        }
        WatchEvent::Rescan => {
            settings_changed = true;
            FsEvent::Rescan { documents: check_documents(&state, session_id, None) }
        }
        WatchEvent::Changes(changes) => {
            let (lsp, settings_file) = {
                let guard = state.session.lock().unwrap();
                match guard.as_ref().filter(|s| s.id == session_id) {
                    Some(session) => (Arc::clone(&session.lsp), settings_path(&session.root)),
                    None => return,
                }
            };
            // The file itself, or `.vscode` as a whole, changed.
            settings_changed = changes.iter().any(|c| settings_file.starts_with(&c.path));
            lsp.files_changed(changes.iter().filter_map(lsp_file_event).collect());
            let documents = check_documents(&state, session_id, Some(&changes));
            let changes = changes
                .into_iter()
                .map(|c| FsChange { path: c.path.to_string_lossy().into_owned(), kind: c.kind })
                .collect();
            FsEvent::Changes { changes, documents }
        }
    };
    let _ = app.emit(FS_EVENT, &event);
    if settings_changed {
        reload_settings(app, session_id);
    }
}

/// Loads `.vscode/settings.json` again and tells the UI.
fn reload_settings<R: Runtime>(app: &AppHandle<R>, session_id: u64) {
    let state = app.state::<AppState>();
    let Some(root) = state.session.lock().unwrap().as_ref().filter(|s| s.id == session_id).map(|s| s.root.clone())
    else {
        return;
    };
    let settings = Arc::new(Settings::load(&root));
    let warnings = settings.warnings.clone();
    {
        let mut guard = state.session.lock().unwrap();
        let Some(session) = guard.as_mut().filter(|s| s.id == session_id) else { return };
        session.settings = settings;
    }
    for warning in &warnings {
        log::warn!("{warning}");
    }
    let _ = app.emit(FS_EVENT, &FsEvent::SettingsChanged { warnings });
}

fn lsp_file_event(change: &FileChange) -> Option<FileEvent> {
    let typ = match change.kind {
        FileChangeKind::Created => FileChangeType::CREATED,
        FileChangeKind::Changed => FileChangeType::CHANGED,
        FileChangeKind::Deleted => FileChangeType::DELETED,
    };
    Some(FileEvent { uri: rc_core::uri::path_to_uri(&change.path)?, typ })
}

/// Compares open documents affected by `changes` (all of them for `None`)
/// with the disk. Files are read without holding the session lock.
fn check_documents(state: &AppState, session_id: u64, changes: Option<&[FileChange]>) -> Vec<DiskChange> {
    let candidates = {
        let guard = state.session.lock().unwrap();
        let Some(session) = guard.as_ref().filter(|s| s.id == session_id) else { return Vec::new() };
        match changes {
            None => session.docs.all_disk_states(),
            Some(changes) => {
                let mut seen = HashSet::new();
                changes
                    .iter()
                    .flat_map(|c| session.docs.affected_by(&c.path))
                    .filter(|(uri, ..)| seen.insert(uri.clone()))
                    .collect()
            }
        }
    };
    if candidates.is_empty() {
        return Vec::new();
    }
    let observed: Vec<_> = candidates
        .into_iter()
        .filter_map(|(uri, path, known)| match DiskStamp::read(&path, Some(&known)) {
            Ok(stamp) => Some((uri, stamp)),
            Err(e) => {
                log::warn!("cannot check {}: {e}", path.display());
                None
            }
        })
        .collect();
    let mut guard = state.session.lock().unwrap();
    let Some(session) = guard.as_mut().filter(|s| s.id == session_id) else { return Vec::new() };
    observed.into_iter().filter_map(|(uri, stamp)| session.docs.observe_disk(&uri, stamp)).collect()
}

#[tauri::command]
async fn list_dir(state: State<'_, AppState>, path: Option<String>) -> CmdResult<Vec<DirEntry>> {
    let (root, settings) = with_session(&state, |s| Ok((s.root.clone(), Arc::clone(&s.settings))))?;
    let dir = path.map(PathBuf::from).unwrap_or_else(|| root.clone());
    rc_core::list_dir(&dir, &root, &settings.files_exclude).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Opens a document by path or `file://` URI. Opening an already-open document
/// returns its current state.
#[tauri::command]
async fn open_document(state: State<'_, AppState>, path: Option<String>, uri: Option<String>) -> CmdResult<OpenedDocument> {
    let path = match (path, uri) {
        (Some(path), _) => PathBuf::from(path),
        (None, Some(uri)) => rc_core::uri::uri_to_path(&parse_uri(&uri)?).ok_or_else(|| format!("{uri} is not a local file"))?,
        (None, None) => return Err("open_document needs a path or uri".into()),
    };
    with_session(&state, |session| {
        let (doc, newly_opened) = session.docs.open(&path).map_err(|e| e.to_string())?;
        let opened = OpenedDocument {
            uri: doc.uri.to_string(),
            path: doc.path.to_string_lossy().into_owned(),
            language_id: doc.language_id.clone(),
            text: doc.text.to_string(),
            version: doc.version,
        };
        if newly_opened {
            session.lsp.open(doc.uri.clone(), &doc.language_id, doc.version, opened.text.clone());
        }
        Ok(opened)
    })
}

#[tauri::command]
async fn change_document(
    state: State<'_, AppState>,
    uri: String,
    version: i32,
    changes: Vec<TextDocumentContentChangeEvent>,
) -> CmdResult<()> {
    let uri = parse_uri(&uri)?;
    with_session(&state, |session| {
        session.docs.change(&uri, version, &changes).map_err(|e| e.to_string())?;
        session.lsp.change(&uri, version, changes);
        Ok(())
    })
}

/// Saves a document. Returns `conflict` without writing when the file changed
/// on disk since the document last synced with it, unless `overwrite` is set.
#[tauri::command]
async fn save_document(state: State<'_, AppState>, uri: String, overwrite: Option<bool>) -> CmdResult<SaveOutcome> {
    let uri = parse_uri(&uri)?;
    with_session(&state, |session| {
        let outcome = session.docs.save(&uri, overwrite.unwrap_or(false)).map_err(|e| e.to_string())?;
        if outcome == SaveOutcome::Saved {
            session.lsp.save(&uri);
        }
        Ok(outcome)
    })
}

/// Reads a document's file for reverting or reloading it. The UI applies the
/// text as ordinary edits, then calls `accept_disk_read`.
#[tauri::command]
async fn read_document_from_disk(state: State<'_, AppState>, uri: String) -> CmdResult<DiskRead> {
    let uri = parse_uri(&uri)?;
    with_session(&state, |session| session.docs.read_from_disk(&uri).map_err(|e| e.to_string()))
}

#[tauri::command]
async fn accept_disk_read(state: State<'_, AppState>, uri: String, read_id: u64) -> CmdResult<()> {
    let uri = parse_uri(&uri)?;
    with_session(&state, |session| session.docs.accept_disk_read(&uri, read_id).map_err(|e| e.to_string()))
}

#[tauri::command]
async fn close_document(state: State<'_, AppState>, uri: String) -> CmdResult<()> {
    let uri = parse_uri(&uri)?;
    with_session(&state, |session| {
        session.docs.close(&uri);
        session.lsp.close(&uri);
        Ok(())
    })
}

/// Forwards a request to the language server for `uri`. Returns `null` when no
/// server handles the document or the request was cancelled.
#[tauri::command]
async fn lsp_request(state: State<'_, AppState>, uri: String, method: String, params: Value) -> CmdResult<Value> {
    if !ALLOWED_LSP_METHODS.contains(&method.as_str()) {
        return Err(format!("LSP method {method} is not allowed"));
    }
    let uri = parse_uri(&uri)?;
    let lsp = with_session(&state, |s| Ok(Arc::clone(&s.lsp)))?;
    match lsp.request(&uri, &method, params).await {
        Ok(value) => Ok(value),
        Err(e) if e.is_cancellation() => Ok(Value::Null),
        Err(e) => Err(e.to_string()),
    }
}

/// Starts searching the folder for `query`. Results arrive as `search-event`s
/// tagged with `search_id`; a running search is cancelled.
#[tauri::command]
async fn search_text<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    search_id: u64,
    query: TextSearchQuery,
) -> CmdResult<SearchStarted> {
    let (search, skip, open_documents, cancel, session_id) = with_session(&state, |session| {
        let search =
            TextSearch::new(&session.root, &query, &session.settings.search_excludes).map_err(|e| e.to_string())?;
        let mut skip = HashSet::new();
        let mut open_documents = Vec::new();
        for doc in session.docs.documents() {
            skip.insert(doc.path.clone());
            if search.in_scope(&doc.path) {
                open_documents.push(doc.uri.to_string());
            }
        }
        if let Some(previous) = session.search.take() {
            previous.cancel.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        session.search = Some(ActiveSearch { id: search_id, cancel: Arc::clone(&cancel) });
        Ok((search, skip, open_documents, cancel, session.id))
    })?;
    tauri::async_runtime::spawn_blocking(move || {
        run_search(&app, session_id, search_id, &search, &skip, &cancel);
    });
    Ok(SearchStarted { open_documents })
}

/// Runs a search on the current thread, sending results in batches.
fn run_search<R: Runtime>(
    app: &AppHandle<R>,
    session_id: u64,
    search_id: u64,
    search: &TextSearch,
    skip: &HashSet<PathBuf>,
    cancel: &AtomicBool,
) {
    let (tx, rx) = mpsc::channel::<FileMatches>();
    let forward_app = app.clone();
    let forwarder = std::thread::spawn(move || {
        let mut batch = Vec::new();
        let mut last = Instant::now();
        loop {
            let done = match rx.recv_timeout(SEARCH_BATCH_INTERVAL.saturating_sub(last.elapsed())) {
                Ok(file) => {
                    batch.push(file);
                    false
                }
                Err(mpsc::RecvTimeoutError::Timeout) => false,
                Err(mpsc::RecvTimeoutError::Disconnected) => true,
            };
            if batch.is_empty() {
                last = Instant::now();
            } else if done || batch.len() >= SEARCH_BATCH_FILES || last.elapsed() >= SEARCH_BATCH_INTERVAL {
                let files = std::mem::take(&mut batch);
                let _ = forward_app.emit(SEARCH_EVENT, &SearchEvent::Results { search_id, files });
                last = Instant::now();
            }
            if done {
                break;
            }
        }
    });
    let complete = search.run(skip, cancel, |file| {
        let _ = tx.send(file);
    });
    drop(tx);
    let _ = forwarder.join();
    let cancelled = cancel.load(Ordering::Relaxed);
    let _ = app.emit(SEARCH_EVENT, &SearchEvent::Done { search_id, limit_hit: complete.limit_hit, cancelled });

    let state = app.state::<AppState>();
    let mut guard = state.session.lock().unwrap();
    if let Some(session) = guard.as_mut().filter(|s| s.id == session_id)
        && session.search.as_ref().is_some_and(|s| s.id == search_id)
    {
        session.search = None;
    }
}

#[tauri::command]
async fn cancel_search(state: State<'_, AppState>, search_id: u64) -> CmdResult<()> {
    with_session(&state, |session| {
        if session.search.as_ref().is_some_and(|s| s.id == search_id) {
            let search = session.search.take().expect("checked above");
            search.cancel.store(true, Ordering::Relaxed);
        }
        Ok(())
    })
}

/// Registers RustCode's state and commands on a builder (shared with tests).
fn setup<R: Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.manage(AppState::default()).invoke_handler(tauri::generate_handler![
        initial_path,
        open_workspace,
        list_dir,
        open_document,
        change_document,
        save_document,
        read_document_from_disk,
        accept_disk_read,
        close_document,
        lsp_request,
        search_text,
        cancel_search,
    ])
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let app = setup(tauri::Builder::default().plugin(tauri_plugin_dialog::init()))
        .build(tauri::generate_context!())
        .expect("failed to build the RustCode app");

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            // Shut language servers down cleanly so no processes are left behind.
            let session = handle.state::<AppState>().session.lock().unwrap().take();
            if let Some(session) = session {
                tauri::async_runtime::block_on(session.lsp.shutdown());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    //! Drives the real commands through Tauri's mock runtime.

    use super::*;
    use serde_json::json;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };
    use tauri::{
        ipc::{CallbackFn, InvokeBody, InvokeResponseBody},
        test::{get_ipc_response, mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY},
        webview::InvokeRequest,
        Listener, WebviewWindow,
    };

    fn invoke(webview: &WebviewWindow<MockRuntime>, cmd: &str, args: Value) -> Result<Value, Value> {
        let request = InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: if cfg!(windows) { "http://tauri.localhost" } else { "tauri://localhost" }.parse().unwrap(),
            body: InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        };
        get_ipc_response(webview, request).map(|body| match body {
            InvokeResponseBody::Json(s) => serde_json::from_str(&s).unwrap(),
            InvokeResponseBody::Raw(_) => panic!("unexpected raw response"),
        })
    }

    /// Waits for an `fs-event` matching `pred`, running `poke` every 500ms
    /// meanwhile (the watcher starts in the background, so early writes can be
    /// missed).
    fn wait_event(rx: &mpsc::Receiver<Value>, what: &str, poke: &dyn Fn(), pred: &dyn Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        poke();
        let mut seen = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(event) if pred(&event) => return event,
                Ok(event) => seen.push(event),
                Err(_) => poke(),
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}; saw {seen:#?}");
        }
    }

    #[test]
    fn external_changes_reach_documents_and_conflicts_are_detected() {
        let dir = tempfile::tempdir().unwrap();
        let root = rc_core::uri::canonicalize(dir.path()).unwrap();
        let file = root.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();

        let app = setup(mock_builder()).build(mock_context(noop_assets())).unwrap();
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
        let (tx, rx) = mpsc::channel::<Value>();
        app.listen_any(FS_EVENT, move |e| {
            let _ = tx.send(serde_json::from_str(e.payload()).unwrap());
        });

        invoke(&webview, "open_workspace", json!({"path": root})).unwrap();
        let opened = invoke(&webview, "open_document", json!({"path": file})).unwrap();
        let uri = opened["uri"].as_str().unwrap().to_owned();
        assert_eq!(opened["text"], "one\n");

        // A local edit, then an external change: the document is told its file
        // changed, and saving reports a conflict instead of clobbering it.
        let edit = json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "text": "mine "});
        invoke(&webview, "change_document", json!({"uri": uri, "version": 1, "changes": [edit]})).unwrap();
        let event = wait_event(
            &rx,
            "content change",
            &|| std::fs::write(&file, "theirs\n").unwrap(),
            &|e| e["kind"] == "changes" && e["documents"] == json!([{"uri": uri, "exists": true, "contentChanged": true}]),
        );
        assert!(event["changes"].as_array().unwrap().iter().any(|c| c["path"] == json!(file) && c["kind"] == "changed"));
        assert_eq!(invoke(&webview, "save_document", json!({"uri": uri})).unwrap(), "conflict");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");

        // Revert: read, replace the text, accept. Then saving works again.
        let read = invoke(&webview, "read_document_from_disk", json!({"uri": uri})).unwrap();
        assert_eq!(read["text"], "theirs\n");
        let full = json!({"text": read["text"]});
        invoke(&webview, "change_document", json!({"uri": uri, "version": 2, "changes": [full]})).unwrap();
        invoke(&webview, "accept_disk_read", json!({"uri": uri, "readId": read["readId"]})).unwrap();
        assert_eq!(invoke(&webview, "save_document", json!({"uri": uri})).unwrap(), "saved");

        // Our own save is not reported as an external change; a deletion is.
        let event = wait_event(&rx, "deletion", &|| {
            let _ = std::fs::remove_file(&file);
        }, &|e| e["kind"] == "changes" && e["documents"].as_array().is_some_and(|d| !d.is_empty()));
        assert_eq!(event["documents"], json!([{"uri": uri, "exists": false, "contentChanged": false}]));

        // New files show up for the explorer.
        let other = root.join("b.txt");
        wait_event(&rx, "creation", &|| std::fs::write(&other, "b").unwrap(), &|e| {
            e["kind"] == "changes" && e["changes"].as_array().unwrap().iter().any(|c| c["path"] == json!(other))
        });

        // Saving recreates the deleted file without a conflict.
        assert_eq!(invoke(&webview, "save_document", json!({"uri": uri})).unwrap(), "saved");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");
    }

    #[test]
    fn search_streams_disk_results_and_leaves_open_documents_to_the_ui() {
        let dir = tempfile::tempdir().unwrap();
        let root = rc_core::uri::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("open.txt"), "needle\n").unwrap();
        std::fs::write(root.join("closed.txt"), "a\nneedle needle\n").unwrap();
        std::fs::write(root.join("other.txt"), "nothing").unwrap();

        let app = setup(mock_builder()).build(mock_context(noop_assets())).unwrap();
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
        let (tx, rx) = mpsc::channel::<Value>();
        app.listen_any(SEARCH_EVENT, move |e| {
            let _ = tx.send(serde_json::from_str(e.payload()).unwrap());
        });

        invoke(&webview, "open_workspace", json!({"path": root})).unwrap();
        let opened = invoke(&webview, "open_document", json!({"path": root.join("open.txt")})).unwrap();

        let started = invoke(&webview, "search_text", json!({"searchId": 7, "query": {"pattern": "NEEDLE"}})).unwrap();
        assert_eq!(started["openDocuments"], json!([opened["uri"]]));

        let mut files = Vec::new();
        let done = loop {
            let event = rx.recv_timeout(Duration::from_secs(10)).expect("search finished");
            assert_eq!(event["searchId"], 7);
            match event["kind"].as_str().unwrap() {
                "results" => files.extend(event["files"].as_array().unwrap().iter().cloned()),
                "done" => break event,
                other => panic!("unexpected event {other}"),
            }
        };
        assert_eq!(done["limitHit"], false);
        assert_eq!(done["cancelled"], false);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], json!(root.join("closed.txt")));
        assert_eq!(
            files[0]["matches"][1],
            json!({
                "range": {"start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 13}},
                "preview": {"text": "needle needle", "start": 7, "end": 13}
            })
        );

        let err = invoke(&webview, "search_text", json!({"searchId": 8, "query": {"pattern": "(", "isRegex": true}}));
        assert!(err.unwrap_err().as_str().unwrap().contains("unclosed group"));
        invoke(&webview, "cancel_search", json!({"searchId": 7})).unwrap();
    }

    #[test]
    fn workspace_settings_drive_explorer_and_search_and_reload_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = rc_core::uri::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join(".vscode")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/keep")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/other")).unwrap();
        std::fs::write(root.join("node_modules/keep/a.txt"), "needle").unwrap();
        std::fs::write(root.join("node_modules/other/b.txt"), "needle").unwrap();
        std::fs::write(root.join("secret.txt"), "needle").unwrap();
        let settings = root.join(".vscode/settings.json");
        std::fs::write(
            &settings,
            r#"{ "files.exclude": { "secret.txt": true, "bad[": true }, // comment
                 "search.exclude": { "!node_modules/keep": true }, }"#,
        )
        .unwrap();

        let app = setup(mock_builder()).build(mock_context(noop_assets())).unwrap();
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
        let (fs_tx, fs_rx) = mpsc::channel::<Value>();
        app.listen_any(FS_EVENT, move |e| {
            let _ = fs_tx.send(serde_json::from_str(e.payload()).unwrap());
        });
        let (search_tx, search_rx) = mpsc::channel::<Value>();
        app.listen_any(SEARCH_EVENT, move |e| {
            let _ = search_tx.send(serde_json::from_str(e.payload()).unwrap());
        });

        let info = invoke(&webview, "open_workspace", json!({"path": root})).unwrap();
        assert_eq!(info["settingsWarnings"].as_array().unwrap().len(), 1);
        let names = |webview: &WebviewWindow<MockRuntime>| -> Vec<String> {
            let entries = invoke(webview, "list_dir", json!({})).unwrap();
            entries.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_owned()).collect()
        };
        assert_eq!(names(&webview), [".vscode", "node_modules"]);

        // The exception re-includes node_modules/keep for search.
        invoke(&webview, "search_text", json!({"searchId": 1, "query": {"pattern": "needle"}})).unwrap();
        let mut found = Vec::new();
        loop {
            let event = search_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if event["kind"] == "done" {
                break;
            }
            for file in event["files"].as_array().unwrap() {
                found.push(file["path"].as_str().unwrap().to_owned());
            }
        }
        assert_eq!(found, [root.join("node_modules/keep/a.txt").to_string_lossy()]);

        // Editing the settings file reloads it.
        wait_event(
            &fs_rx,
            "settings reload",
            &|| std::fs::write(&settings, r#"{ "files.exclude": { "**/node_modules": true } }"#).unwrap(),
            &|e| e["kind"] == "settingsChanged",
        );
        assert_eq!(names(&webview), [".vscode", "secret.txt"]);
    }
}
