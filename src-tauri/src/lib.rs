//! The Tauri shell: exposes rc-core and rc-lsp to the web UI as commands and
//! forwards language server events to it.
//!
//! The command/event surface is the editor's internal API. It is kept to plain
//! JSON (LSP shapes where possible) so a future extension host can sit on the
//! same boundary as the UI.

use lsp_types::TextDocumentContentChangeEvent;
use rc_core::{DirEntry, DocumentStore};
use rc_lsp::{LspManager, ManagerEvent};
use serde::Serialize;
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};
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

#[derive(Default)]
struct AppState {
    session: Mutex<Option<Session>>,
}

/// Everything tied to the open folder.
struct Session {
    root: PathBuf,
    docs: DocumentStore,
    lsp: Arc<LspManager>,
}

type CmdResult<T> = Result<T, String>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceInfo {
    root: String,
    name: String,
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
async fn open_workspace(app: AppHandle, state: State<'_, AppState>, path: String) -> CmdResult<WorkspaceInfo> {
    let root = rc_core::uri::canonicalize(PathBuf::from(&path).as_path()).map_err(|e| format!("{path}: {e}"))?;
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let root_uri = rc_core::uri::path_to_uri(&root).ok_or("folder path cannot be expressed as a URI")?;

    let previous = state.session.lock().unwrap().take();
    if let Some(previous) = previous {
        previous.lsp.shutdown().await;
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ManagerEvent>();
    let lsp = Arc::new(LspManager::new(&root, root_uri, rc_lsp::default_servers(), tx));
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let ManagerEvent::Log { server, level, message } = &event {
                log::debug!("[{server}] {level}: {message}");
            }
            let _ = app.emit(LSP_EVENT, &event);
        }
    });

    let info = WorkspaceInfo {
        root: root.to_string_lossy().into_owned(),
        name: root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string()),
    };
    *state.session.lock().unwrap() = Some(Session { root, docs: DocumentStore::new(), lsp });
    Ok(info)
}

#[tauri::command]
async fn list_dir(state: State<'_, AppState>, path: Option<String>) -> CmdResult<Vec<DirEntry>> {
    let dir = match path {
        Some(p) => PathBuf::from(p),
        None => with_session(&state, |s| Ok(s.root.clone()))?,
    };
    rc_core::list_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))
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

#[tauri::command]
async fn save_document(state: State<'_, AppState>, uri: String) -> CmdResult<()> {
    let uri = parse_uri(&uri)?;
    with_session(&state, |session| {
        session.docs.save(&uri).map_err(|e| e.to_string())?;
        session.lsp.save(&uri);
        Ok(())
    })
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            initial_path,
            open_workspace,
            list_dir,
            open_document,
            change_document,
            save_document,
            close_document,
            lsp_request,
        ])
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
