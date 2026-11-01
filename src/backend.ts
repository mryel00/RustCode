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

export const backend = {
  initialPath: () => invoke<string | null>('initial_path'),
  openWorkspace: (path: string) => invoke<WorkspaceInfo>('open_workspace', { path }),
  listDir: (path: string) => invoke<DirEntry[]>('list_dir', { path }),
  openDocument: (target: { path: string } | { uri: string }) => invoke<OpenedDocument>('open_document', target),
  changeDocument: (uri: string, version: number, changes: ContentChange[]) =>
    invoke<void>('change_document', { uri, version, changes }),
  saveDocument: (uri: string) => invoke<void>('save_document', { uri }),
  closeDocument: (uri: string) => invoke<void>('close_document', { uri }),
  lspRequest: <T>(uri: string, method: string, params: unknown) => invoke<T | null>('lsp_request', { uri, method, params }),
  onLspEvent: (handler: (event: LspEvent) => void): Promise<UnlistenFn> =>
    listen<LspEvent>('lsp-event', (e) => handler(e.payload)),
};

export function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
