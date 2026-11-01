// Typed wrappers around the Rust commands in src-tauri/src/lib.rs.

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type * as L from 'vscode-languageserver-types';

export interface WorkspaceInfo {
  root: string;
  name: string;
}

export interface DirEntry {
  name: string;
  path: string;
  isDir: boolean;
}

export interface OpenedDocument {
  uri: string;
  path: string;
  languageId: string;
  text: string;
  version: number;
}

export type FileChangeKind = 'created' | 'changed' | 'deleted';

export interface FsChange {
  path: string;
  kind: FileChangeKind;
}

/** How an open document now relates to its file on disk. */
export interface DiskChange {
  uri: string;
  exists: boolean;
  /** The file differs from what the document was last loaded from or saved to. */
  contentChanged: boolean;
}

export type FsEvent =
  | { kind: 'changes'; changes: FsChange[]; documents: DiskChange[] }
  | { kind: 'rescan'; documents: DiskChange[] }
  | { kind: 'error'; message: string };

export type SaveOutcome = 'saved' | 'conflict';

export interface DiskRead {
  readId: number;
  text: string;
}

export type ContentChange = { range: L.Range; text: string } | { text: string };

export type ServerStatus =
  | { state: 'starting' }
  | { state: 'ready'; name: string; languages: string[]; completionTriggerCharacters: string[] }
  | { state: 'notInstalled'; command: string }
  | { state: 'failed'; message: string }
  | { state: 'exited'; code: number | null };

/** `$/progress` payloads (LSP work done progress). */
export type WorkDoneProgress =
  | { kind: 'begin'; title: string; message?: string; percentage?: number }
  | { kind: 'report'; message?: string; percentage?: number }
  | { kind: 'end'; message?: string };

export type LspEvent =
  | { kind: 'status'; server: string; status: ServerStatus }
  | { kind: 'diagnostics'; server: string; uri: string; version: number | null; diagnostics: L.Diagnostic[] }
  | { kind: 'log'; server: string; level: string; message: string }
  | { kind: 'showMessage'; server: string; level: string; message: string }
  | { kind: 'progress'; server: string; token: string | number; value: WorkDoneProgress };

/** A search as entered in the Search view (rc-core's `TextSearchQuery`). */
export interface TextSearchQuery {
  pattern: string;
  isRegex: boolean;
  isCaseSensitive: boolean;
  isWordMatch: boolean;
  /** "files to include", comma separated. */
  includes: string;
  /** "files to exclude", comma separated. */
  excludes: string;
  useExcludeSettingsAndIgnoreFiles: boolean;
  maxResults?: number;
}

/** Part of a match's first line; `start..end` (UTF-16) is the matched part. */
export interface SearchPreview {
  text: string;
  start: number;
  end: number;
}

export interface TextMatch {
  range: L.Range;
  preview: SearchPreview;
  /** The full matched text, for matches spanning several lines. */
  text?: string;
}

export interface FileMatches {
  uri: string;
  path: string;
  matches: TextMatch[];
}

export type SearchEvent =
  | { kind: 'results'; searchId: number; files: FileMatches[] }
  | { kind: 'done'; searchId: number; limitHit: boolean; cancelled: boolean };

export interface SearchStarted {
  /** Open documents the query covers; the UI searches them in their models. */
  openDocuments: string[];
}

export const backend = {
  initialPath: () => invoke<string | null>('initial_path'),
  openWorkspace: (path: string) => invoke<WorkspaceInfo>('open_workspace', { path }),
  listDir: (path: string) => invoke<DirEntry[]>('list_dir', { path }),
  openDocument: (target: { path: string } | { uri: string }) => invoke<OpenedDocument>('open_document', target),
  changeDocument: (uri: string, version: number, changes: ContentChange[]) =>
    invoke<void>('change_document', { uri, version, changes }),
  saveDocument: (uri: string, overwrite = false) => invoke<SaveOutcome>('save_document', { uri, overwrite }),
  readDocumentFromDisk: (uri: string) => invoke<DiskRead>('read_document_from_disk', { uri }),
  acceptDiskRead: (uri: string, readId: number) => invoke<void>('accept_disk_read', { uri, readId }),
  closeDocument: (uri: string) => invoke<void>('close_document', { uri }),
  lspRequest: <T>(uri: string, method: string, params: unknown) => invoke<T | null>('lsp_request', { uri, method, params }),
  searchText: (searchId: number, query: TextSearchQuery) => invoke<SearchStarted>('search_text', { searchId, query }),
  cancelSearch: (searchId: number) => invoke<void>('cancel_search', { searchId }),
  onSearchEvent: (handler: (event: SearchEvent) => void): Promise<UnlistenFn> =>
    listen<SearchEvent>('search-event', (e) => handler(e.payload)),
  onLspEvent: (handler: (event: LspEvent) => void): Promise<UnlistenFn> =>
    listen<LspEvent>('lsp-event', (e) => handler(e.payload)),
  onFsEvent: (handler: (event: FsEvent) => void): Promise<UnlistenFn> =>
    listen<FsEvent>('fs-event', (e) => handler(e.payload)),
};

export function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
