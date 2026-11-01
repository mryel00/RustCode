# RustCode

A code editor in the spirit of VS Code, with its core written in Rust.
Tauri desktop shell, Monaco editor UI, language intelligence via the Language Server Protocol.

**Milestone 1 (this version):** open a folder, browse files, edit with tabs and syntax
highlighting, save, plus LSP completion, hover, go-to-definition and diagnostics.

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
`Ctrl+Space` suggest, `Ctrl+Click` peek/go to definition.

## Test

```sh
cargo test --workspace   # includes an end-to-end test against a real rust-analyzer
npm run build            # type-checks and bundles the UI
```

## Layout

```
crates/rc-core   text model (ropes, UTF-16 positions), documents, file system, languages
crates/rc-lsp    LSP framing, client, per-language server manager
src-tauri        Tauri app: commands + event bridge between UI and the crates
src              UI: Monaco setup, workbench (explorer, tabs, status bar), LSP adapters
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the pieces fit together.

## License

MIT, see [LICENSE](LICENSE).
