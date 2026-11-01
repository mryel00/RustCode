// Typed wrappers around the Rust commands in src-tauri/src/lib.rs.

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type * as L from 'vscode-languageserver-types';

/** The settings the UI applies (rc-core's `UiSettings`), from VS Code's settings files. */
export interface UiSettings {
  smartCase: boolean;
  /** `null`: no limit. */
  maxResults: number | null;
  searchOnType: boolean;
  searchOnTypeDebouncePeriod: number;
  seedWithNearestWord: boolean;
  seedSearchStringFromSelection: 'never' | 'always' | 'selection';
  /** `files.refactoring.autoSave`. */
  refactoringAutoSave: boolean;
}

/** VS Code's defaults, until a folder's settings are loaded. */
export const DEFAULT_UI_SETTINGS: UiSettings = {
  smartCase: false,
  maxResults: 20000,
  searchOnType: true,
  searchOnTypeDebouncePeriod: 300,
  seedWithNearestWord: false,
  seedSearchStringFromSelection: 'always',
  refactoringAutoSave: true,
};

export interface WorkspaceInfo {
  root: string;
  name: string;
  /** Problems with the settings files. */
  settingsWarnings: string[];
  settings: UiSettings;
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
  | { kind: 'error'; message: string }
  /** A settings file changed and was loaded again (`files.exclude` may hide other files). */
  | { kind: 'settingsChanged'; warnings: string[]; settings: UiSettings };

export type SaveOutcome = 'saved' | 'conflict';

export interface DiskRead {
  readId: number;
  text: string;
}

export type ContentChange = { range: L.Range; text: string } | { text: string };

/** The parts of LSP `ServerCapabilities` the UI looks at. */
export interface ServerCapabilities {
  renameProvider?: boolean | { prepareProvider?: boolean };
}

export type ServerStatus =
  | { state: 'starting' }
  | {
      state: 'ready';
      name: string;
      languages: string[];
      completionTriggerCharacters: string[];
      /** The server's LSP `ServerCapabilities`. */
      capabilities: ServerCapabilities;
    }
  | { state: 'notInstalled'; command: string }
  | { state: 'failed'; message: string }
  /** Exited on its own; `restarting` unless it crashed too often. */
  | { state: 'exited'; code: number | null; restarting: boolean; message: string };

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
  | { kind: 'progress'; server: string; token: string | number; value: WorkDoneProgress }
  /** A request from the server for the editor; answer with `backend.lspReply(id, …)`. */
  | { kind: 'request'; server: string; id: number; method: string; params: unknown };

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
  /** Answers a server request (`LspEvent` of kind `request`) with a result or an error message. */
  lspReply: (id: number, answer: { result: unknown } | { error: string }) =>
    invoke<void>('lsp_reply', { id, ...answer }),
  /** Restarts a server, named or as the server of a document; resolves to its name. */
  restartLanguageServer: (target: { server: string } | { uri: string }) =>
    invoke<string>('restart_language_server', target),
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
