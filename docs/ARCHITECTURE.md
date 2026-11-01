# RustCode architecture

## Overview

```
┌──────────────────────── Web UI (TypeScript, Monaco) ────────────────────────┐
│ Explorer/Search │ EditorGroup (tabs + one Monaco editor) │ StatusBar │ LSP  │
└──────────────┬──────────────────────────────────────────────▲───────────────┘
   invoke (commands, JSON)          emit "lsp-event", "fs-event", "search-event"
┌──────────────▼──────────────── src-tauri ────────────────────┴──────────────┐
│ AppState → Session { root, DocumentStore, Arc<LspManager>, watcher, search }│
└──────────────┬───────────────────────────────────────┬──────────────────────┘
        rc-core│                                 rc-lsp│
  DocumentStore, ropes,                    LspManager ─► one task per server
  UTF-16 positions, fs,                    LspClient  ─► stdio JSON-RPC process
  watcher, text search
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
deletes inside a deleted folder are dropped) and skips paths matching
`files.watcherExclude` (default `.git/objects`, `.git/subtree-cache`,
`.hg/store`; absolute patterns match absolute paths, and an excluded folder
hides everything in it). Changing that setting restarts the watcher.
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

## Search

Find in Files follows VS Code, which runs ripgrep. `rc-core::search` turns a
query into the options VS Code passes to `rg` (`getRgArgs`) and walks the
folder with ripgrep's own `ignore` crate:

- Hidden files are searched, symlinks followed, `.gitignore`, `.ignore` and
  `.rgignore` honored (not those of parent folders, not the global one), and
  `files.exclude` and `search.exclude` (see Settings; by default `.git`,
  `node_modules`, `bower_components`, `*.code-search` …) left out. The "Use
  Exclude Settings and Ignore Files" toggle turns all of that off.
- Exclude exceptions (`"!node_modules/mylib": true` in those settings)
  re-include paths that excludes or ignore files leave out. ripgrep does not
  descend into excluded folders, so, like VS Code, a second walk without
  ignore files visits only the folders leading to the exceptions
  (`getExcludeExceptionGlobs`) and reports what the first walk did not.
  Version control folders (`.git`, `.svn`, `.hg`, `.jj`) are only walked
  when an exception names them. A `!` typed in "files to exclude" is matched
  literally: there, as in VS Code, it is not a setting.
- "files to include/exclude" are expanded like VS Code's `QueryBuilder`
  (`*.ts` → `**/*.ts/**`, `**/*.ts`; `./src` anchored at the folder; `.ts` →
  `*.ts`; absolute paths inside the folder), handed to the walk as `rg -g`
  overrides, and every result is checked again with VS Code's glob semantics
  (`QueryGlobTester`), because the `-g` globs can let more through.
- Matching uses the `regex` crate, falling back to `fancy-regex` for
  look-around and backreferences (ripgrep's `--engine auto`). Lines are
  searched one at a time unless the pattern can span lines (Monaco's
  `isMultilineRegexSource`: `\n`, `\r`, `\W` or a typed line break); then the
  whole file is searched with every line break seen as `\n`. Files whose whole
  text cannot match are skipped without the per-line pass. Binary files (a
  NUL byte) are skipped and a UTF-8 BOM is not part of the text.
- Positions are (line, UTF-16 column) like everywhere else. Each match carries
  a one-line preview trimmed like VS Code's (`TextSearchMatch`).

`search_text` takes a search id chosen by the UI and returns at once; results
stream as `search-event` batches (every 50 ms) with that id, followed by
`done` (with `limitHit` at 20 000 matches). Starting a search cancels the
previous one.

Open documents are not read from disk: the walk skips every open document,
`search_text` returns the ones the query covers, and the UI searches their
Monaco models (`model.findMatches`, VS Code's `getOpenEditorResults`), so
unsaved edits are found. While results are shown, an open document is
searched again 250 ms after each edit, so its results follow typing.

Replace uses a port of VS Code's `ReplacePattern` (JavaScript regex
semantics: `$1`, `$0`, `\n`, `\u`/`\U`/`\l`/`\L`, Preserve Case). Like VS Code's
`ReplaceService`, it searches the document's current content again, applies
the replacements as one undo step and saves; a document opened only for the
replace is closed again. Replace All asks first and clears the results.

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

A server that exits on its own is restarted like vscode-languageclient's
`DefaultErrorHandler`: after every close, unless it is the fifth close within
three minutes; then it stays stopped and the UI shows "The … server crashed 5
times in the last 3 minutes. The server will not be restarted." The server task
keeps its mirrors across the restart, so the new process gets `didOpen` for
every open document at its current version, then whatever was queued while it
started (edits, requests). Requests in flight when it crashed fail; the UI drops
the server's diagnostics and providers and registers them again once it is
ready. A server that fails during `initialize` is not restarted (VS Code stops
it too), and shutting down while a server is still starting kills it instead
of waiting for the handshake.

"Restart Language Server" (the editor's command palette, F1, for the active
file's server, or a click on a server in the status bar) does what the
"Restart Server" commands of VS Code's language extensions do: a graceful
`shutdown`/`exit`, then a fresh start with every open document. It also brings
back a server that failed, was not installed or crashed too often; while
stopped, the task keeps its mirrors up to date for that. As in
vscode-languageclient, a manual restart does not reset the crash count.

Events (status, diagnostics, progress, messages) flow to the UI as one
`lsp-event` stream. The UI registers Monaco providers per server once it is
ready, using the server's own completion trigger characters.

## Settings

`rc-core::settings` reads VS Code's own settings files, so projects and habits
set up for VS Code carry over. Like VS Code, three layers are laid over each
other: defaults, VS Code's user settings (`~/.config/Code/User/settings.json`
on Linux, `~/Library/Application Support/Code/…` on macOS, `%APPDATA%\Code\…`
on Windows; the first existing one of VS Code, Insiders, VSCodium and
Code - OSS), then the folder's `.vscode/settings.json`. Object settings are
merged key by key (so `"**/.git": false` shows `.git` again); other settings
are replaced. Files may have comments and trailing commas, and keys may be
dotted or nested.

Settings read, all with VS Code's defaults:

| Setting | Used by |
|---|---|
| `files.exclude`, `search.exclude` | explorer, search |
| `files.watcherExclude` | file watcher |
| `search.useIgnoreFiles`, `search.useParentIgnoreFiles`, `search.useGlobalIgnoreFiles`, `search.followSymlinks` | search walk |
| `search.maxResults` (`null`: no limit) | search, backend and open documents |
| `search.smartCase`, `search.searchOnType`, `search.searchOnTypeDebouncePeriod`, `search.seedWithNearestWord`, `editor.find.seedSearchStringFromSelection` | Search view |

The UI gets its share as `UiSettings` with the workspace info and every
reload. Like VS Code's `QueryBuilder`, the UI resolves smart case into the
query's case sensitivity, so disk and open documents agree. Seeding follows
VS Code's `getSelectionTextFromEditor`: a multi-line selection is joined with
`\n`, an empty one gives the word at the cursor only with
`seedWithNearestWord`, regex mode escapes the text, and nothing is seeded
while the Search view has focus.

Exclude expressions follow `base/common/glob.ts` (`rc-core::glob`): patterns
map to `true`, `false` or a sibling clause (`{ "when": "$(basename).ts" }`,
excluded only when that sibling exists), and `!`-prefixed keys are
exceptions. The ripgrep walk can only apply plain enabled patterns; sibling
clauses and exceptions are applied to every result.

Values of the wrong type and patterns that are not valid globs are left out
and reported; a file that is not valid JSON is ignored as a whole (the other
layers still apply), with a warning naming the file. The folder watcher
notices changes to `.vscode/settings.json`, and a separate watch on the user
settings folder (not recursive, ignoring reads) notices the user file; either
reloads the settings and sends `settingsChanged`, and the explorer lists its
folders again.

The explorer applies `files.exclude` like VS Code (`evaluateExcludeTreeEntry`):
an entry is hidden when it or its folder is excluded and no exception covers
it, but an excluded folder that an exception could match beneath is still
listed, holding only what the exception re-includes.

## UI

An activity bar switches the side bar between the Explorer and the Search
view (clicking the active icon hides the side bar). The Search view keeps its
result list virtualized, so tens of thousands of rows stay cheap.

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

- No terminal, settings UI, multiple editor groups, or `workspace/applyEdit`.
- Settings: only the settings in the table above, from the default VS Code
  profile (no other profiles, `.code-workspace` files or language-specific
  `[lang]` blocks). The explorer does not show excluded files that are open in
  an editor (VS Code does).
- No diff view for save conflicts (VS Code's "Compare").
- Search: no search editor, tree view mode, search history or replace diff
  preview; non-UTF-8 files are searched lossily but cannot be opened.
