//! End-to-end test against a real rust-analyzer. Skipped if it is not on PATH.

use lsp_types::{FileChangeType, FileEvent, Position, Range, TextDocumentContentChangeEvent};
use rc_lsp::{default_servers, LspManager, ManagerEvent, ServerStatus};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use url::Url;

const MAIN_RS: &str = "\
fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn main() {
    let total = add(1, 2);
    let s: i32 = \"oops\";
}
";

struct Events {
    rx: mpsc::UnboundedReceiver<ManagerEvent>,
    seen: Vec<ManagerEvent>,
}

impl Events {
    async fn wait_for(&mut self, what: &str, timeout: Duration, pred: impl Fn(&ManagerEvent) -> bool) {
        if self.seen.iter().any(&pred) {
            return;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.rx.recv()).await {
                Ok(Some(ev)) => {
                    let hit = pred(&ev);
                    self.seen.push(ev);
                    if hit {
                        return;
                    }
                }
                _ => panic!("timed out waiting for {what}; events: {:#?}", self.seen.iter().rev().take(10).collect::<Vec<_>>()),
            }
        }
    }
}

/// rust-analyzer answers `null` or "content modified" while it is still loading the
/// project, so retry until `accept` is satisfied.
async fn request_until(mgr: &LspManager, uri: &Url, method: &str, params: Value, accept: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let result = mgr.request(uri, method, params.clone()).await;
        if let Ok(v) = &result
            && accept(v)
        {
            return v.clone();
        }
        assert!(Instant::now() < deadline, "{method} never returned an acceptable result; last: {result:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_analyzer_end_to_end() {
    if which::which("rust-analyzer").is_err() {
        eprintln!("rust-analyzer not found, skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), MAIN_RS).unwrap();

    let root = rc_core::uri::canonicalize(dir.path()).unwrap();
    let root_uri = rc_core::uri::path_to_uri(&root).unwrap();
    let uri = rc_core::uri::path_to_uri(&root.join("src/main.rs")).unwrap();

    let (tx, rx) = mpsc::unbounded_channel();
    let mut events = Events { rx, seen: Vec::new() };
    let mgr = LspManager::new(&root, root_uri, default_servers(), tx);

    assert!(!mgr.open(uri.clone(), "plaintext", 0, String::new()), "no server for plaintext");
    assert!(mgr.open(uri.clone(), "rust", 0, MAIN_RS.into()));

    // Requests queued before the server is ready must still be answered.
    let hover_params = json!({"textDocument": {"uri": uri}, "position": {"line": 5, "character": 17}});
    let hover = request_until(&mgr, &uri, "textDocument/hover", hover_params.clone(), |v| !v.is_null()).await;
    assert!(hover.to_string().contains("fn add"), "hover: {hover}");

    events
        .wait_for("ready status", Duration::from_secs(5), |e| {
            matches!(e, ManagerEvent::Status { status: ServerStatus::Ready { .. }, .. })
        })
        .await;

    let def = request_until(&mgr, &uri, "textDocument/definition", hover_params, |v| !v.is_null()).await;
    let target_line = def.pointer("/0/targetRange/start/line").or(def.pointer("/0/range/start/line"));
    assert_eq!(target_line, Some(&json!(0)), "definition: {def}");

    events
        .wait_for("type mismatch diagnostic", Duration::from_secs(120), |e| {
            matches!(e, ManagerEvent::Diagnostics { diagnostics, .. }
                if diagnostics.iter().any(|d| d.range.start.line == 6
                    && d.code == Some(lsp_types::NumberOrString::String("E0308".into()))))
        })
        .await;

    // Incremental edit: add "    tot" before the closing brace, then complete it.
    let at = Position::new(7, 0);
    mgr.change(
        &uri,
        1,
        vec![TextDocumentContentChangeEvent { range: Some(Range::new(at, at)), range_length: None, text: "    tot\n".into() }],
    );
    let completion_params = json!({
        "textDocument": {"uri": uri},
        "position": {"line": 7, "character": 7},
        "context": {"triggerKind": 1}
    });
    let completion = request_until(&mgr, &uri, "textDocument/completion", completion_params, |v| {
        v.to_string().contains("\"total\"")
    })
    .await;
    assert!(completion.to_string().contains("\"total\""));

    // A file created on disk after the project loaded is only seen through
    // workspace/didChangeWatchedFiles: rust-analyzer registers watchers instead
    // of watching itself because we advertise dynamic registration.
    let util = root.join("src/util.rs");
    std::fs::write(&util, "pub fn helper() -> i32 {\n    42\n}\n").unwrap();
    let util_uri = rc_core::uri::path_to_uri(&util).unwrap();
    mgr.files_changed(vec![FileEvent { uri: util_uri.clone(), typ: FileChangeType::CREATED }]);
    let end = Position::new(9, 0);
    mgr.change(
        &uri,
        2,
        vec![TextDocumentContentChangeEvent {
            range: Some(Range::new(end, end)),
            range_length: None,
            text: "mod util;\nfn answer() -> i32 {\n    util::helper()\n}\n".into(),
        }],
    );
    let helper_params = json!({"textDocument": {"uri": uri}, "position": {"line": 11, "character": 12}});
    let hover =
        request_until(&mgr, &uri, "textDocument/hover", helper_params.clone(), |v| v.to_string().contains("fn helper")).await;
    assert!(hover.to_string().contains("fn helper() -> i32"), "hover: {hover}");

    // rust-analyzer does not re-read a file it already knows unless told it changed.
    std::fs::write(&util, "pub fn helper() -> u64 {\n    42\n}\n").unwrap();
    mgr.files_changed(vec![FileEvent { uri: util_uri.clone(), typ: FileChangeType::CHANGED }]);
    let hover = request_until(&mgr, &uri, "textDocument/hover", helper_params, |v| v.to_string().contains("-> u64")).await;
    assert!(hover.to_string().contains("fn helper() -> u64"), "hover: {hover}");

    // Rename across files: we announce `documentChanges`, so the edit is
    // versioned for the open document and unversioned for the other file.
    let position = json!({"textDocument": {"uri": uri}, "position": {"line": 11, "character": 12}});
    let prepared = request_until(&mgr, &uri, "textDocument/prepareRename", position.clone(), |v| !v.is_null()).await;
    let range = prepared.get("range").unwrap_or(&prepared);
    assert_eq!(range, &json!({"start": {"line": 11, "character": 10}, "end": {"line": 11, "character": 16}}));
    let mut rename_params = position;
    rename_params["newName"] = json!("assist");
    let edit = request_until(&mgr, &uri, "textDocument/rename", rename_params, |v| !v.is_null()).await;
    let changes = edit["documentChanges"].as_array().unwrap_or_else(|| panic!("documentChanges: {edit}"));
    let for_file = |u: &Url| changes.iter().find(|c| c["textDocument"]["uri"] == json!(u)).unwrap_or_else(|| panic!("{edit}"));
    assert_eq!(for_file(&uri)["textDocument"]["version"], json!(2));
    assert_eq!(for_file(&util_uri)["textDocument"]["version"], Value::Null);
    assert!(for_file(&util_uri)["edits"].to_string().contains("\"assist\""), "{edit}");

    mgr.close(&uri);
    mgr.shutdown().await;
    events
        .wait_for("exit or shutdown", Duration::from_secs(1), |_| true)
        .await;
}
