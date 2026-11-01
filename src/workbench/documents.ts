// Open documents: one Monaco model per backend document, kept in sync with the
// Rust document store.

import { backend, errorMessage, type ContentChange } from '../backend';
import { monaco, monacoLanguage } from '../monaco/setup';
import { Emitter } from './events';

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

  async save(doc: Doc): Promise<void> {
    const savedAlt = doc.model.getAlternativeVersionId();
    // Saving goes through the queue so every edit typed so far reaches disk.
    await doc.sync.run(() => backend.saveDocument(doc.uri));
    doc.savedAltVersionId = savedAlt;
    this.onDidChangeDirty.fire(doc);
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
