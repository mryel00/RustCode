// Checks of the Monaco-free workspace edit helpers.
// Run: node --experimental-strip-types scripts/test-workspace-edit.ts
import assert from 'node:assert/strict';
import type * as L from 'vscode-languageserver-types';
import { isNoResultError, lspErrorMessage, textEditsOf, versionsMatch } from '../src/lsp/workspace-edit.ts';

const range = (l1: number, c1: number, l2: number, c2: number): L.Range => ({
  start: { line: l1, character: c1 },
  end: { line: l2, character: c2 },
});
const edit = (text: string, l = 0): L.TextEdit => ({ range: range(l, 0, l, 1), newText: text });

// `changes` only.
assert.deepEqual(textEditsOf({ changes: { 'file:///a': [edit('x')], 'file:///b': [edit('y')] } }), {
  ok: true,
  files: [
    { uri: 'file:///a', edits: [edit('x')] },
    { uri: 'file:///b', edits: [edit('y')] },
  ],
});

// `documentChanges` win over `changes`; edits of one file are merged, also
// across URI spellings that name the same file.
const key = (uri: string) => uri.replace('%3A', ':');
assert.deepEqual(
  textEditsOf(
    {
      changes: { 'file:///ignored': [edit('no')] },
      documentChanges: [
        { textDocument: { uri: 'file:///c%3A/a', version: 1 }, edits: [edit('1')] },
        { textDocument: { uri: 'file:///b', version: null }, edits: [edit('2')] },
        { textDocument: { uri: 'file:///c:/a', version: 1 }, edits: [edit('3', 2)] },
      ],
    },
    key,
  ),
  {
    ok: true,
    files: [
      { uri: 'file:///c%3A/a', edits: [edit('1'), edit('3', 2)] },
      { uri: 'file:///b', edits: [edit('2')] },
    ],
  },
);

// Resource operations and snippet edits are not supported.
assert.equal(textEditsOf({ documentChanges: [{ kind: 'create', uri: 'file:///n' }] }).ok, false);
const snippet = { range: range(0, 0, 0, 0), snippet: { kind: 'snippet', value: '$1' } } as unknown as L.TextEdit;
assert.equal(textEditsOf({ documentChanges: [{ textDocument: { uri: 'file:///a', version: 1 }, edits: [snippet] }] }).ok, false);
assert.deepEqual(textEditsOf({}), { ok: true, files: [] });

// Versions: only versioned edits of open documents are checked.
const open = new Map([['file:///a', 3]]);
const versioned = (uri: string, version: number | null): L.WorkspaceEdit => ({
  documentChanges: [{ textDocument: { uri, version }, edits: [edit('x')] }],
});
assert.equal(versionsMatch(versioned('file:///a', 3), (u) => open.get(u)), true);
assert.equal(versionsMatch(versioned('file:///a', 2), (u) => open.get(u)), false);
assert.equal(versionsMatch(versioned('file:///a', null), (u) => open.get(u)), true);
assert.equal(versionsMatch(versioned('file:///closed', 9), (u) => open.get(u)), true);
assert.equal(versionsMatch({ changes: { 'file:///a': [edit('x')] } }, (u) => open.get(u)), true);

// Error messages from the backend.
assert.equal(lspErrorMessage('language server error -32602: cannot rename\nhere'), 'cannot rename\nhere');
assert.equal(lspErrorMessage(new Error('language server request timed out: x')), 'language server request timed out: x');
assert.equal(isNoResultError('language server error -32801: content modified'), true);
assert.equal(isNoResultError('language server is not running'), true);
assert.equal(isNoResultError('language server error -32603: oops'), false);

console.log('ok: workspace edit helpers');
