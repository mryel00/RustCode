# RustCode architecture

## Overview

```
┌──────────────────────── Web UI (TypeScript, Monaco) ────────────────────────┐
│ Explorer │ EditorGroup (tabs + one Monaco editor) │ StatusBar │ LSP adapters │
└──────────────┬──────────────────────────────────────────────▲───────────────┘
   invoke (commands, JSON)                          emit "lsp-event"
┌──────────────▼──────────────── src-tauri ────────────────────┴──────────────┐
│ AppState → Session { root, DocumentStore, Arc<LspManager> }                 │
└──────────────┬───────────────────────────────────────┬──────────────────────┘
        rc-core│                                 rc-lsp│
  DocumentStore, ropes,                    LspManager ─► one task per server
  UTF-16 positions, fs                     LspClient  ─► stdio JSON-RPC process
```

The Rust side owns all state that matters: file contents, versions, language
server processes. The UI is a view plus input: it renders documents and turns
keystrokes into edits.

## Text and positions

Monaco, LSP and the backend all address text as **(line, UTF-16 column)**, so
edits cross every boundary unchanged. `rc-core::text` converts those positions
to rope char indices. Line breaks are exactly `\n`, `\r\n` and `\r` (ropey's
Unicode line breaks are disabled) to match LSP and Monaco. Files keep their
line endings and UTF-8 BOM on save; non-UTF-8 files are refused rather than
corrupted.

## Document synchronization

1. Opening a file: `open_document` loads it into the `DocumentStore` (version 0)
   and the UI creates a Monaco model from the returned text.
2. Every Monaco content change becomes `change_document(uri, version, changes)`.
   The UI keeps its own version counter (not Monaco's `versionId`) and sends
   changes through a per-document serial queue, so edits arrive in order.
3. The backend applies the changes to its rope and forwards them to the
   document's language server.
4. Requests (completion, hover, …) wait for the document's queue to drain, so a
   request is never sent before the edit that preceded it. On a failed sync
   the UI resends the full text.

## File watching

Opening a folder starts a recursive watcher (`rc-core::watcher`, built on
`notify`) in the background. Like VS Code it collects raw events for 75 ms,
coalesces them (created+deleted cancel out, deleted+created becomes changed,
deletes inside a deleted folder are dropped) and skips VS Code's default
`files.watcherExclude` (`.git/objects`, `.git/subtree-cache`, `.hg/store`).
Each batch goes to three places:

- **Language servers** get `workspace/didChangeWatchedFiles`, filtered by the
  glob watchers each server registered through `client/registerCapability`
  (`rc-lsp::watched_files`), as vscode-languageclient does.
- **Open documents**: every document records the disk content it was last
  loaded from or saved to (`DiskStamp`: size and mtime as a quick check, a
  content hash to decide). Affected documents are compared with the disk, so
  our own saves and `touch` are not changes. The UI receives
  `{ uri, exists, contentChanged }` per document in the `fs-event` stream.
- **The explorer** relists the loaded folders that gained or lost entries.

What the UI does with document changes follows VS Code:

- no unsaved changes: the document reloads from disk with one minimal edit
  (common leading/trailing lines kept, a port of `ModelService.updateModel`),
  so cursor, scroll position and undo history survive. Reloads go through
  the normal edit path: `read_document_from_disk`, edits through the sync
  queue, then `accept_disk_read` marks the buffer as synced with the disk.
- unsaved changes: nothing happens until save. `save_document` then answers
  `conflict` without writing, and the UI offers Overwrite or Revert.
- file deleted: the document stays open, its tab is struck through and marked
  "(deleted)"; saving recreates the file.

A lost-events signal (kernel queue overflow) triggers a full explorer refresh
and a check of every open document. If the watch cannot be set up (for
example the inotify limit), the UI shows a warning.

## Language servers

`LspManager` maps language ids to `ServerConfig`s (`rc-lsp::config`) and starts
a server lazily the first time a document of that language opens.

Each server runs in a dedicated task that owns its `LspClient` and a **mirror**
rope of every document it has open. All traffic for that server (open, change,
save, close, requests) goes through one ordered channel, which gives two
guarantees:

- commands queued while the server is still starting are delivered in order
  once it is ready;
- for servers that only support full sync, the mirror provides the exact text
  at each version.

The client negotiates UTF-16 positions, answers common server-to-client
requests (`workspace/configuration`, progress creation, capability
registration, keeping file watcher registrations), cancels timed-out requests with `$/cancelRequest`, and shuts
servers down with `shutdown`/`exit` (killing them if they hang). Servers also
exit on their own when the app dies, because their stdin closes.

Events (status, diagnostics, progress, messages) flow to the UI as one
`lsp-event` stream. The UI registers Monaco providers per server once it is
ready, using the server's own completion trigger characters.

## UI

Monaco is loaded without its built-in TypeScript/CSS/JSON/HTML language
services (`src/monaco/features.ts`, generated by
`scripts/gen-monaco-features.sh`), so the language servers are the only source
of language intelligence. Syntax highlighting uses Monaco's Monarch grammars.

## Path to VS Code extensions

Extensions are planned, not built. The design keeps that door open:

- The command/event boundary in `src-tauri/src/lib.rs` is plain JSON in LSP
  shapes. An extension host (Node process speaking a subset of the
  `vscode` API) would attach at the same boundary as the UI: it needs document
  open/change events, the ability to register providers, and the
  `lsp-event`-style notification stream.
- Documents and versions already live in Rust, which is where an extension
  host would read them from.
- Language features are already provider-based on the UI side, so
  extension-contributed providers can be merged with LSP ones.

## Known limitations

- A crashed language server is not restarted automatically (reopen the folder).
- No search, terminal, settings UI, multiple editor groups, or `workspace/applyEdit`.
- `files.watcherExclude` is fixed to VS Code's defaults (no settings yet), and
  there is no diff view for save conflicts (VS Code's "Compare").
