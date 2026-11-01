// Wires language server results into Monaco: completion, hover, definition,
// rename and diagnostics, registered per server once it reports ready.

import type * as L from 'vscode-languageserver-types';
import {
  backend,
  errorMessage,
  type LspEvent,
  type ServerCapabilities,
  type ServerStatus,
  type WorkDoneProgress,
} from '../backend';
import { monaco, monacoLanguage } from '../monaco/setup';
import { uriKey, type Doc, type Documents } from '../workbench/documents';
import { Emitter } from '../workbench/events';
import {
  applyResolved,
  definitionTargets,
  toCompletionList,
  toHover,
  toLocationLinks,
  toLspPosition,
  toMarker,
  toMonacoRange,
  type LspCompletionItem,
  type LspDefinition,
} from './convert';
import { isNoResultError, lspErrorMessage, textEditsOf, versionsMatch } from './workspace-edit';

export interface ServerState {
  status: ServerStatus;
  /** Active work-done progress, e.g. "Indexing 42%". */
  progress?: string;
}

interface ProgressState {
  title: string;
  message?: string;
  percentage?: number;
}

function formatProgress({ title, message, percentage }: ProgressState): string {
  return [title, message, percentage !== undefined ? `${Math.round(percentage)}%` : undefined]
    .filter(Boolean)
    .join(' · ');
}

export interface ProblemCounts {
  errors: number;
  warnings: number;
}

export class LanguageFeatures {
  readonly servers = new Map<string, ServerState>();
  readonly onDidChangeServers = new Emitter<void>();
  readonly onDidChangeProblems = new Emitter<ProblemCounts>();
  readonly onMessage = new Emitter<{ level: string; message: string }>();
  /** A server asks the editor for something (`workspace/applyEdit`); it must be answered. */
  readonly onServerRequest = new Emitter<{ server: string; id: number; method: string; params: unknown }>();

  private readonly registrations = new Map<string, monaco.IDisposable[]>();
  /** uriKey -> server -> diagnostics; kept for documents that are not open yet. */
  private readonly diagnostics = new Map<string, Map<string, L.Diagnostic[]>>();
  private readonly progress = new Map<string, Map<string | number, ProgressState>>();

  constructor(private readonly docs: Documents) {
    docs.onDidOpen.on((doc) => this.applyMarkers(doc));
  }

  /** Forgets everything from the previous workspace. */
  reset(): void {
    for (const disposables of this.registrations.values()) disposables.forEach((d) => d.dispose());
    this.registrations.clear();
    this.servers.clear();
    this.progress.clear();
    this.diagnostics.clear();
    for (const model of monaco.editor.getModels()) {
      for (const owner of new Set(monaco.editor.getModelMarkers({ resource: model.uri }).map((m) => m.owner))) {
        monaco.editor.setModelMarkers(model, owner, []);
      }
    }
    this.onDidChangeServers.fire();
    this.fireProblems();
  }

  handle(event: LspEvent): void {
    switch (event.kind) {
      case 'status':
        this.handleStatus(event.server, event.status);
        break;
      case 'diagnostics':
        this.handleDiagnostics(event.server, event.uri, event.diagnostics);
        break;
      case 'progress':
        this.handleProgress(event.server, event.token, event.value);
        break;
      case 'showMessage':
        this.onMessage.fire({ level: event.level, message: `${event.server}: ${event.message}` });
        break;
      case 'log':
        if (event.level === 'error') console.warn(`[${event.server}]`, event.message);
        break;
      case 'request':
        this.onServerRequest.fire(event);
        break;
    }
  }

  private handleStatus(server: string, status: ServerStatus): void {
    this.servers.set(server, { status });
    if (status.state === 'ready') {
      this.register(server, status.languages, status.completionTriggerCharacters, status.capabilities);
    } else {
      // Not running (also while restarting): like vscode-languageclient, drop
      // its providers, progress and diagnostics until it is ready again.
      this.registrations.get(server)?.forEach((d) => d.dispose());
      this.registrations.delete(server);
      this.progress.delete(server);
      this.clearDiagnostics(server);
      if (status.state === 'failed') this.onMessage.fire({ level: 'error', message: `${server} failed to start: ${status.message}` });
      // A restart is only logged; giving up is shown (DefaultErrorHandler).
      if (status.state === 'exited' && !status.restarting) this.onMessage.fire({ level: 'error', message: status.message });
    }
    this.onDidChangeServers.fire();
  }

  private handleProgress(server: string, token: string | number, value: WorkDoneProgress): void {
    const tokens = this.progress.get(server) ?? new Map<string | number, ProgressState>();
    this.progress.set(server, tokens);
    if (value.kind === 'end') {
      tokens.delete(token);
    } else if (value.kind === 'begin') {
      tokens.set(token, { title: value.title, message: value.message, percentage: value.percentage });
    } else {
      // A report only carries what changed; keep the rest from earlier.
      const current = tokens.get(token);
      if (!current) return;
      if (value.message !== undefined) current.message = value.message;
      if (value.percentage !== undefined) current.percentage = value.percentage;
    }
    const state = this.servers.get(server);
    if (state) {
      const latest = [...tokens.values()].at(-1);
      state.progress = latest ? formatProgress(latest) : undefined;
      this.onDidChangeServers.fire();
    }
  }

  private handleDiagnostics(server: string, uri: string, diagnostics: L.Diagnostic[]): void {
    const key = uriKey(uri);
    const byServer = this.diagnostics.get(key) ?? new Map<string, L.Diagnostic[]>();
    byServer.set(server, diagnostics);
    this.diagnostics.set(key, byServer);
    const doc = this.docs.get(uri);
    if (doc) this.applyMarkers(doc);
    this.fireProblems();
  }

  private clearDiagnostics(server: string): void {
    let changed = false;
    for (const [key, byServer] of this.diagnostics) {
      if (!byServer.delete(server)) continue;
      changed = true;
      if (byServer.size === 0) this.diagnostics.delete(key);
    }
    for (const model of monaco.editor.getModels()) monaco.editor.setModelMarkers(model, `lsp:${server}`, []);
    if (changed) this.fireProblems();
  }

  private applyMarkers(doc: Doc): void {
    const byServer = this.diagnostics.get(uriKey(doc.uri));
    if (!byServer || doc.model.isDisposed()) return;
    for (const [server, diagnostics] of byServer) {
      monaco.editor.setModelMarkers(doc.model, `lsp:${server}`, diagnostics.map(toMarker));
    }
  }

  private fireProblems(): void {
    const counts = { errors: 0, warnings: 0 };
    for (const byServer of this.diagnostics.values()) {
      for (const list of byServer.values()) {
        for (const d of list) {
          if ((d.severity ?? 1) === 1) counts.errors++;
          else if (d.severity === 2) counts.warnings++;
        }
      }
    }
    this.onDidChangeProblems.fire(counts);
  }

  private register(
    server: string,
    languages: string[],
    triggerCharacters: string[],
    capabilities: ServerCapabilities,
  ): void {
    this.registrations.get(server)?.forEach((d) => d.dispose());
    const selector = [...new Set(languages.map(monacoLanguage))];
    const docs = this.docs;

    /** Runs a request after all pending edits for the document have been sent. */
    const request = async <T>(doc: Doc, method: string, params: unknown): Promise<T | null> => {
      await doc.sync.idle();
      try {
        return await backend.lspRequest<T>(doc.uri, method, params);
      } catch (err) {
        console.warn(`${method} failed:`, errorMessage(err));
        return null;
      }
    };
    const textDocumentPosition = (doc: Doc, position: monaco.Position) => ({
      textDocument: { uri: doc.uri },
      position: toLspPosition(position),
    });

    const disposables = [
      monaco.languages.registerCompletionItemProvider(selector, {
        triggerCharacters,
        async provideCompletionItems(model, position, context, token) {
          const doc = docs.getByModel(model);
          if (!doc) return { suggestions: [] };
          const result = await request<L.CompletionList | L.CompletionItem[]>(doc, 'textDocument/completion', {
            ...textDocumentPosition(doc, position),
            // Monaco's trigger kinds are LSP's minus one.
            context: { triggerKind: context.triggerKind + 1, triggerCharacter: context.triggerCharacter },
          });
          if (token.isCancellationRequested || model.isDisposed()) return { suggestions: [] };
          return toCompletionList(result, model, position, doc.uri);
        },
        async resolveCompletionItem(item, token) {
          const lspItem = item as LspCompletionItem;
          const doc = docs.get(lspItem.docUri);
          if (!doc || !lspItem.lspItem) return item;
          const resolved = await request<L.CompletionItem>(doc, 'completionItem/resolve', lspItem.lspItem);
          if (!resolved || token.isCancellationRequested) return item;
          return applyResolved(lspItem, resolved);
        },
      }),
      monaco.languages.registerHoverProvider(selector, {
        async provideHover(model, position, token) {
          const doc = docs.getByModel(model);
          if (!doc) return null;
          const result = await request<L.Hover>(doc, 'textDocument/hover', textDocumentPosition(doc, position));
          return token.isCancellationRequested ? null : toHover(result);
        },
      }),
      monaco.languages.registerDefinitionProvider(selector, {
        async provideDefinition(model, position, token) {
          const doc = docs.getByModel(model);
          if (!doc) return null;
          const result = await request<LspDefinition>(doc, 'textDocument/definition', textDocumentPosition(doc, position));
          if (token.isCancellationRequested) return null;
          // Monaco can only show locations that have a model, so open targets first.
          await Promise.all(
            [...new Set(definitionTargets(result))].map((uri) => docs.open({ uri }).catch(() => undefined)),
          );
          return toLocationLinks(result, (uri) => docs.get(uri)?.model.uri);
        },
      }),
    ];
    if (capabilities.renameProvider) {
      const prepare = typeof capabilities.renameProvider === 'object' && capabilities.renameProvider.prepareProvider === true;
      disposables.push(monaco.languages.registerRenameProvider(selector, this.renameProvider(prepare)));
    }
    this.registrations.set(server, disposables);
  }

  /**
   * Rename, as vscode-languageclient's rename feature and VS Code's
   * `RenameAdapter` do it: errors become reject reasons shown to the user,
   * and `prepareRename` (when the server has it) decides what can be renamed.
   */
  private renameProvider(prepare: boolean): monaco.languages.RenameProvider {
    const docs = this.docs;
    const provider: monaco.languages.RenameProvider = {
      async provideRenameEdits(model, position, newName, token) {
        const doc = docs.getByModel(model);
        if (!doc) return null;
        let result: L.WorkspaceEdit | null;
        try {
          await doc.sync.idle();
          result = await backend.lspRequest<L.WorkspaceEdit>(doc.uri, 'textDocument/rename', {
            textDocument: { uri: doc.uri },
            position: toLspPosition(position),
            newName,
          });
        } catch (err) {
          if (isNoResultError(err)) return null;
          return { edits: [], rejectReason: lspErrorMessage(err) };
        }
        if (token.isCancellationRequested || !result) return null;
        if (!versionsMatch(result, (uri) => docs.get(uri)?.version)) {
          return {
            edits: [],
            rejectReason: 'The rename edit returned from the server is not valid anymore and cannot be applied.',
          };
        }
        const converted = textEditsOf(result, uriKey);
        if (!converted.ok) return { edits: [], rejectReason: converted.reason };
        const edits: monaco.languages.IWorkspaceTextEdit[] = converted.files.flatMap((file) =>
          file.edits.map((e) => ({
            resource: monaco.Uri.parse(file.uri),
            textEdit: { range: toMonacoRange(e.range), text: e.newText },
            versionId: undefined,
          })),
        );
        return { edits };
      },
    };
    if (prepare) {
      provider.resolveRenameLocation = async (model, position, token) => {
        const doc = docs.getByModel(model);
        if (!doc) return null;
        type Prepared = L.Range | { range: L.Range; placeholder: string } | { defaultBehavior: boolean } | null;
        let result: Prepared;
        try {
          await doc.sync.idle();
          result = await backend.lspRequest<Prepared>(doc.uri, 'textDocument/prepareRename', {
            textDocument: { uri: doc.uri },
            position: toLspPosition(position),
          });
        } catch (err) {
          return { range: emptyRange(position), text: '', rejectReason: lspErrorMessage(err) };
        }
        if (token.isCancellationRequested) return null;
        const cannot = { range: emptyRange(position), text: '', rejectReason: "The element can't be renamed." };
        let range: L.Range;
        let text: string;
        if (result && 'start' in result) {
          range = result;
          text = model.getValueInRange(toMonacoRange(result));
        } else if (result && 'defaultBehavior' in result) {
          // Rename the word at the cursor, as if there were no prepareRename.
          return result.defaultBehavior ? null : cannot;
        } else if (result && 'range' in result) {
          range = result.range;
          text = result.placeholder;
        } else {
          return cannot;
        }
        // VS Code ignores locations without text or not on the cursor's line.
        if (!text || range.start.line > position.lineNumber - 1 || range.end.line < position.lineNumber - 1) return null;
        return { range: toMonacoRange(range), text };
      };
    }
    return provider;
  }
}

function emptyRange(position: monaco.IPosition): monaco.IRange {
  return { startLineNumber: position.lineNumber, startColumn: position.column, endLineNumber: position.lineNumber, endColumn: position.column };
}
