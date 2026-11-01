// LSP workspace edits, independent of Monaco: their text edits per file and
// the checks vscode-languageclient and VS Code's bulk edit service run on them.

import type * as L from 'vscode-languageserver-types';

/** The text edits of one file. */
export interface FileTextEdits {
  uri: string;
  edits: L.TextEdit[];
}

export type TextEditsResult = { ok: true; files: FileTextEdits[] } | { ok: false; reason: string };

/**
 * The text edits of a workspace edit, grouped by file in order of first
 * appearance. Like vscode-languageclient's `asWorkspaceEdit`, `documentChanges`
 * win over `changes`. Like VS Code's `BulkTextEdits`, all edits of one file
 * apply together, against the file as it is (so several `TextDocumentEdit`s
 * for one file are merged). `key` decides which URIs name the same file.
 * Resource operations are not supported (RustCode does not announce them).
 */
export function textEditsOf(edit: L.WorkspaceEdit, key: (uri: string) => string = (u) => u): TextEditsResult {
  const files = new Map<string, FileTextEdits>();
  const add = (uri: string, edits: L.TextEdit[]) => {
    const k = key(uri);
    const file = files.get(k);
    if (file) file.edits.push(...edits);
    else files.set(k, { uri, edits: [...edits] });
  };
  if (edit.documentChanges) {
    for (const change of edit.documentChanges) {
      if (!('textDocument' in change)) {
        return { ok: false, reason: `Unsupported resource operation '${change.kind}'.` };
      }
      const edits: L.TextEdit[] = [];
      for (const e of change.edits) {
        // Snippet edits (LSP 3.18) are not announced by RustCode.
        if (!('newText' in e)) return { ok: false, reason: 'Unsupported snippet text edit.' };
        edits.push(e);
      }
      add(change.textDocument.uri, edits);
    }
  } else if (edit.changes) {
    for (const [uri, edits] of Object.entries(edit.changes)) add(uri, edits);
  }
  return { ok: true, files: [...files.values()] };
}

/**
 * vscode-languageclient's `validateWorkspaceEdit`: false when a versioned
 * `TextDocumentEdit` targets an open document whose version differs, because
 * the edit was computed for older text. `openVersion` gives an open
 * document's version, `undefined` for documents that are not open.
 */
export function versionsMatch(edit: L.WorkspaceEdit, openVersion: (uri: string) => number | undefined): boolean {
  for (const change of edit.documentChanges ?? []) {
    if (!('textDocument' in change)) continue;
    const version = change.textDocument.version;
    if (version === null || version === undefined || version < 0) continue;
    const current = openVersion(change.textDocument.uri);
    if (current !== undefined && current !== version) return false;
  }
  return true;
}

/**
 * The message of a failed `lsp_request`: the server's own message for an LSP
 * error response (the backend reports `language server error <code>: <message>`).
 */
export function lspErrorMessage(err: unknown): string {
  const text = err instanceof Error ? err.message : String(err);
  const match = /^language server error -?\d+: ([\s\S]*)$/.exec(text);
  return match ? match[1] : text;
}

/** LSP's `ContentModified` error code. */
export const CONTENT_MODIFIED = -32801;

/**
 * Whether a failed `lsp_request` means "no result" to vscode-languageclient's
 * `handleFailedRequest`: the document changed meanwhile (`ContentModified`)
 * or the server went away while answering.
 */
export function isNoResultError(err: unknown): boolean {
  const text = err instanceof Error ? err.message : String(err);
  return text.startsWith(`language server error ${CONTENT_MODIFIED}:`) || text === 'language server is not running';
}
