use rc_lsp::{LspError, LspManager, ManagerEvent, ServerConfig, ServerStatus};
use serde_json::json;
use tokio::sync::mpsc;

#[tokio::test]
async fn missing_server_reports_not_installed_and_requests_fail_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_core::uri::canonicalize(dir.path()).unwrap();
    let uri = rc_core::uri::path_to_uri(&root.join("a.foo")).unwrap();
    let config = ServerConfig {
        id: "foo-ls".into(),
        command: "rustcode-definitely-not-installed-ls".into(),
        args: vec![],
        languages: vec!["foo".into()],
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mgr = LspManager::new(&root, rc_core::uri::path_to_uri(&root).unwrap(), vec![config], tx);

    assert!(mgr.open(uri.clone(), "foo", 0, "x".into()));
    let err = mgr.request(&uri, "textDocument/hover", json!({})).await.unwrap_err();
    assert!(matches!(err, LspError::Unavailable(_)), "{err:?}");

    let mut statuses = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let ManagerEvent::Status { status, .. } = ev {
            statuses.push(status);
        }
    }
    assert!(matches!(statuses.as_slice(), [ServerStatus::Starting, ServerStatus::NotInstalled { .. }]), "{statuses:?}");

    // Documents without a server resolve to null instead of erroring.
    let other = rc_core::uri::path_to_uri(&root.join("b.txt")).unwrap();
    assert_eq!(mgr.request(&other, "textDocument/hover", json!({})).await.unwrap(), json!(null));
    mgr.shutdown().await;
}
