#!/usr/bin/env bash
# Regenerates src/monaco/features.ts from the installed monaco-editor.
set -euo pipefail
cd "$(dirname "$0")/.."
main=node_modules/monaco-editor/esm/vs/editor/editor.main.js
version=$(node -p "require('./node_modules/monaco-editor/package.json').version")
{
  echo "// Generated from monaco-editor $version esm/vs/editor/editor.main.js: every editor"
  echo "// feature, but none of Monaco's built-in language services (TS/CSS/JSON/HTML),"
  echo "// because language intelligence comes from language servers via the Rust backend."
  echo "// Regenerate when upgrading Monaco: see scripts/gen-monaco-features.sh"
  grep -E "^import '" "$main" | grep -v "definitions/" | grep -vE "features/(css|html|json|typescript)/|lsp-client" \
    | sed -E "s#^import '\./#import 'monaco-esm/editor/#; s#^import '\.\./#import 'monaco-esm/#"
  # Syntax highlighting (Monarch) for every language, without the language services.
  grep -E "^import '\./definitions/[^/]+/register\.js';" node_modules/monaco-editor/esm/vs/languages/register.all.js \
    | sed -E "s#^import '\./#import 'monaco-esm/languages/#"
} > src/monaco/features.ts
