// Applies workspace edits like VS Code's bulk edit service (`BulkEditService`,
// `BulkTextEdits`): from Monaco's rename and code actions, and from language
// servers (`workspace/applyEdit`, vscode-languageclient's
// `handleApplyWorkspaceEdit`). Files that are not open are opened for the
// edit and stay open in the background while they have unsaved changes.

import type * as L from 'vscode-languageserver-types';
import { computeMoreMinimalEdits, monaco, type BulkEditOptions, type BulkEditResult } from '../monaco/setup';
import { uriKey, type Doc, type Documents } from '../workbench/documents';
import { toMonacoRange } from './convert';
import { textEditsOf, versionsMatch } from './workspace-edit';

/** `workspace/applyEdit` params (LSP `ApplyWorkspaceEditParams`). */
export interface ApplyWorkspaceEditParams {
  label?: string;
  edit: L.WorkspaceEdit;
  metadata?: { isRefactoring?: boolean };
}

export interface ApplyWorkspaceEditResult {
  applied: boolean;
  failureReason?: string;
  failedChange?: number;
}

export interface BulkEditHost {
  /** The editor whose model gets its edits through the editor (cursor-aware). */
  readonly editor: monaco.editor.ICodeEditor;
  isShown(doc: Doc): boolean;
  /** Saves a document, resolving save conflicts with the user. */
  save(doc: Doc): Promise<void>;
  /** `files.refactoring.autoSave`. */
  refactoringAutoSave(): boolean;
}

/** The text edits for one file. `versionId`: the Monaco model version they were computed for. */
interface FileEdits {
  uri: string;
  edits: monaco.languages.TextEdit[];
  versionId?: number;
}

interface Applied {
  /** Documents that changed. */
  changed: Doc[];
  /** Documents that were opened for the edit (and kept open as they changed). */
  openedForEdit: Doc[];
}

/** Runs async tasks one after another. */
class Serial {
  private tail: Promise<unknown> = Promise.resolve();

  run<T>(task: () => Promise<T>): Promise<T> {
    const result = this.tail.then(task);
    this.tail = result.catch(() => undefined);
    return result;
  }
}

export class BulkEdits {
  /** Server edits are converted, checked and applied one after another (vscode-languageclient's lock). */
  private readonly serverEdits = new Serial();

  constructor(
    private readonly docs: Documents,
    private readonly host: BulkEditHost,
  ) {}

  /** VS Code's `IBulkEditService.apply`, for Monaco's features. */
  async apply(
    editsIn: monaco.languages.WorkspaceEdit | monaco.languages.WorkspaceEdit['edits'],
    options: BulkEditOptions = {},
  ): Promise<BulkEditResult> {
    const list = Array.isArray(editsIn) ? editsIn : editsIn.edits;
    if (list.length === 0) return { ariaSummary: 'Made no edits', isApplied: false };
    const files = new Map<string, FileEdits>();
    for (const edit of list) {
      if (!('textEdit' in edit)) throw new Error('Only text edits are supported in workspace edits');
      const key = uriKey(edit.resource);
      let file = files.get(key);
      if (!file) {
        file = { uri: edit.resource.toString(), edits: [], versionId: edit.versionId };
        files.set(key, file);
      }
      file.edits.push({ range: edit.textEdit.range, text: edit.textEdit.text });
    }
    const applied = await this.applyFiles([...files.values()], options.editor ?? this.host.editor);
    // Like VS Code: save what a refactoring changed, if it changed several files.
    if (applied && options.respectAutoSaveConfig) await this.autoSave(applied);
    return { ariaSummary: summary(list.length, files.size), isApplied: true };
  }

  /**
   * Answers `workspace/applyEdit`. Like vscode-languageclient, an edit for an
   * open document whose version changed meanwhile is not applied.
   * Resolves once the changes are on their way to the language servers.
   */
  applyServerEdit(params: ApplyWorkspaceEditParams): Promise<ApplyWorkspaceEditResult> {
    return this.serverEdits.run(async () => {
      const converted = textEditsOf(params.edit, uriKey);
      if (!converted.ok) return { applied: false, failureReason: converted.reason };
      const files = converted.files.map((f) => ({
        uri: f.uri,
        edits: f.edits.map((e) => ({ range: toMonacoRange(e.range), text: e.newText })),
      }));
      // VS Code applies an empty edit as "Made no edits", which is not applied.
      if (files.every((f) => f.edits.length === 0)) return { applied: false };
      const openVersion = (uri: string) => this.docs.get(uri)?.version;
      // Checked against the documents open before the edit: the server cannot
      // know versions of documents opened for it.
      const openBefore = new Set(files.filter((f) => this.docs.get(f.uri)).map((f) => uriKey(f.uri)));
      try {
        const applied = await this.applyFiles(files, this.host.editor, () =>
          versionsMatch(params.edit, (uri) => (openBefore.has(uriKey(uri)) ? openVersion(uri) : undefined)),
        );
        if (!applied) return { applied: false };
        if (params.metadata?.isRefactoring) await this.autoSave(applied);
        return { applied: true };
      } catch (err) {
        console.warn('workspace/applyEdit failed:', err);
        return { applied: false };
      }
    });
  }

  /**
   * Opens the files and applies their edits, each file's as one undo step.
   * `check` runs right before anything is applied; when it fails, nothing is
   * applied and the result is null. Otherwise resolves once the changes are
   * queued for the backend.
   */
  private async applyFiles(
    files: FileEdits[],
    editor: monaco.editor.ICodeEditor,
    check?: () => boolean,
  ): Promise<Applied | null> {
    const wasOpen = new Set(files.filter((f) => this.docs.get(f.uri)).map((f) => uriKey(f.uri)));
    const opened: Doc[] = [];
    try {
      for (const file of files) opened.push(await this.docs.open({ uri: file.uri }));
      // Edits in the editor's model are made minimal, so its cursors stay put.
      const edits = await Promise.all(
        files.map(async (file, i) => {
          const model = opened[i].model;
          const valid = file.edits
            .map((e) => ({ range: model.validateRange(e.range), text: e.text ?? '' }))
            // No-op edits (VS Code's ModelEditTask.addEdit skips them).
            .filter((e) => !(e.range.isEmpty() && e.text === ''));
          if (editor.getModel() !== model || valid.length === 0) return valid;
          const minimal = await computeMoreMinimalEdits(model.uri, valid);
          return (minimal ?? valid).map((e) => ({ range: model.validateRange(e.range), text: e.text ?? '' }));
        }),
      );

      // Nothing awaits from here until the edits are applied, so the documents
      // cannot change between the checks and the edits.
      if (check && !check()) return null;
      for (const [i, file] of files.entries()) {
        const model = opened[i].model;
        if (model.isDisposed() || this.docs.get(file.uri) !== opened[i]) {
          throw new Error(`${file.uri} was closed in the meantime`);
        }
        if (typeof file.versionId === 'number' && model.getVersionId() !== file.versionId) {
          throw new Error(`${file.uri} has changed in the meantime`);
        }
        if (overlapping(edits[i].map((e) => e.range))) throw new Error(`Overlapping edits in ${file.uri}`);
      }
      const changed: Doc[] = [];
      for (const [i, doc] of opened.entries()) {
        const ops = edits[i]
          .map((e) => ({ range: e.range, text: e.text, forceMoveMarkers: true }))
          .sort((a, b) => monaco.Range.compareRangesUsingStarts(a.range, b.range));
        if (ops.length === 0) continue;
        if (editor.getModel() === doc.model) {
          editor.pushUndoStop();
          editor.executeEdits('workspaceEdit', ops);
          editor.pushUndoStop();
        } else {
          doc.model.pushStackElement();
          doc.model.pushEditOperations(null, ops, () => null);
          doc.model.pushStackElement();
        }
        changed.push(doc);
      }
      // A language server waiting for the answer gets the changes first.
      await Promise.all(changed.map((doc) => doc.sync.idle()));
      return { changed, openedForEdit: opened.filter((doc) => !wasOpen.has(uriKey(doc.uri))) };
    } finally {
      // Files opened only for this edit are closed again unless they changed.
      for (const doc of opened) {
        if (!wasOpen.has(uriKey(doc.uri)) && !this.docs.isDirty(doc) && !this.host.isShown(doc)) {
          void this.docs.close(doc);
        }
      }
    }
  }

  /**
   * VS Code's `files.refactoring.autoSave`: when an edit changed more than
   * one file, saves those with unsaved changes (`_saveAll`).
   */
  private async autoSave({ changed, openedForEdit }: Applied): Promise<void> {
    if (!this.host.refactoringAutoSave() || changed.length <= 1) return;
    await Promise.all(changed.filter((d) => this.docs.isDirty(d)).map((d) => this.host.save(d)));
    // Documents opened only for the edit no longer need to stay open once saved.
    for (const doc of openedForEdit) {
      if (!this.docs.isDirty(doc) && !this.host.isShown(doc) && this.docs.get(doc.uri) === doc) void this.docs.close(doc);
    }
  }
}

/** Monaco's rule: sorted by their ends, no range may start before the previous one ends. */
function overlapping(ranges: monaco.Range[]): boolean {
  const sorted = [...ranges].sort(monaco.Range.compareRangesUsingEnds);
  for (let i = 1; i < sorted.length; i++) {
    if (sorted[i].getStartPosition().isBefore(sorted[i - 1].getEndPosition())) return true;
  }
  return false;
}

function summary(edits: number, files: number): string {
  return files === 1 ? `Made ${edits} text edits in one file` : `Made ${edits} text edits in ${files} files`;
}
