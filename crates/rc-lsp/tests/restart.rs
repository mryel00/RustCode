//! Restarting crashed servers and answering server requests, against a small
//! fake server written in Python. Skipped if `python3` is not on PATH.

use lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use rc_lsp::{LspError, LspManager, ManagerEvent, ServerConfig, ServerStatus};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use url::Url;

const TIMEOUT: Duration = Duration::from_secs(20);

struct Fixture {
    _dir: tempfile::TempDir,
    mgr: LspManager,
    rx: mpsc::UnboundedReceiver<ManagerEvent>,
    uri: Url,
}

fn fixture(extra_args: &[&str]) -> Option<Fixture> {
    if which::which("python3").is_err() {
        eprintln!("python3 not found, skipping");
        return None;
    }
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_ls.py");
    let mut args = vec![script.to_string_lossy().into_owned()];
    args.extend(extra_args.iter().map(|a| a.to_string()));
    let config = ServerConfig { id: "fake".into(), command: "python3".into(), args, languages: vec!["fake".into()] };
    let dir = tempfile::tempdir().unwrap();
    let root = rc_core::uri::canonicalize(dir.path()).unwrap();
    let uri = rc_core::uri::path_to_uri(&root.join("a.fake")).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let mgr = LspManager::new(&root, rc_core::uri::path_to_uri(&root).unwrap(), vec![config], tx);
    Some(Fixture { _dir: dir, mgr, rx, uri })
}

impl Fixture {
    /// Waits for the next status event.
    async fn status(&mut self) -> ServerStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.rx.recv()).await {
                Ok(Some(ManagerEvent::Status { status, .. })) => return status,
                Ok(Some(_)) => {}
                _ => panic!("no status event"),
            }
        }
    }

    /// Waits for the next server request; returns its id and params.
    async fn server_request(&mut self) -> (u64, String, Value) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.rx.recv()).await {
                Ok(Some(ManagerEvent::Request { server, id, method, params })) => {
                    assert_eq!(server, "fake");
                    return (id, method, params);
                }
                Ok(Some(_)) => {}
                _ => panic!("no server request"),
            }
        }
    }

    async fn state(&self) -> Value {
        self.mgr.request(&self.uri, "fake/state", json!({})).await.unwrap()
    }

    /// Makes the server exit and returns the status that follows.
    async fn crash(&mut self) -> ServerStatus {
        let err = self.mgr.request(&self.uri, "fake/exit", json!({})).await.unwrap_err();
        assert!(matches!(err, LspError::ServerExited), "{err:?}");
        self.status().await
    }
}

#[tokio::test]
async fn crashed_server_restarts_with_open_documents() {
    let Some(mut f) = fixture(&[]) else { return };
    assert!(f.mgr.open(f.uri.clone(), "fake", 0, "one".into()));
    assert!(matches!(f.status().await, ServerStatus::Starting));
    assert!(matches!(f.status().await, ServerStatus::Ready { .. }));
    let change = TextDocumentContentChangeEvent {
        range: Some(Range::new(Position::new(0, 0), Position::new(0, 3))),
        range_length: None,
        text: "two".into(),
    };
    f.mgr.change(&f.uri, 1, vec![change]);
    let before = f.state().await;

    let status = f.crash().await;
    assert!(matches!(status, ServerStatus::Exited { code: Some(3), restarting: true, .. }), "{status:?}");
    // Changes and requests made while the server restarts are queued for it.
    let change = TextDocumentContentChangeEvent {
        range: Some(Range::new(Position::new(0, 3), Position::new(0, 3))),
        range_length: None,
        text: "!".into(),
    };
    f.mgr.change(&f.uri, 2, vec![change]);
    let after = f.state().await;
    assert!(matches!(f.status().await, ServerStatus::Starting));
    assert!(matches!(f.status().await, ServerStatus::Ready { .. }));

    assert_ne!(before["pid"], after["pid"], "a new process");
    assert_eq!(after["docs"][f.uri.as_str()], json!({"version": 2, "text": "two!"}));
    f.mgr.shutdown().await;
}

#[tokio::test]
async fn server_crashing_five_times_in_three_minutes_stays_stopped() {
    let Some(mut f) = fixture(&[]) else { return };
    f.mgr.open(f.uri.clone(), "fake", 0, "x".into());
    for _ in 0..4 {
        assert!(matches!(f.status().await, ServerStatus::Starting));
        assert!(matches!(f.status().await, ServerStatus::Ready { .. }));
        let status = f.crash().await;
        assert!(matches!(status, ServerStatus::Exited { restarting: true, .. }), "{status:?}");
    }
    assert!(matches!(f.status().await, ServerStatus::Starting));
    assert!(matches!(f.status().await, ServerStatus::Ready { .. }));
    let ServerStatus::Exited { restarting: false, message, .. } = f.crash().await else { panic!("expected a final exit") };
    assert_eq!(message, "The fake server crashed 5 times in the last 3 minutes. The server will not be restarted.");
    let err = f.mgr.request(&f.uri, "fake/state", json!({})).await.unwrap_err();
    assert!(matches!(err, LspError::Unavailable(_)), "{err:?}");

    // Edits while stopped are kept, and a manual restart brings it back.
    f.mgr.change(&f.uri, 1, vec![replace_all("y")]);
    assert!(f.mgr.restart("fake"));
    assert!(matches!(f.status().await, ServerStatus::Starting));
    assert!(matches!(f.status().await, ServerStatus::Ready { .. }));
    assert_eq!(f.state().await["docs"][f.uri.as_str()], json!({"version": 1, "text": "y"}));
    f.mgr.shutdown().await;
}

fn replace_all(text: &str) -> TextDocumentContentChangeEvent {
    TextDocumentContentChangeEvent { range: None, range_length: None, text: text.into() }
}

#[tokio::test]
async fn restart_on_request_starts_a_new_process_with_open_documents() {
    let Some(mut f) = fixture(&[]) else { return };
    assert!(!f.mgr.restart("fake"), "not started yet");
    f.mgr.open(f.uri.clone(), "fake", 0, "a".into());
    assert!(matches!(f.status().await, ServerStatus::Starting));
    assert!(matches!(f.status().await, ServerStatus::Ready { .. }));
    f.mgr.change(&f.uri, 1, vec![replace_all("b")]);
    let before = f.state().await;

    assert!(f.mgr.restart("fake"));
    let after = f.state().await;
    // No exit is reported: the restart was asked for.
    assert!(matches!(f.status().await, ServerStatus::Starting));
    assert!(matches!(f.status().await, ServerStatus::Ready { .. }));
    assert_ne!(before["pid"], after["pid"]);
    assert_eq!(after["docs"][f.uri.as_str()], json!({"version": 1, "text": "b"}));
    f.mgr.shutdown().await;
}

#[tokio::test]
async fn restart_while_starting_starts_over() {
    let Some(mut f) = fixture(&["--slow-init"]) else { return };
    f.mgr.open(f.uri.clone(), "fake", 0, "x".into());
    assert!(matches!(f.status().await, ServerStatus::Starting));
    assert!(f.mgr.restart("fake"));
    assert!(matches!(f.status().await, ServerStatus::Starting));
    f.mgr.shutdown().await;
}

#[tokio::test]
async fn shutdown_does_not_wait_for_a_starting_server() {
    let Some(mut f) = fixture(&["--slow-init"]) else { return };
    f.mgr.open(f.uri.clone(), "fake", 0, "x".into());
    assert!(matches!(f.status().await, ServerStatus::Starting));
    let started = Instant::now();
    f.mgr.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
}

#[tokio::test]
async fn apply_edit_requests_are_answered_after_the_edits_they_caused() {
    let Some(mut f) = fixture(&[]) else { return };
    f.mgr.open(f.uri.clone(), "fake", 0, "old".into());
    assert!(matches!(f.status().await, ServerStatus::Starting));
    assert!(matches!(f.status().await, ServerStatus::Ready { .. }));
    let state = f.state().await;
    assert_eq!(state["capabilities"]["workspace"]["applyEdit"], json!(true));
    assert_eq!(state["capabilities"]["workspace"]["workspaceEdit"]["documentChanges"], json!(true));

    let edit = json!({"label": "x", "edit": {"changes": {f.uri.as_str(): [
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "new"}
    ]}}});
    f.mgr.request(&f.uri, "fake/applyEdit", edit.clone()).await.unwrap();
    let (id, method, params) = f.server_request().await;
    assert_eq!(method, "workspace/applyEdit");
    assert_eq!(params, edit);

    // The editor applies the edit, then answers: the server sees the change first.
    f.mgr.change(&f.uri, 1, vec![replace_all("new")]);
    assert!(f.mgr.reply(id, Ok(json!({"applied": true}))));
    assert!(!f.mgr.reply(id, Ok(json!({"applied": true}))), "answered once");
    let replies = f.state().await["replies"].clone();
    assert_eq!(replies, json!([{"id": "edit-0", "result": {"applied": true}, "error": null, "versions": {f.uri.as_str(): 1}}]));

    // Errors are answered as JSON-RPC errors.
    f.mgr.request(&f.uri, "fake/applyEdit", edit.clone()).await.unwrap();
    let (id, _, _) = f.server_request().await;
    assert!(f.mgr.reply(id, Err("no".into())));
    let replies = f.state().await["replies"].clone();
    assert_eq!(replies[1]["error"], json!({"code": -32603, "message": "no"}));

    // A request of a process that has gone away cannot be answered.
    f.mgr.request(&f.uri, "fake/applyEdit", edit).await.unwrap();
    let (id, _, _) = f.server_request().await;
    assert!(f.mgr.restart("fake"));
    assert!(matches!(f.status().await, ServerStatus::Starting));
    assert!(matches!(f.status().await, ServerStatus::Ready { .. }));
    assert!(!f.mgr.reply(id, Ok(json!({"applied": false}))));
    assert_eq!(f.state().await["replies"], json!([]), "a new process");
    f.mgr.shutdown().await;
}
