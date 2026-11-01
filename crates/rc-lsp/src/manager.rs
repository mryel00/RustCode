//! Starts language servers on demand and keeps them in sync with open documents.
//!
//! Each server runs in its own task that owns the [`LspClient`] and a mirror of
//! every document it has open. All traffic for a server (open, change, save,
//! close and requests) goes through one ordered queue, so a completion request
//! is never sent before the edit that preceded it, even while the server is
//! still starting.
//!
//! A server that exits on its own is restarted like vscode-languageclient does
//! (up to five closes in three minutes) and gets every open document again;
//! [`LspManager::restart`] restarts one on request.

use crate::{
    client::{Incoming, LspClient, LspError},
    config::ServerConfig,
};
use lsp_types::{
    notification::{
        DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    },
    Diagnostic, DidChangeTextDocumentParams, DidChangeWatchedFilesParams, FileEvent, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, HoverProviderCapability, MessageType, OneOf, PublishDiagnosticsParams,
    ServerCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncSaveOptions,
    VersionedTextDocumentIdentifier,
};
use ropey::Rope;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use url::Url;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ServerStatus {
    Starting,
    Ready { name: String, languages: Vec<String>, completion_trigger_characters: Vec<String> },
    NotInstalled { command: String },
    Failed { message: String },
    /// The server went away on its own. `restarting`: a new process is being
    /// started; otherwise it crashed too often and stays stopped.
    Exited { code: Option<i32>, restarting: bool, message: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ManagerEvent {
    Status { server: String, status: ServerStatus },
    Diagnostics { server: String, uri: Url, version: Option<i32>, diagnostics: Vec<Diagnostic> },
    Log { server: String, level: &'static str, message: String },
    ShowMessage { server: String, level: &'static str, message: String },
    Progress { server: String, token: Value, value: Value },
}

enum Cmd {
    Open { uri: Url, language_id: String, version: i32, text: String },
    Change { uri: Url, version: i32, changes: Vec<TextDocumentContentChangeEvent> },
    Save { uri: Url },
    Close { uri: Url },
    FilesChanged { changes: Vec<FileEvent> },
    Request { method: String, params: Value, reply: oneshot::Sender<Result<Value, LspError>> },
    Restart,
    Shutdown { done: oneshot::Sender<()> },
}

pub struct LspManager {
    root: Url,
    root_path: PathBuf,
    configs: Vec<ServerConfig>,
    servers: Mutex<HashMap<String, mpsc::UnboundedSender<Cmd>>>,
    /// Which server each open document was sent to.
    docs: Mutex<HashMap<Url, String>>,
    events: mpsc::UnboundedSender<ManagerEvent>,
}

impl LspManager {
    pub fn new(root_path: &Path, root: Url, configs: Vec<ServerConfig>, events: mpsc::UnboundedSender<ManagerEvent>) -> Self {
        Self {
            root,
            root_path: root_path.to_owned(),
            configs,
            servers: Mutex::default(),
            docs: Mutex::default(),
            events,
        }
    }

    pub fn server_for_language(&self, language_id: &str) -> Option<&ServerConfig> {
        self.configs.iter().find(|c| c.languages.iter().any(|l| l == language_id))
    }

    /// Sends `didOpen`, starting the server first if needed. Returns false when
    /// no server is configured for the language.
    pub fn open(&self, uri: Url, language_id: &str, version: i32, text: String) -> bool {
        let Some(config) = self.server_for_language(language_id) else { return false };
        let mut docs = self.docs.lock().unwrap();
        if docs.contains_key(&uri) {
            return true;
        }
        docs.insert(uri.clone(), config.id.clone());
        let tx = self.server_sender(config);
        let _ = tx.send(Cmd::Open { uri, language_id: language_id.to_owned(), version, text });
        true
    }

    pub fn change(&self, uri: &Url, version: i32, changes: Vec<TextDocumentContentChangeEvent>) {
        self.send_for(uri, Cmd::Change { uri: uri.clone(), version, changes });
    }

    pub fn save(&self, uri: &Url) {
        self.send_for(uri, Cmd::Save { uri: uri.clone() });
    }

    pub fn close(&self, uri: &Url) {
        self.send_for(uri, Cmd::Close { uri: uri.clone() });
        self.docs.lock().unwrap().remove(uri);
    }

    /// Tells every running server about file changes in the workspace. Each
    /// server only gets the events matching its registered watchers.
    pub fn files_changed(&self, changes: Vec<FileEvent>) {
        if changes.is_empty() {
            return;
        }
        for tx in self.servers.lock().unwrap().values() {
            let _ = tx.send(Cmd::FilesChanged { changes: changes.clone() });
        }
    }

    /// Sends a request to the server that owns `uri`. Resolves to `null` when the
    /// document has no server or the server lacks the capability.
    pub async fn request(&self, uri: &Url, method: &str, params: Value) -> Result<Value, LspError> {
        let (reply, rx) = oneshot::channel();
        let cmd = Cmd::Request { method: method.to_owned(), params, reply };
        if !self.send_for(uri, cmd) {
            return Ok(Value::Null);
        }
        rx.await.unwrap_or(Err(LspError::ServerExited))
    }

    /// Stops the server (gracefully) and starts it again with every open
    /// document, also when it had failed or crashed too often. Returns false
    /// when that server has not been started in this workspace.
    pub fn restart(&self, server_id: &str) -> bool {
        self.servers.lock().unwrap().get(server_id).is_some_and(|tx| tx.send(Cmd::Restart).is_ok())
    }

    /// The server a document was sent to.
    pub fn server_of(&self, uri: &Url) -> Option<String> {
        self.docs.lock().unwrap().get(uri).cloned()
    }

    /// Shuts down every running server.
    pub async fn shutdown(&self) {
        let senders: Vec<_> = self.servers.lock().unwrap().drain().map(|(_, tx)| tx).collect();
        self.docs.lock().unwrap().clear();
        let mut waits = Vec::new();
        for tx in senders {
            let (done, rx) = oneshot::channel();
            if tx.send(Cmd::Shutdown { done }).is_ok() {
                waits.push(rx);
            }
        }
        for rx in waits {
            let _ = rx.await;
        }
    }

    fn send_for(&self, uri: &Url, cmd: Cmd) -> bool {
        let Some(server_id) = self.docs.lock().unwrap().get(uri).cloned() else { return false };
        let servers = self.servers.lock().unwrap();
        servers.get(&server_id).is_some_and(|tx| tx.send(cmd).is_ok())
    }

    fn server_sender(&self, config: &ServerConfig) -> mpsc::UnboundedSender<Cmd> {
        let mut servers = self.servers.lock().unwrap();
        if let Some(tx) = servers.get(&config.id) {
            return tx.clone();
        }
        let (tx, rx) = mpsc::unbounded_channel();
        servers.insert(config.id.clone(), tx.clone());
        tokio::spawn(run_server(config.clone(), self.root.clone(), self.root_path.clone(), rx, self.events.clone()));
        tx
    }
}

async fn run_server(
    config: ServerConfig,
    root: Url,
    root_path: PathBuf,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    events: mpsc::UnboundedSender<ManagerEvent>,
) {
    let id = config.id.clone();
    let status = |status: ServerStatus| {
        let _ = events.send(ManagerEvent::Status { server: id.clone(), status });
    };
    // Survive restarts: the documents the server should have open, and the
    // commands that arrived while it was starting.
    let mut mirrors: HashMap<Url, Mirror> = HashMap::new();
    let mut queued: VecDeque<Cmd> = VecDeque::new();
    let mut restarts = RestartPolicy::default();
    loop {
        status(ServerStatus::Starting);
        let (client, mut incoming, caps) = match start(&config, &root, &root_path, &mut rx, &mut queued, &events).await {
            Started::Ready(client, incoming, caps) => (client, incoming, caps),
            Started::Failed(failure, reason) => {
                status(failure);
                if stopped(&mut queued, &mut rx, &mut mirrors, &reason).await {
                    continue;
                }
                return;
            }
            Started::Restart => continue,
            Started::ShutDown => return,
        };
        status(ServerStatus::Ready {
            name: config.id.clone(),
            languages: config.languages.clone(),
            completion_trigger_characters: caps
                .completion_provider
                .as_ref()
                .and_then(|c| c.trigger_characters.clone())
                .unwrap_or_default(),
        });

        let sync = SyncInfo::from(&*caps);
        // After a restart, the new process gets every document the old one had,
        // at its current version, before anything that was queued meanwhile.
        if sync.open_close {
            for (uri, doc) in &mirrors {
                let item = TextDocumentItem {
                    uri: uri.clone(),
                    language_id: doc.language_id.clone(),
                    version: doc.version,
                    text: doc.text.to_string(),
                };
                let _ = client.notify::<DidOpenTextDocument>(DidOpenTextDocumentParams { text_document: item });
            }
        }
        while let Some(cmd) = queued.pop_front() {
            handle(cmd, &client, &caps, &sync, &mut mirrors);
        }

        let code = loop {
            tokio::select! {
                cmd = rx.recv() => {
                    match cmd {
                        Some(Cmd::Shutdown { done }) => {
                            client.shutdown().await;
                            let _ = done.send(());
                            return;
                        }
                        // Asked for (like vscode-languageclient's restart()): stop
                        // gracefully and start again; not counted as a crash.
                        Some(Cmd::Restart) => {
                            client.shutdown().await;
                            break None;
                        }
                        Some(cmd) => handle(cmd, &client, &caps, &sync, &mut mirrors),
                        None => {
                            client.shutdown().await;
                            return;
                        }
                    }
                }
                event = incoming.recv() => match event {
                    Some(Incoming::Exited(code)) => break Some(code),
                    None => break Some(None),
                    Some(event) => forward(&id, event, &events),
                }
            }
        };
        let Some(code) = code else { continue };

        // The server went away without being asked to: restart it unless it
        // keeps crashing (vscode-languageclient's DefaultErrorHandler.closed).
        if restarts.closed(Instant::now()) {
            let message = "Connection to server got closed. Server will restart.".to_owned();
            let _ = events.send(ManagerEvent::Log { server: id.clone(), level: "info", message: message.clone() });
            status(ServerStatus::Exited { code, restarting: true, message });
        } else {
            let message = format!(
                "The {id} server crashed {} times in the last 3 minutes. The server will not be restarted.",
                MAX_RESTART_COUNT + 1
            );
            let _ = events.send(ManagerEvent::Log { server: id.clone(), level: "error", message: message.clone() });
            status(ServerStatus::Exited { code, restarting: false, message: message.clone() });
            if !stopped(&mut queued, &mut rx, &mut mirrors, &message).await {
                return;
            }
        }
    }
}

enum Started {
    Ready(LspClient, mpsc::UnboundedReceiver<Incoming>, Box<ServerCapabilities>),
    /// The status to report and the reason to give requests.
    Failed(ServerStatus, String),
    /// A restart was asked for while starting: start over.
    Restart,
    /// Shut down (or the manager went away) before the server was ready.
    ShutDown,
}

/// Spawns the server and runs the `initialize` handshake. Commands arriving
/// meanwhile are queued, except a shutdown or restart, which ends the start.
async fn start(
    config: &ServerConfig,
    root: &Url,
    root_path: &Path,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    queued: &mut VecDeque<Cmd>,
    events: &mpsc::UnboundedSender<ManagerEvent>,
) -> Started {
    let Ok(program) = which::which(&config.command) else {
        let reason = format!("{} is not installed", config.command);
        return Started::Failed(ServerStatus::NotInstalled { command: config.command.clone() }, reason);
    };
    let (client, mut incoming) = match LspClient::spawn(&program, &config.args, root, root_path) {
        Ok(spawned) => spawned,
        Err(e) => return Started::Failed(ServerStatus::Failed { message: e.to_string() }, e.to_string()),
    };
    // Keep reading server output during the handshake so stderr cannot fill up,
    // and remember the first stderr line to explain a failed start.
    let mut first_stderr: Option<String> = None;
    let init = {
        let handshake = client.initialize(root);
        tokio::pin!(handshake);
        loop {
            tokio::select! {
                result = &mut handshake => break result,
                Some(event) = incoming.recv() => {
                    if let Incoming::Stderr(line) = &event
                        && first_stderr.is_none()
                        && !line.trim().is_empty()
                    {
                        first_stderr = Some(line.trim().to_owned());
                    }
                    forward(&config.id, event, events);
                }
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Shutdown { done }) => {
                        client.kill().await;
                        let _ = done.send(());
                        return Started::ShutDown;
                    }
                    Some(Cmd::Restart) => {
                        client.kill().await;
                        return Started::Restart;
                    }
                    Some(cmd) => queued.push_back(cmd),
                    None => {
                        client.kill().await;
                        return Started::ShutDown;
                    }
                },
            }
        }
    };
    match init {
        Ok(result) => Started::Ready(client, incoming, Box::new(result.capabilities)),
        Err(e) => {
            let message = match first_stderr {
                Some(line) => format!("{e}: {line}"),
                None => e.to_string(),
            };
            client.shutdown().await;
            Started::Failed(ServerStatus::Failed { message }, e.to_string())
        }
    }
}

/// While no server process runs (it failed to start, is not installed or
/// crashed too often): keep the mirrors up to date, answer requests (queued
/// ones first) with an error and drop everything else. Returns true when a
/// restart is asked for, false on shutdown.
async fn stopped(
    queued: &mut VecDeque<Cmd>,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    mirrors: &mut HashMap<Url, Mirror>,
    reason: &str,
) -> bool {
    loop {
        let cmd = match queued.pop_front() {
            Some(cmd) => cmd,
            None => match rx.recv().await {
                Some(cmd) => cmd,
                None => return false,
            },
        };
        match cmd {
            Cmd::Open { uri, language_id, version, text } => {
                mirrors.insert(uri, Mirror { language_id, version, text: Rope::from_str(&text) });
            }
            Cmd::Change { uri, version, changes } => {
                if let Some(mirror) = mirrors.get_mut(&uri) {
                    rc_core::text::apply_changes(&mut mirror.text, &changes);
                    mirror.version = version;
                }
            }
            Cmd::Close { uri } => {
                mirrors.remove(&uri);
            }
            Cmd::Request { reply, .. } => {
                let _ = reply.send(Err(LspError::Unavailable(reason.to_owned())));
            }
            Cmd::Save { .. } | Cmd::FilesChanged { .. } => {}
            Cmd::Restart => return true,
            Cmd::Shutdown { done } => {
                let _ = done.send(());
                return false;
            }
        }
    }
}

/// Restarts allowed in a row before the time window is checked.
const MAX_RESTART_COUNT: usize = 4;
const RESTART_WINDOW: Duration = Duration::from_secs(3 * 60);

/// vscode-languageclient's default: restart after every close, unless the
/// server closed `MAX_RESTART_COUNT + 1` times within three minutes.
#[derive(Default)]
struct RestartPolicy {
    closes: VecDeque<Instant>,
}

impl RestartPolicy {
    /// Records a close and says whether to restart.
    fn closed(&mut self, now: Instant) -> bool {
        self.closes.push_back(now);
        if self.closes.len() <= MAX_RESTART_COUNT {
            return true;
        }
        let first = self.closes[0];
        if now.duration_since(first) <= RESTART_WINDOW {
            return false;
        }
        self.closes.pop_front();
        true
    }
}

/// A document as the server knows it.
struct Mirror {
    language_id: String,
    version: i32,
    text: Rope,
}

fn handle(cmd: Cmd, client: &LspClient, caps: &ServerCapabilities, sync: &SyncInfo, mirrors: &mut HashMap<Url, Mirror>) {
    match cmd {
        Cmd::Open { uri, language_id, version, text } => {
            let mirror = Mirror { language_id: language_id.clone(), version, text: Rope::from_str(&text) };
            mirrors.insert(uri.clone(), mirror);
            if sync.open_close {
                let item = TextDocumentItem { uri, language_id, version, text };
                let _ = client.notify::<DidOpenTextDocument>(DidOpenTextDocumentParams { text_document: item });
            }
        }
        Cmd::Change { uri, version, changes } => {
            let Some(mirror) = mirrors.get_mut(&uri) else { return };
            rc_core::text::apply_changes(&mut mirror.text, &changes);
            mirror.version = version;
            let content_changes = if sync.change == TextDocumentSyncKind::INCREMENTAL {
                changes
            } else if sync.change == TextDocumentSyncKind::FULL {
                vec![TextDocumentContentChangeEvent { range: None, range_length: None, text: mirror.text.to_string() }]
            } else {
                return;
            };
            let _ = client.notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier { uri, version },
                content_changes,
            });
        }
        Cmd::Save { uri } => {
            let Some(include_text) = sync.save else { return };
            let text = include_text.then(|| mirrors.get(&uri).map(|m| m.text.to_string())).flatten();
            let _ = client.notify::<DidSaveTextDocument>(DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri },
                text,
            });
        }
        Cmd::Close { uri } => {
            if mirrors.remove(&uri).is_some() && sync.open_close {
                let _ = client.notify::<DidCloseTextDocument>(DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier { uri },
                });
            }
        }
        Cmd::FilesChanged { changes } => {
            let changes = client.watched_file_events(&changes);
            if !changes.is_empty() {
                let _ = client.notify::<DidChangeWatchedFiles>(DidChangeWatchedFilesParams { changes });
            }
        }
        Cmd::Request { method, params, reply } => {
            if !supports(caps, &method) {
                let _ = reply.send(Ok(Value::Null));
                return;
            }
            // Queue the write now (preserving order), wait for the reply elsewhere.
            match client.send_request(&method, params) {
                Ok((id, rx)) => {
                    let client = client.clone();
                    tokio::spawn(async move {
                        let result = client.await_response(id, &method, rx, REQUEST_TIMEOUT).await;
                        let _ = reply.send(result);
                    });
                }
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            }
        }
        Cmd::Restart | Cmd::Shutdown { .. } => unreachable!("handled by run_server"),
    }
}

fn forward(server: &str, event: Incoming, events: &mpsc::UnboundedSender<ManagerEvent>) {
    let server = server.to_owned();
    let event = match event {
        Incoming::Notification { method, params } => match method.as_str() {
            "textDocument/publishDiagnostics" => match serde_json::from_value::<PublishDiagnosticsParams>(params) {
                Ok(p) => ManagerEvent::Diagnostics { server, uri: p.uri, version: p.version, diagnostics: p.diagnostics },
                Err(e) => ManagerEvent::Log { server, level: "error", message: format!("bad diagnostics: {e}") },
            },
            "window/logMessage" | "window/showMessage" => {
                let level = message_level(&params);
                let message = params.get("message").and_then(Value::as_str).unwrap_or_default().to_owned();
                if method == "window/logMessage" {
                    ManagerEvent::Log { server, level, message }
                } else {
                    ManagerEvent::ShowMessage { server, level, message }
                }
            }
            "$/progress" => ManagerEvent::Progress {
                server,
                token: params.get("token").cloned().unwrap_or(Value::Null),
                value: params.get("value").cloned().unwrap_or(Value::Null),
            },
            _ => return,
        },
        Incoming::Stderr(line) => ManagerEvent::Log { server, level: "log", message: line },
        Incoming::Exited(_) => return,
    };
    let _ = events.send(event);
}

fn message_level(params: &Value) -> &'static str {
    let ty = params.get("type").cloned().and_then(|t| serde_json::from_value::<MessageType>(t).ok());
    match ty {
        Some(MessageType::ERROR) => "error",
        Some(MessageType::WARNING) => "warning",
        Some(MessageType::INFO) => "info",
        _ => "log",
    }
}

/// How the server wants documents synchronized.
struct SyncInfo {
    open_close: bool,
    change: TextDocumentSyncKind,
    /// `Some(include_text)` when the server wants `didSave`.
    save: Option<bool>,
}

impl From<&ServerCapabilities> for SyncInfo {
    fn from(caps: &ServerCapabilities) -> Self {
        match &caps.text_document_sync {
            None => Self { open_close: false, change: TextDocumentSyncKind::NONE, save: None },
            // The bare-kind form implies open/close and save, as in vscode-languageclient.
            Some(TextDocumentSyncCapability::Kind(kind)) => {
                let on = *kind != TextDocumentSyncKind::NONE;
                Self { open_close: on, change: *kind, save: on.then_some(false) }
            }
            Some(TextDocumentSyncCapability::Options(o)) => Self {
                open_close: o.open_close.unwrap_or(false),
                change: o.change.unwrap_or(TextDocumentSyncKind::NONE),
                save: match &o.save {
                    None | Some(TextDocumentSyncSaveOptions::Supported(false)) => None,
                    Some(TextDocumentSyncSaveOptions::Supported(true)) => Some(false),
                    Some(TextDocumentSyncSaveOptions::SaveOptions(s)) => Some(s.include_text.unwrap_or(false)),
                },
            },
        }
    }
}

fn supports(caps: &ServerCapabilities, method: &str) -> bool {
    match method {
        "textDocument/completion" => caps.completion_provider.is_some(),
        "completionItem/resolve" => caps.completion_provider.as_ref().and_then(|c| c.resolve_provider).unwrap_or(false),
        "textDocument/hover" => !matches!(caps.hover_provider, None | Some(HoverProviderCapability::Simple(false))),
        "textDocument/definition" => !matches!(caps.definition_provider, None | Some(OneOf::Left(false))),
        "textDocument/signatureHelp" => caps.signature_help_provider.is_some(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_policy_matches_vscode_languageclient() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut policy = RestartPolicy::default();
        // Four closes always restart.
        for s in 0..4 {
            assert!(policy.closed(at(s)));
        }
        // The fifth within three minutes of the first does not.
        assert!(!policy.closed(at(180)));

        let mut policy = RestartPolicy::default();
        for s in 0..4 {
            assert!(policy.closed(at(s)));
        }
        // Spread over more than three minutes: keep restarting, sliding the window.
        for s in [181, 182, 200, 201] {
            assert!(policy.closed(at(s)));
        }
        // Now 181, 182, 200, 201 and 202 fall within three minutes.
        assert!(!policy.closed(at(202)));
    }
}
