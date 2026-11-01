# RustCode

A code editor in the spirit of VS Code, with its core written in Rust.
Tauri desktop shell, Monaco editor UI, language intelligence via the Language Server Protocol.

**Milestone 1:** open a folder, browse files, edit with tabs and syntax
highlighting, save, plus LSP completion, hover, go-to-definition and diagnostics.

**File watching:** external changes reload open files without unsaved changes, deleted
files are marked in their tab, saving over a newer file asks before overwriting, the
explorer updates itself, and language servers get `workspace/didChangeWatchedFiles`.

**Search:** Find in Files and Replace in Files as in VS Code: match case, whole word,
regex (with look-around), preserve case, files to include/exclude, `.gitignore` and the
default excludes, unsaved changes in open files, results that follow your edits, and
replace one match, one file or everything.

**Settings:** read from VS Code's own files: your VS Code user settings, then the
folder's `.vscode/settings.json`, and they reload when either changes. Supported:
`files.exclude` and `search.exclude` (with `!` exclude exceptions such as
`"!node_modules/mylib": true` and `when` sibling clauses), `files.watcherExclude`,
the `search.*` ignore-file, symlink, smart case, max results, search-on-type and
seeding settings, and `editor.find.seedSearchStringFromSelection`. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#settings).

## Requirements

- Rust 1.88+ (edition 2024) and Node.js 20+
- Tauri system libraries: see <https://v2.tauri.app/start/prerequisites/>
  (Linux: `libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev`)
- Language servers on `PATH` for the languages you use:

| Language | Server | Install |
|---|---|---|
| Rust | rust-analyzer | `rustup component add rust-analyzer rust-src` |
| TypeScript / JavaScript | typescript-language-server | `npm i -g typescript typescript-language-server` |
| Python | pyright | `npm i -g pyright` |
| C / C++ | clangd | your package manager |
| Go | gopls | `go install golang.org/x/tools/gopls@latest` |

Missing servers are shown in the status bar; the editor still works without them.

## Run

```sh
npm install
npx tauri dev                     # dev mode with hot reload
npx tauri dev -- -- ~/my-proj     # open a folder at startup
npx tauri build                   # release build + installers
```

Shortcuts: `Ctrl+O` open folder, `Ctrl+S` save, `Ctrl+W` close tab, `F12` go to definition,
`Ctrl+Space` suggest, `Ctrl+Click` peek/go to definition, `Ctrl+Shift+F` search,
`Ctrl+Shift+H` replace, `F4`/`Shift+F4` next/previous result, `Ctrl+Shift+E` explorer.

## Test

```sh
cargo test --workspace   # includes an end-to-end test against a real rust-analyzer
npm test                 # UI unit tests
npm run build            # type-checks and bundles the UI
```

## Layout

```
crates/rc-core   text model (ropes, UTF-16 positions), documents, file system, watcher,
                 search, globs and exclude expressions, workspace settings
crates/rc-lsp    LSP framing, client, per-language server manager
src-tauri        Tauri app: commands + event bridge between UI and the crates
src              UI: Monaco setup, workbench (explorer, tabs, status bar), LSP adapters
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the pieces fit together.

## License

MIT, see [LICENSE](LICENSE).
