// Open documents: one Monaco model per backend document, kept in sync with the
// Rust document store.

import { backend, errorMessage, type ContentChange, type DiskChange, type SaveOutcome } from '../backend';
import { monaco, monacoLanguage } from '../monaco/setup';
import { Emitter } from './events';
import { computeTextUpdate } from './text-update';

/** Runs async tasks one after another. */
class SerialQueue {
  private tail: Promise<unknown> = Promise.resolve();

  run<T>(task: () => Promise<T>): Promise<T> {
    const result = this.tail.then(task);
    this.tail = result.catch(() => undefined);
    return result;
  }

  /** Resolves once everything queued so far has finished. */
  idle(): Promise<void> {
    return this.tail.then(() => undefined);
  }
}

export interface Doc {
  /** URI exactly as the backend spells it; always used when talking to Rust. */
  readonly uri: string;
  readonly path: string;
  readonly languageId: string;
  readonly model: monaco.editor.ITextModel;
  readonly name: string;
  /** Document version as last sent to the backend. */
  version: number;
  savedAltVersionId: number;
  readonly sync: SerialQueue;
  /** The file was deleted on disk; the document stays open. */
  orphaned: boolean;
  /** Reloads from disk, one at a time. */
  readonly reloads: SerialQueue;
  reloadQueued: boolean;
}

/**
 * Brings `model` to `text` with one minimal edit, so cursors, folding and
 * undo history survive (VS Code's `ModelService.updateModel`).
 */
export function updateModelText(model: monaco.editor.ITextModel, text: string): void {
  const update = computeTextUpdate(model.getLinesContent(), model.getEOL(), text);
  if (!update.eol && !update.edit) return;
  model.pushStackElement();
  if (update.eol) {
    model.pushEOL(update.eol === '\r\n' ? monaco.editor.EndOfLineSequence.CRLF : monaco.editor.EndOfLineSequence.LF);
  }
  if (update.edit) {
    model.pushEditOperations([], [{ ...update.edit, forceMoveMarkers: true }], () => null);
  }
  model.pushStackElement();
}

/** Normalizes URI spellings (e.g. %3A vs :) so lookups match across sources. */
export function uriKey(uri: string | monaco.Uri): string {
  return (typeof uri === 'string' ? monaco.Uri.parse(uri) : uri).toString();
}

export class Documents {
  private readonly docs = new Map<string, Doc>();
  private readonly opening = new Map<string, Promise<Doc>>();
  readonly onDidOpen = new Emitter<Doc>();
  readonly onDidClose = new Emitter<Doc>();
  readonly onDidChangeDirty = new Emitter<Doc>();
  readonly onDidChangeOrphaned = new Emitter<Doc>();
  readonly onError = new Emitter<string>();

  get(uri: string | monaco.Uri): Doc | undefined {
    return this.docs.get(uriKey(uri));
  }

  getByModel(model: monaco.editor.ITextModel): Doc | undefined {
    const doc = this.docs.get(uriKey(model.uri));
    return doc?.model === model ? doc : undefined;
  }

  all(): Doc[] {
    return [...this.docs.values()];
  }

  isDirty(doc: Doc): boolean {
    return doc.model.getAlternativeVersionId() !== doc.savedAltVersionId;
  }

  /** Opens a document by path or URI, or returns it if it is already open. */
  async open(target: { path: string } | { uri: string }): Promise<Doc> {
    if ('uri' in target) {
      const existing = this.get(target.uri);
      if (existing) return existing;
    }
    const pendingKey = 'uri' in target ? uriKey(target.uri) : `path:${target.path}`;
    const pending = this.opening.get(pendingKey);
    if (pending) return pending;

    const promise = (async () => {
      const opened = await backend.openDocument(target);
      const existing = this.get(opened.uri);
      if (existing) return existing;
      const monacoUri = monaco.Uri.parse(opened.uri);
      monaco.editor.getModel(monacoUri)?.dispose();
      const model = monaco.editor.createModel(opened.text, monacoLanguage(opened.languageId), monacoUri);
      const doc: Doc = {
        uri: opened.uri,
        path: opened.path,
        languageId: opened.languageId,
        name: opened.path.split(/[\\/]/).pop() ?? opened.path,
        model,
        version: opened.version,
        savedAltVersionId: model.getAlternativeVersionId(),
        sync: new SerialQueue(),
        orphaned: false,
        reloads: new SerialQueue(),
        reloadQueued: false,
      };
      model.onDidChangeContent((e) => this.handleChange(doc, e));
      this.docs.set(uriKey(opened.uri), doc);
      this.onDidOpen.fire(doc);
      return doc;
    })();
    this.opening.set(pendingKey, promise);
    try {
      return await promise;
    } finally {
      this.opening.delete(pendingKey);
    }
  }

  /**
   * Saves the document. Resolves to `conflict` without writing when the file
   * changed on disk since it was loaded or last saved, unless `overwrite`.
   */
  async save(doc: Doc, overwrite = false): Promise<SaveOutcome> {
    const savedAlt = doc.model.getAlternativeVersionId();
    // Saving goes through the queue so every edit typed so far reaches disk.
    const outcome = await doc.sync.run(() => backend.saveDocument(doc.uri, overwrite));
    if (outcome === 'saved') {
      doc.savedAltVersionId = savedAlt;
      this.setOrphaned(doc, false);
      this.onDidChangeDirty.fire(doc);
    }
    return outcome;
  }

  /**
   * Replaces the document's contents with its file on disk (VS Code's
   * "Revert File"). With `onlyIfClean`, gives up if the document has unsaved
   * changes by the time the file is read. Resolves to whether it reverted.
   */
  async revert(doc: Doc, { onlyIfClean = false } = {}): Promise<boolean> {
    if (onlyIfClean && this.isDirty(doc)) return false;
    const read = await backend.readDocumentFromDisk(doc.uri);
    if (doc.model.isDisposed() || this.get(doc.uri) !== doc) return false;
    if (onlyIfClean && this.isDirty(doc)) return false;
    // The edits travel to the backend through the sync queue like typing does;
    // the read is accepted after them, so the backend knows the buffer and the
    // file agree again.
    updateModelText(doc.model, read.text);
    doc.savedAltVersionId = doc.model.getAlternativeVersionId();
    this.setOrphaned(doc, false);
    this.onDidChangeDirty.fire(doc);
    await doc.sync.run(() => backend.acceptDiskRead(doc.uri, read.readId));
    return true;
  }

  /** Applies disk changes reported by the file watcher. */
  handleDiskChanges(changes: DiskChange[]): void {
    for (const change of changes) {
      const doc = this.get(change.uri);
      if (!doc) continue;
      this.setOrphaned(doc, !change.exists);
      // Like VS Code: documents without unsaved changes follow the disk;
      // modified ones keep their changes and saving reports the conflict.
      if (change.exists && change.contentChanged && !this.isDirty(doc)) this.scheduleReload(doc);
    }
  }

  private scheduleReload(doc: Doc): void {
    if (doc.reloadQueued) return;
    doc.reloadQueued = true;
    void doc.reloads
      .run(() => {
        doc.reloadQueued = false;
        return this.revert(doc, { onlyIfClean: true });
      })
      .catch((err) => this.onError.fire(`Could not reload ${doc.name} from disk: ${errorMessage(err)}`));
  }

  private setOrphaned(doc: Doc, orphaned: boolean): void {
    if (doc.orphaned === orphaned) return;
    doc.orphaned = orphaned;
    this.onDidChangeOrphaned.fire(doc);
  }

  async close(doc: Doc): Promise<void> {
    this.docs.delete(uriKey(doc.uri));
    await doc.sync.run(() => backend.closeDocument(doc.uri)).catch((e) => this.onError.fire(errorMessage(e)));
    this.onDidClose.fire(doc);
    doc.model.dispose();
  }

  private handleChange(doc: Doc, e: monaco.editor.IModelContentChangedEvent): void {
    // Monaco lists the changes of one event so that applying them in order is
    // correct, which is exactly how the backend applies them.
    const changes: ContentChange[] =
      e.isFlush || e.changes.length === 0
        ? [{ text: doc.model.getValue() }]
        : e.changes.map((c) => ({
            range: {
              start: { line: c.range.startLineNumber - 1, character: c.range.startColumn - 1 },
              end: { line: c.range.endLineNumber - 1, character: c.range.endColumn - 1 },
            },
            text: c.text,
          }));
    const version = ++doc.version;
    doc.sync.run(() => backend.changeDocument(doc.uri, version, changes)).catch((err) => {
      this.onError.fire(`Sync failed for ${doc.name}: ${errorMessage(err)}. Resynchronizing.`);
      this.resync(doc);
    });
    this.onDidChangeDirty.fire(doc);
  }

  /** Replaces the backend's copy with the model's full text. */
  private resync(doc: Doc): void {
    if (doc.model.isDisposed()) return;
    const version = ++doc.version;
    doc.sync
      .run(() => backend.changeDocument(doc.uri, version, [{ text: doc.model.getValue() }]))
      .catch((err) => this.onError.fire(`Could not resynchronize ${doc.name}: ${errorMessage(err)}`));
  }
}
