// Search results and replace, modelled on VS Code's SearchModel/SearchResult.
//
// The backend searches the files on disk. Open documents are searched in
// their Monaco models instead (VS Code's `getOpenEditorResults`), and the
// results of every open document follow its edits (VS Code binds file
// matches to models). Replacing edits the documents and saves them, like
// VS Code's ReplaceService.

import { backend, errorMessage, type FileMatches, type SearchEvent, type TextMatch, type TextSearchQuery } from '../../backend';
import { monaco } from '../../monaco/setup';
import { uriKey, type Doc, type Documents } from '../documents';
import { Emitter } from '../events';
import { charsPerLine, comparePaths, makePreview, matchId, matchText } from './matches';
import { isMultilineQuery, ReplacePattern } from './replace-pattern';

/** Monaco's default `editor.wordSeparators`, used by whole-word search in models. */
const WORD_SEPARATORS = '`~!@#$%^&*()-=+[{]}\\|;:\'",.<>/?';
const MAX_MATCH_TEXT = 10000;
/** VS Code's delay before re-searching an edited model. */
const LIVE_UPDATE_DELAY = 250;

const MATCH_DECORATION: monaco.editor.IModelDecorationOptions = {
  stickiness: monaco.editor.TrackedRangeStickiness.NeverGrowsWhenTypingAtEdges,
  zIndex: 12,
  className: 'findMatch',
  overviewRuler: { color: { id: 'editorOverviewRuler.findMatchForeground' }, position: monaco.editor.OverviewRulerLane.Center },
};
const CURRENT_MATCH_DECORATION: monaco.editor.IModelDecorationOptions = {
  ...MATCH_DECORATION,
  zIndex: 13,
  className: 'currentFindMatch',
};

export interface FileResult {
  readonly key: string;
  /** URI as the backend spells it. */
  readonly uri: string;
  readonly path: string;
  /** Path relative to the folder, with `/`. */
  readonly relPath: string;
  readonly name: string;
  /** Folder part of `relPath` ('' at the top level). */
  readonly dir: string;
  matches: TextMatch[];
  collapsed: boolean;
}

export type SearchState =
  | { kind: 'idle' }
  | { kind: 'searching' }
  | { kind: 'done'; limitHit: boolean; cancelled: boolean }
  | { kind: 'error'; message: string };

export interface SearchHost {
  readonly docs: Documents;
  /** Saves a document after a replace, resolving conflicts with the user. */
  save(doc: Doc): Promise<void>;
  /** Whether a document is shown in an editor tab. */
  isShown(doc: Doc): boolean;
}

export interface ReplaceAllResult {
  matches: number;
  files: number;
  errors: string[];
}

export class SearchModel {
  readonly onDidChangeResults = new Emitter<void>();
  readonly onDidChangeState = new Emitter<SearchState>();
  query: TextSearchQuery | null = null;
  state: SearchState = { kind: 'idle' };

  private root: string | null = null;
  private readonly files = new Map<string, FileResult>();
  /** Dismissed matches by file key; they stay hidden when results update. */
  private readonly removed = new Map<string, Set<string>>();
  private readonly dismissedFiles = new Set<string>();
  /** Open documents covered by the query: their results follow their edits. */
  private readonly live = new Set<string>();
  private searchId = 0;
  private maxResults: number | null = 20000;
  private modelLimitHit = false;
  private readonly timers = new Map<string, ReturnType<typeof setTimeout>>();
  private readonly listeners = new Map<Doc, monaco.IDisposable>();
  private readonly decorations = new Map<Doc, string[]>();
  private highlights = false;
  private current: { key: string; id: string } | null = null;

  private replacePattern: ReplacePattern | null = null;
  private replaceValue = '';
  private preserveCase = false;

  constructor(private readonly host: SearchHost) {
    host.docs.all().forEach((doc) => this.attach(doc));
    host.docs.onDidOpen.on((doc) => {
      this.attach(doc);
      // A file in the results now has a model: follow it from now on.
      if (this.files.has(uriKey(doc.uri))) this.schedule(doc, 0);
    });
    host.docs.onDidClose.on((doc) => this.detach(doc));
    void backend.onSearchEvent((event) => this.handleEvent(event));
  }

  /** `search.maxResults` (`null`: no limit). */
  setMaxResults(maxResults: number | null): void {
    this.maxResults = maxResults;
  }

  setRoot(root: string | null): void {
    this.root = root;
    this.clear();
  }

  /** Starts a search, replacing the current results. */
  async search(query: TextSearchQuery): Promise<void> {
    this.reset();
    const id = this.searchId;
    this.query = query;
    this.compileReplacePattern();
    this.setState({ kind: 'searching' });
    this.changed();
    let openDocuments: string[];
    try {
      ({ openDocuments } = await backend.searchText(id, query));
    } catch (err) {
      if (id === this.searchId) this.setState({ kind: 'error', message: errorMessage(err) });
      return;
    }
    if (id !== this.searchId) return;
    for (const uri of openDocuments) {
      const doc = this.host.docs.get(uri);
      if (!doc) continue;
      this.live.add(uriKey(doc.uri));
      this.updateFromModel(doc);
    }
    if (this.state.kind === 'done' && this.modelLimitHit && !this.state.limitHit) {
      this.setState({ ...this.state, limitHit: true });
    }
    this.changed();
  }

  /** Stops a running search, keeping the results found so far. */
  cancel(): void {
    if (this.state.kind !== 'searching') return;
    void backend.cancelSearch(this.searchId).catch(() => undefined);
    this.setState({ kind: 'done', limitHit: this.modelLimitHit, cancelled: true });
  }

  /** Clears the results and forgets the query. */
  clear(): void {
    this.reset();
    this.query = null;
    this.replacePattern = null;
    this.setState({ kind: 'idle' });
    this.changed();
  }

  private reset(): void {
    if (this.state.kind === 'searching') void backend.cancelSearch(this.searchId).catch(() => undefined);
    this.searchId++;
    this.files.clear();
    this.removed.clear();
    this.dismissedFiles.clear();
    this.live.clear();
    this.modelLimitHit = false;
    this.current = null;
    for (const timer of this.timers.values()) clearTimeout(timer);
    this.timers.clear();
    for (const doc of [...this.decorations.keys()]) this.updateDecorations(doc);
  }

  private handleEvent(event: SearchEvent): void {
    if (event.searchId !== this.searchId || this.state.kind !== 'searching') return;
    if (event.kind === 'results') {
      for (const file of event.files) this.addDiskResult(file);
      this.changed();
    } else {
      this.setState({ kind: 'done', limitHit: event.limitHit || this.modelLimitHit, cancelled: event.cancelled });
    }
  }

  private addDiskResult(found: FileMatches): void {
    const key = uriKey(found.uri);
    if (this.dismissedFiles.has(key)) return;
    const doc = this.host.docs.get(found.uri);
    if (doc) {
      // Opened while the search ran: the model is the truth.
      this.live.add(key);
      this.updateFromModel(doc);
      return;
    }
    const removed = this.removed.get(key);
    const matches = removed ? found.matches.filter((m) => !removed.has(matchId(m))) : found.matches;
    if (matches.length > 0) this.files.set(key, this.makeFile(key, found.uri, found.path, matches));
  }

  // ---- Results

  /** Files with results, in VS Code's order. */
  results(): FileResult[] {
    return [...this.files.values()].sort((a, b) => comparePaths(a.relPath, b.relPath));
  }

  get fileCount(): number {
    return this.files.size;
  }

  get matchCount(): number {
    let count = 0;
    for (const file of this.files.values()) count += file.matches.length;
    return count;
  }

  get hasResults(): boolean {
    return this.files.size > 0;
  }

  /** Hides a match, or a whole file, from the results (VS Code's "Dismiss"). */
  dismiss(file: FileResult, match?: TextMatch): void {
    if (match) {
      const id = matchId(match);
      let removed = this.removed.get(file.key);
      if (!removed) this.removed.set(file.key, (removed = new Set()));
      removed.add(id);
      file.matches = file.matches.filter((m) => matchId(m) !== id);
      if (file.matches.length === 0) this.files.delete(file.key);
    } else {
      this.files.delete(file.key);
      this.dismissedFiles.add(file.key);
      this.live.delete(file.key);
    }
    this.refreshDecorations(file.key);
    this.changed();
  }

  setCollapsed(file: FileResult, collapsed: boolean): void {
    file.collapsed = collapsed;
    this.changed();
  }

  setAllCollapsed(collapsed: boolean): void {
    for (const file of this.files.values()) file.collapsed = collapsed;
    this.changed();
  }

  /** The match after (or before) `from` in result order, wrapping around. */
  neighbor(from: { file: FileResult; match: TextMatch } | null, direction: 1 | -1): { file: FileResult; match: TextMatch } | null {
    const all = this.results().flatMap((file) => file.matches.map((match) => ({ file, match })));
    if (all.length === 0) return null;
    const fromId = from && matchId(from.match);
    const index = from ? all.findIndex((e) => e.file.key === from.file.key && matchId(e.match) === fromId) : -1;
    if (index < 0) return direction === 1 ? all[0] : all[all.length - 1];
    return all[(index + direction + all.length) % all.length];
  }

  /** Marks the match shown in the editor, highlighted differently. */
  setCurrent(file: FileResult | null, match: TextMatch | null): void {
    const previous = this.current?.key;
    this.current = file && match ? { key: file.key, id: matchId(match) } : null;
    if (previous) this.refreshDecorations(previous);
    if (this.current && this.current.key !== previous) this.refreshDecorations(this.current.key);
  }

  isCurrent(file: FileResult, match: TextMatch): boolean {
    return this.current?.key === file.key && this.current.id === matchId(match);
  }

  /** Shows result highlights in editors (while the Search view is visible). */
  setHighlights(on: boolean): void {
    if (this.highlights === on) return;
    this.highlights = on;
    for (const doc of this.host.docs.all()) this.updateDecorations(doc);
  }

  // ---- Replace

  /** The replace string and options. */
  setReplace(value: string, preserveCase: boolean): void {
    this.replaceValue = value;
    this.preserveCase = preserveCase;
    this.compileReplacePattern();
    this.changed();
  }

  /** The replacement shown in the results preview for `match`. */
  previewReplacement(match: TextMatch): string {
    const pattern = this.replacePattern;
    if (!pattern) return this.replaceValue;
    return pattern.getReplaceString(matchText(match), this.preserveCase) ?? pattern.pattern;
  }

  get canReplace(): boolean {
    return this.replacePattern !== null && this.state.kind !== 'searching';
  }

  /** Replaces one match, or every match in a file. Resolves to the number replaced. */
  async replace(file: FileResult, match?: TextMatch): Promise<number> {
    const count = await this.replaceInFile(file, match && matchId(match));
    this.changed();
    return count;
  }

  /** Replaces every match in the results, then clears them (like VS Code). */
  async replaceAll(): Promise<ReplaceAllResult> {
    const result: ReplaceAllResult = { matches: 0, files: 0, errors: [] };
    for (const file of this.results()) {
      try {
        const count = await this.replaceInFile(file);
        result.matches += count;
        if (count > 0) result.files++;
      } catch (err) {
        result.errors.push(`${file.name}: ${errorMessage(err)}`);
      }
    }
    this.reset();
    this.setState({ kind: 'done', limitHit: false, cancelled: false });
    this.changed();
    return result;
  }

  /**
   * Replaces the current matches of a file (all of them, or the one with
   * `onlyId`) in its document and saves it. A document that was not open is
   * opened for this and closed again.
   */
  private async replaceInFile(file: FileResult, onlyId?: string): Promise<number> {
    const pattern = this.replacePattern;
    if (!pattern) return 0;
    const docs = this.host.docs;
    const wasOpen = docs.get(file.uri) !== undefined;
    const doc = await docs.open({ uri: file.uri });
    if (this.files.get(file.key) === file) this.live.add(file.key);

    // The document may differ from what was searched: replace what matches now.
    const removed = this.removed.get(file.key);
    const current = this.findInModel(doc.model).filter((m) => !removed?.has(matchId(m)));
    const targets = onlyId ? current.filter((m) => matchId(m) === onlyId) : current;
    if (targets.length > 0) {
      const edits = targets.map((m) => {
        const range = toMonacoRange(m.range);
        return { range, text: this.replaceString(pattern, doc.model, range) };
      });
      doc.model.pushStackElement();
      doc.model.pushEditOperations([], edits, () => null);
      doc.model.pushStackElement();
    }
    if (this.live.has(file.key)) this.updateFromModel(doc);
    if (targets.length > 0) await this.host.save(doc);
    if (!wasOpen && !this.host.isShown(doc) && !docs.isDirty(doc)) await docs.close(doc);
    return targets.length;
  }

  /** VS Code's `Match.replaceString`: the match alone, then with its whole lines as context. */
  private replaceString(pattern: ReplacePattern, model: monaco.editor.ITextModel, range: monaco.Range): string {
    const lf = monaco.editor.EndOfLinePreference.LF;
    const inside = pattern.getReplaceString(model.getValueInRange(range, lf), this.preserveCase);
    if (inside !== null) return inside;
    const lines = new monaco.Range(range.startLineNumber, 1, range.endLineNumber, model.getLineMaxColumn(range.endLineNumber));
    return pattern.getReplaceString(model.getValueInRange(lines, lf), this.preserveCase) ?? pattern.pattern;
  }

  private compileReplacePattern(): void {
    const q = this.query;
    try {
      this.replacePattern = q
        ? new ReplacePattern(this.replaceValue, {
            pattern: q.pattern,
            isRegExp: q.isRegex,
            isCaseSensitive: q.isCaseSensitive,
            isWordMatch: q.isWordMatch,
            isMultiline: isMultilineQuery(q.pattern, q.isRegex),
          })
        : null;
    } catch {
      this.replacePattern = null;
    }
  }

  // ---- Open documents

  private attach(doc: Doc): void {
    if (this.listeners.has(doc)) return;
    this.listeners.set(
      doc,
      doc.model.onDidChangeContent(() => {
        const key = uriKey(doc.uri);
        if (this.query && (this.live.has(key) || this.files.has(key))) this.schedule(doc, LIVE_UPDATE_DELAY);
      }),
    );
  }

  private detach(doc: Doc): void {
    this.listeners.get(doc)?.dispose();
    this.listeners.delete(doc);
    this.decorations.delete(doc);
    const key = uriKey(doc.uri);
    const timer = this.timers.get(key);
    if (timer !== undefined) clearTimeout(timer);
    this.timers.delete(key);
  }

  private schedule(doc: Doc, delay: number): void {
    const key = uriKey(doc.uri);
    const existing = this.timers.get(key);
    if (existing !== undefined) clearTimeout(existing);
    this.timers.set(
      key,
      setTimeout(() => {
        this.timers.delete(key);
        if (this.host.docs.get(doc.uri) !== doc || doc.model.isDisposed() || !this.query) return;
        if (this.dismissedFiles.has(key)) return;
        this.live.add(key);
        this.updateFromModel(doc);
        this.changed();
      }, delay),
    );
  }

  /** Recomputes a document's results from its model. */
  private updateFromModel(doc: Doc): void {
    const key = uriKey(doc.uri);
    const removed = this.removed.get(key);
    const matches = this.findInModel(doc.model).filter((m) => !removed?.has(matchId(m)));
    const existing = this.files.get(key);
    if (matches.length === 0) {
      this.files.delete(key);
    } else if (existing) {
      existing.matches = matches;
    } else {
      this.files.set(key, this.makeFile(key, doc.uri, doc.path, matches));
    }
    this.updateDecorations(doc);
  }

  /** Searches a model like VS Code does for open editors (`model.findMatches`). */
  private findInModel(model: monaco.editor.ITextModel): TextMatch[] {
    const q = this.query;
    if (!q || !q.pattern) return [];
    // `search.maxResults`; the backend applies the same setting on disk.
    const max = this.maxResults ?? Number.MAX_SAFE_INTEGER - 1;
    let found: monaco.editor.FindMatch[];
    try {
      found = model.findMatches(
        q.pattern,
        false,
        q.isRegex,
        q.isCaseSensitive,
        q.isWordMatch ? WORD_SEPARATORS : null,
        false,
        max + 1,
      );
    } catch {
      return [];
    }
    if (found.length > max) {
      this.modelLimitHit = true;
      found = found.slice(0, max);
    }
    const perLine = charsPerLine(q.isRegex);
    return found.map(({ range }) => {
      const line = model.getLineContent(range.startLineNumber);
      const start = range.startColumn - 1;
      const singleLine = range.startLineNumber === range.endLineNumber;
      const match: TextMatch = {
        range: {
          start: { line: range.startLineNumber - 1, character: start },
          end: { line: range.endLineNumber - 1, character: range.endColumn - 1 },
        },
        preview: makePreview(line, start, singleLine ? range.endColumn - 1 : line.length, perLine),
      };
      if (!singleLine) match.text = model.getValueInRange(range).slice(0, MAX_MATCH_TEXT);
      return match;
    });
  }

  // ---- Decorations

  private refreshDecorations(key: string): void {
    const doc = this.host.docs.all().find((d) => uriKey(d.uri) === key);
    if (doc) this.updateDecorations(doc);
  }

  private updateDecorations(doc: Doc): void {
    if (doc.model.isDisposed()) return;
    const key = uriKey(doc.uri);
    const file = this.highlights ? this.files.get(key) : undefined;
    const next = (file?.matches ?? []).map((m) => ({
      range: toMonacoRange(m.range),
      options: this.isCurrent(file!, m) ? CURRENT_MATCH_DECORATION : MATCH_DECORATION,
    }));
    const old = this.decorations.get(doc) ?? [];
    if (old.length === 0 && next.length === 0) return;
    this.decorations.set(doc, doc.model.deltaDecorations(old, next));
  }

  // ---- Helpers

  private makeFile(key: string, uri: string, path: string, matches: TextMatch[]): FileResult {
    const root = this.root;
    let relPath = path;
    if (root && path.length > root.length && path.startsWith(root) && /[\\/]/.test(path[root.length])) {
      relPath = path.slice(root.length + 1);
    }
    relPath = relPath.replace(/\\/g, '/');
    const cut = relPath.lastIndexOf('/');
    return {
      key,
      uri,
      path,
      relPath,
      name: relPath.slice(cut + 1),
      dir: cut < 0 ? '' : relPath.slice(0, cut),
      matches,
      collapsed: false,
    };
  }

  private setState(state: SearchState): void {
    this.state = state;
    this.onDidChangeState.fire(state);
  }

  private changed(): void {
    this.onDidChangeResults.fire();
  }
}

export function toMonacoRange(range: TextMatch['range']): monaco.Range {
  return new monaco.Range(range.start.line + 1, range.start.character + 1, range.end.line + 1, range.end.character + 1);
}

