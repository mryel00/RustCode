// Monaco bootstrap: editor features, syntax highlighting, worker and theme.

import './features';
import * as monaco from 'monaco-esm/editor/editor.api.js';
import EditorWorker from 'monaco-esm/editor/editor.worker.js?worker';
import { IBulkEditService } from 'monaco-esm/editor/browser/services/bulkEditService.js';
import { IEditorWorkerService } from 'monaco-esm/editor/common/services/editorWorker.js';
import {
  StandaloneNotificationService,
  StandaloneServices,
} from 'monaco-esm/editor/standalone/browser/standaloneServices.js';

self.MonacoEnvironment = {
  getWorker: () => new EditorWorker(),
};

/** Options Monaco's rename and code actions pass to the bulk edit service. */
export interface BulkEditOptions {
  editor?: monaco.editor.ICodeEditor;
  label?: string;
  /** Save the changed files if `files.refactoring.autoSave` is on (VS Code). */
  respectAutoSaveConfig?: boolean;
}

export interface BulkEditResult {
  ariaSummary: string;
  isApplied: boolean;
}

export type BulkEditHandler = (
  edits: monaco.languages.WorkspaceEdit | monaco.languages.WorkspaceEdit['edits'],
  options?: BulkEditOptions,
) => Promise<BulkEditResult>;

let bulkEditHandler: BulkEditHandler | undefined;
let notificationHandler: ((message: string, level: 'error' | 'warning' | 'info') => void) | undefined;

/**
 * Where Monaco's rename, code actions and other features apply workspace
 * edits (VS Code's `IBulkEditService`). Monaco's own only edits models that
 * exist and never saves.
 */
export function setBulkEditHandler(handler: BulkEditHandler): void {
  bulkEditHandler = handler;
}

/** Where Monaco's notifications (e.g. "Rename failed to apply edits") go. */
export function setNotificationHandler(handler: (message: string, level: 'error' | 'warning' | 'info') => void): void {
  notificationHandler = handler;
}

class NotificationService extends StandaloneNotificationService {
  override notify(notification: Parameters<StandaloneNotificationService['notify']>[0]) {
    const { message, severity } = notification;
    const text = message instanceof Error ? message.message : String(message);
    // Severity: Ignore 0, Info 1, Warning 2, Error 3.
    const level = severity === 3 ? 'error' : severity === 2 ? 'warning' : 'info';
    if (notificationHandler && text) notificationHandler(text, level);
    return super.notify(notification);
  }
}

const bulkEditService = {
  _serviceBrand: undefined,
  hasPreviewHandler: () => false,
  setPreviewHandler: () => ({ dispose() {} }),
  apply: (edits: Parameters<BulkEditHandler>[0], options?: BulkEditOptions): Promise<BulkEditResult> => {
    if (!bulkEditHandler) return Promise.reject(new Error('workspace edits are not available yet'));
    return bulkEditHandler(edits, options);
  },
};

// Must run before anything else asks Monaco for a service: the first request
// initializes them all and later overrides are ignored.
StandaloneServices.initialize({
  [String(IBulkEditService)]: bulkEditService,
  notificationService: new NotificationService(),
});
if (StandaloneServices.get(IBulkEditService) !== (bulkEditService as unknown)) {
  console.error('Monaco services were initialized too early: workspace edits will not open or save files');
}

monaco.editor.defineTheme('rustcode-dark', {
  base: 'vs-dark',
  inherit: true,
  rules: [],
  colors: {
    'editor.background': '#1f1f1f',
    'editor.lineHighlightBackground': '#ffffff0a',
    'editorGutter.background': '#1f1f1f',
    'editorLineNumber.foreground': '#6e7681',
    'editorLineNumber.activeForeground': '#cccccc',
    'editorWidget.background': '#202020',
    'editorWidget.border': '#454545',
    'editorHoverWidget.background': '#202020',
    'editorSuggestWidget.background': '#202020',
    'editorSuggestWidget.selectedBackground': '#04395e',
  },
});

/**
 * Rewrites edits of a model into smaller ones with the same result (VS Code's
 * `IEditorWorkerService.computeMoreMinimalEdits`), so cursors and
 * selections in unchanged text stay put. Undefined when nothing changes.
 */
export async function computeMoreMinimalEdits(
  uri: monaco.Uri,
  edits: monaco.languages.TextEdit[],
): Promise<monaco.languages.TextEdit[] | undefined> {
  const worker = StandaloneServices.get(IEditorWorkerService) as {
    computeMoreMinimalEdits(uri: monaco.Uri, edits: monaco.languages.TextEdit[], pretty?: boolean): Promise<monaco.languages.TextEdit[] | undefined>;
  };
  return worker.computeMoreMinimalEdits(uri, edits, false);
}

/** Maps an LSP language id to a Monaco language id with a tokenizer, if any. */
export function monacoLanguage(languageId: string): string {
  const aliases: Record<string, string> = {
    typescriptreact: 'typescript',
    javascriptreact: 'javascript',
    shellscript: 'shell',
    jsonc: 'json',
  };
  const id = aliases[languageId] ?? languageId;
  return monaco.languages.getLanguages().some((l) => l.id === id) ? id : 'plaintext';
}

export { monaco };
