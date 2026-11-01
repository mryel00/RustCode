//! A JSON-RPC client for one language server process over stdio.

use crate::{
    transport::{encode_message, read_message},
    watched_files::{self, WatchedFiles},
};
use lsp_types::{
    notification::Notification, request::Request, ClientCapabilities, ClientInfo,
    CompletionClientCapabilities, CompletionItemCapability, CompletionItemCapabilityResolveSupport,
    DiagnosticTag, DidChangeWatchedFilesClientCapabilities, FileEvent, GeneralClientCapabilities, GotoCapability, HoverClientCapabilities,
    InitializeParams, InitializeResult, MarkupKind, PositionEncodingKind,
    PublishDiagnosticsClientCapabilities, SignatureHelpClientCapabilities,
    TagSupport, TextDocumentClientCapabilities, TextDocumentSyncClientCapabilities,
    WindowClientCapabilities, WorkspaceClientCapabilities, WorkspaceFolder,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot, watch},
};
use url::Url;

const METHOD_NOT_FOUND: i64 = -32601;
const REQUEST_CANCELLED: i64 = -32800;

#[derive(Debug, Clone, thiserror::Error)]
pub enum LspError {
    #[error("failed to start language server: {0}")]
    Spawn(String),
    #[error("language server error {code}: {message}")]
    Response { code: i64, message: String },
    #[error("language server request timed out: {0}")]
    Timeout(String),
    #[error("language server is not running")]
    ServerExited,
    #[error("language server unavailable: {0}")]
    Unavailable(String),
    #[error("invalid language server response: {0}")]
    Decode(String),
}

impl LspError {
    /// The request was cancelled (by us or by the server), which callers can
    /// treat as "no result" rather than as a failure.
    pub fn is_cancellation(&self) -> bool {
        matches!(self, Self::Response { code: REQUEST_CANCELLED, .. })
    }
}

/// Things the server sends us that are not responses to our requests.
#[derive(Debug)]
pub enum Incoming {
    Notification { method: String, params: Value },
    Stderr(String),
    /// The process exited. Always the last event.
    Exited(Option<i32>),
}

type Pending = Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspError>>>>;

struct Inner {
    next_id: AtomicI64,
    pending: Pending,
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    workspace_folders: Vec<WorkspaceFolder>,
    exited: watch::Receiver<bool>,
    kill: Mutex<Option<oneshot::Sender<()>>>,
    watched_files: Mutex<WatchedFiles>,
    /// Reports problems that are not tied to a request (bad registrations).
    events: mpsc::UnboundedSender<Incoming>,
}

/// Cheap to clone; all clones talk to the same process.
#[derive(Clone)]
pub struct LspClient {
    inner: Arc<Inner>,
}

impl LspClient {
    /// Spawns `program` and wires up stdio. Call [`LspClient::initialize`] next.
    pub fn spawn(
        program: &Path,
        args: &[String],
        root: &Url,
        root_path: &Path,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Incoming>), LspError> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(root_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| LspError::Spawn(format!("{}: {e}", program.display())))?;

        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (exited_tx, exited_rx) = watch::channel(false);
        let (kill_tx, kill_rx) = oneshot::channel();

        let name = root_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let inner = Arc::new(Inner {
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            outgoing: out_tx,
            workspace_folders: vec![WorkspaceFolder { uri: root.clone(), name }],
            exited: exited_rx,
            kill: Mutex::new(Some(kill_tx)),
            watched_files: Mutex::default(),
            events: in_tx.clone(),
        });

        tokio::spawn(write_loop(stdin, out_rx));
        tokio::spawn(read_loop(stdout, Arc::clone(&inner), in_tx.clone()));
        {
            let in_tx = in_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = in_tx.send(Incoming::Stderr(line));
                }
            });
        }
        tokio::spawn(wait_loop(child, kill_rx, exited_tx, in_tx));

        Ok((Self { inner }, in_rx))
    }

    /// Runs the `initialize` handshake and sends `initialized`.
    pub async fn initialize(&self, root: &Url) -> Result<InitializeResult, LspError> {
        #[allow(deprecated)] // root_uri is deprecated but older servers still need it
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root.clone()),
            workspace_folders: Some(self.inner.workspace_folders.clone()),
            client_info: Some(ClientInfo { name: "RustCode".into(), version: Some(env!("CARGO_PKG_VERSION").into()) }),
            capabilities: client_capabilities(),
            ..Default::default()
        };
        let value = self
            .request_value(
                lsp_types::request::Initialize::METHOD,
                serde_json::to_value(params).expect("params serialize"),
                Duration::from_secs(60),
            )
            .await?;
        let result: InitializeResult =
            serde_json::from_value(value).map_err(|e| LspError::Decode(e.to_string()))?;
        self.notify::<lsp_types::notification::Initialized>(lsp_types::InitializedParams {})?;
        Ok(result)
    }

    /// Writes a request and returns a receiver for its response. The write is
    /// queued before this returns, so calls are sent in the order they are made.
    pub fn send_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(i64, oneshot::Receiver<Result<Value, LspError>>), LspError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if self.inner.outgoing.send(encode_message(&msg)).is_err() {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(LspError::ServerExited);
        }
        Ok((id, rx))
    }

    /// Waits for a response from [`send_request`](Self::send_request), cancelling on timeout.
    pub async fn await_response(
        &self,
        id: i64,
        method: &str,
        rx: oneshot::Receiver<Result<Value, LspError>>,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(LspError::ServerExited),
            Err(_) => {
                self.inner.pending.lock().unwrap().remove(&id);
                let _ = self.notify_value("$/cancelRequest", json!({"id": id}));
                Err(LspError::Timeout(method.to_owned()))
            }
        }
    }

    pub async fn request_value(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, LspError> {
        let (id, rx) = self.send_request(method, params)?;
        self.await_response(id, method, rx, timeout).await
    }

    pub async fn request<R: Request>(&self, params: R::Params, timeout: Duration) -> Result<R::Result, LspError> {
        let params = serde_json::to_value(params).expect("params serialize");
        let value = self.request_value(R::METHOD, params, timeout).await?;
        serde_json::from_value(value).map_err(|e| LspError::Decode(e.to_string()))
    }

    pub fn notify_value(&self, method: &str, params: Value) -> Result<(), LspError> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.inner.outgoing.send(encode_message(&msg)).map_err(|_| LspError::ServerExited)
    }

    pub fn notify<N: Notification>(&self, params: N::Params) -> Result<(), LspError> {
        self.notify_value(N::METHOD, serde_json::to_value(params).expect("params serialize"))
    }

    /// The file events the server registered interest in through
    /// `workspace/didChangeWatchedFiles`.
    pub fn watched_file_events(&self, changes: &[FileEvent]) -> Vec<FileEvent> {
        self.inner.watched_files.lock().unwrap().filter(changes)
    }

    /// Graceful `shutdown` + `exit`, killing the process if it does not exit in time.
    pub async fn shutdown(&self) {
        if !*self.inner.exited.borrow() {
            let _ = self.request::<lsp_types::request::Shutdown>((), Duration::from_secs(5)).await;
            let _ = self.notify::<lsp_types::notification::Exit>(());
            let mut exited = self.inner.exited.clone();
            let _ = tokio::time::timeout(Duration::from_secs(2), exited.wait_for(|e| *e)).await;
        }
        self.kill().await;
    }

    /// Kills the process without the `shutdown` handshake (for a server that has
    /// not finished initializing) and waits until it is gone.
    pub async fn kill(&self) {
        if let Some(kill) = self.inner.kill.lock().unwrap().take() {
            let _ = kill.send(());
        }
        let mut exited = self.inner.exited.clone();
        let _ = tokio::time::timeout(Duration::from_secs(2), exited.wait_for(|e| *e)).await;
    }
}

async fn write_loop(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(bytes) = rx.recv().await {
        if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
            break;
        }
    }
}

async fn read_loop(stdout: ChildStdout, inner: Arc<Inner>, events: mpsc::UnboundedSender<Incoming>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let msg = match read_message(&mut reader).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break,
            Err(e) => {
                let _ = events.send(Incoming::Stderr(format!("[rustcode] protocol error: {e}")));
                break;
            }
        };
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = msg.get("id").filter(|id| !id.is_null()).cloned();
        match (method, id) {
            // Response to one of our requests.
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let Some(tx) = inner.pending.lock().unwrap().remove(&id) else { continue };
                let result = match msg.get("error") {
                    Some(err) => Err(LspError::Response {
                        code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: err.get("message").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    }),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(result);
            }
            // Request from the server.
            (Some(method), Some(id)) => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let reply = match answer_server_request(&method, &params, &inner) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(message) => json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": METHOD_NOT_FOUND, "message": message}}),
                };
                let _ = inner.outgoing.send(encode_message(&reply));
            }
            (Some(method), None) => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let _ = events.send(Incoming::Notification { method, params });
            }
            (None, None) => {}
        }
    }
    // The server is gone: fail everything still waiting.
    for (_, tx) in inner.pending.lock().unwrap().drain() {
        let _ = tx.send(Err(LspError::ServerExited));
    }
}

async fn wait_loop(
    mut child: Child,
    kill: oneshot::Receiver<()>,
    exited: watch::Sender<bool>,
    events: mpsc::UnboundedSender<Incoming>,
) {
    let status = tokio::select! {
        status = child.wait() => status.ok(),
        _ = kill => {
            let _ = child.kill().await;
            child.wait().await.ok()
        }
    };
    let _ = exited.send(true);
    let _ = events.send(Incoming::Exited(status.and_then(|s| s.code())));
}

/// Minimal answers for requests servers commonly send to clients.
fn answer_server_request(method: &str, params: &Value, inner: &Inner) -> Result<Value, String> {
    match method {
        // One `null` per requested item means "use your defaults".
        "workspace/configuration" => {
            let n = params.get("items").and_then(Value::as_array).map_or(0, Vec::len);
            Ok(Value::Array(vec![Value::Null; n]))
        }
        "workspace/workspaceFolders" => Ok(serde_json::to_value(&inner.workspace_folders).expect("serialize")),
        "client/registerCapability" => {
            let registrations = params.get("registrations").and_then(Value::as_array).cloned().unwrap_or_default();
            for r in registrations {
                if r.get("method").and_then(Value::as_str) != Some(watched_files::METHOD) {
                    continue;
                }
                let id = r.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                let options = r.get("registerOptions").cloned().unwrap_or(Value::Null);
                if let Err(e) = inner.watched_files.lock().unwrap().register(id, options) {
                    let _ = inner.events.send(Incoming::Stderr(format!("[rustcode] {e}")));
                }
            }
            Ok(Value::Null)
        }
        "client/unregisterCapability" => {
            // The LSP spec spells the field "unregisterations".
            let list = params.get("unregisterations").and_then(Value::as_array).cloned().unwrap_or_default();
            for u in list {
                if u.get("method").and_then(Value::as_str) == Some(watched_files::METHOD)
                    && let Some(id) = u.get("id").and_then(Value::as_str)
                {
                    inner.watched_files.lock().unwrap().unregister(id);
                }
            }
            Ok(Value::Null)
        }
        "window/workDoneProgress/create"
        | "window/showMessageRequest"
        | "workspace/diagnostic/refresh"
        | "workspace/semanticTokens/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/codeLens/refresh" => Ok(Value::Null),
        "workspace/applyEdit" => Ok(json!({"applied": false, "failureReason": "not supported yet"})),
        _ => Err(format!("unhandled method {method}")),
    }
}

fn client_capabilities() -> ClientCapabilities {
    let markup = vec![MarkupKind::Markdown, MarkupKind::PlainText];
    ClientCapabilities {
        general: Some(GeneralClientCapabilities {
            // Monaco columns are UTF-16, so that is the only encoding we accept.
            position_encodings: Some(vec![PositionEncodingKind::UTF16]),
            ..Default::default()
        }),
        window: Some(WindowClientCapabilities { work_done_progress: Some(true), ..Default::default() }),
        workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            workspace_folders: Some(true),
            did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                relative_pattern_support: Some(true),
            }),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities {
                did_save: Some(true),
                ..Default::default()
            }),
            completion: Some(CompletionClientCapabilities {
                completion_item: Some(CompletionItemCapability {
                    snippet_support: Some(true),
                    documentation_format: Some(markup.clone()),
                    deprecated_support: Some(true),
                    preselect_support: Some(true),
                    insert_replace_support: Some(true),
                    label_details_support: Some(true),
                    resolve_support: Some(CompletionItemCapabilityResolveSupport {
                        properties: vec!["documentation".into(), "detail".into(), "additionalTextEdits".into()],
                    }),
                    ..Default::default()
                }),
                context_support: Some(true),
                ..Default::default()
            }),
            hover: Some(HoverClientCapabilities { content_format: Some(markup.clone()), ..Default::default() }),
            signature_help: Some(SignatureHelpClientCapabilities::default()),
            definition: Some(GotoCapability { link_support: Some(true), ..Default::default() }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                related_information: Some(true),
                version_support: Some(true),
                tag_support: Some(TagSupport { value_set: vec![DiagnosticTag::UNNECESSARY, DiagnosticTag::DEPRECATED] }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}
