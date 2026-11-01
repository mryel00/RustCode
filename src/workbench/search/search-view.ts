// The Search view: VS Code's "Find in Files" side bar view, with replace,
// include/exclude patterns and a virtualized result list.

import type { TextMatch, TextSearchQuery } from '../../backend';
import { Emitter } from '../events';
import {
  matchId,
  previewParts,
  replaceAllConfirmation,
  replaceAllDone,
  resultCountMessage,
} from './matches';
import type { FileResult, SearchModel } from './search-model';

const ROW_HEIGHT = 22;
/** VS Code's `search.searchOnTypeDebouncePeriod`. */
const SEARCH_ON_TYPE_DELAY = 300;

type Row = { kind: 'file'; file: FileResult } | { kind: 'match'; file: FileResult; match: TextMatch };

export interface OpenMatchEvent {
  file: FileResult;
  match: TextMatch;
  preserveFocus: boolean;
}

export interface SearchViewHost {
  confirm(message: string, okLabel: string): Promise<boolean>;
  report(message: string): void;
}

interface Toggle {
  el: HTMLButtonElement;
  checked: boolean;
}

export class SearchView {
  readonly onOpenMatch = new Emitter<OpenMatchEvent>();
  readonly onOpenFile = new Emitter<FileResult>();
  readonly onOpenFolder = new Emitter<void>();

  private readonly searchInput: HTMLTextAreaElement;
  private readonly replaceInput: HTMLTextAreaElement;
  private readonly includeInput: HTMLInputElement;
  private readonly excludeInput: HTMLInputElement;
  private readonly inputMessage: HTMLElement;
  private readonly replaceRow: HTMLElement;
  private readonly replaceToggle: HTMLButtonElement;
  private readonly replaceAllButton: HTMLButtonElement;
  private readonly details: HTMLElement;
  private readonly message: HTMLElement;
  private readonly list: HTMLElement;
  private readonly spacer: HTMLElement;
  private readonly rowsEl: HTMLElement;
  private readonly progress: HTMLElement;
  private readonly matchCase: Toggle;
  private readonly wholeWord: Toggle;
  private readonly regex: Toggle;
  private readonly preserveCase: Toggle;
  private readonly useExcludes: Toggle;
  private readonly actions: Record<'refresh' | 'cancel' | 'clear' | 'collapse' | 'expand', HTMLButtonElement>;

  private rows: Row[] = [];
  private focused: { key: string; id?: string } | null = null;
  private replaceVisible = false;
  private hasWorkspace = false;
  private searchTimer: ReturnType<typeof setTimeout> | undefined;
  private lastQuery: string | null = null;
  /** Shown instead of the result count after Replace All, until the next search. */
  private notice: string | null = null;
  private renderQueued = false;

  constructor(
    container: HTMLElement,
    actionsContainer: HTMLElement,
    private readonly model: SearchModel,
    private readonly host: SearchViewHost,
  ) {
    container.classList.add('search-view');
    container.innerHTML = `
      <div class="progress" hidden></div>
      <div class="search-widgets">
        <button class="toggle-replace icon-button codicon codicon-chevron-right" title="Toggle Replace" aria-expanded="false"></button>
        <div class="search-fields">
          <div class="input-box">
            <textarea class="search-input" rows="1" placeholder="Search" aria-label="Search" spellcheck="false"></textarea>
            <div class="input-toggles"></div>
          </div>
          <div class="input-message" role="alert" hidden></div>
          <div class="replace-row" hidden>
            <div class="input-box">
              <textarea class="replace-input" rows="1" placeholder="Replace" aria-label="Replace" spellcheck="false"></textarea>
              <div class="input-toggles"></div>
            </div>
            <button class="replace-all icon-button codicon codicon-replace-all" title="Replace All (Ctrl+Alt+Enter)" disabled></button>
          </div>
        </div>
      </div>
      <div class="query-details">
        <button class="toggle-details icon-button codicon codicon-ellipsis" title="Toggle Search Details" aria-expanded="false"></button>
        <div class="details" hidden>
          <label class="details-label">files to include</label>
          <div class="input-box">
            <input class="include-input" placeholder="e.g. *.ts, src/**/include" aria-label="files to include" spellcheck="false">
          </div>
          <label class="details-label">files to exclude</label>
          <div class="input-box">
            <input class="exclude-input" placeholder="e.g. *.ts, src/**/exclude" aria-label="files to exclude" spellcheck="false">
            <div class="input-toggles"></div>
          </div>
        </div>
      </div>
      <div class="search-message"></div>
      <div class="search-results" tabindex="0" role="tree" aria-label="Search results">
        <div class="results-spacer"><div class="results-rows"></div></div>
      </div>`;
    const q = <T extends HTMLElement>(selector: string) => container.querySelector<T>(selector)!;
    this.searchInput = q('.search-input');
    this.replaceInput = q('.replace-input');
    this.includeInput = q('.include-input');
    this.excludeInput = q('.exclude-input');
    this.inputMessage = q('.input-message');
    this.replaceRow = q('.replace-row');
    this.replaceToggle = q('.toggle-replace');
    this.replaceAllButton = q('.replace-all');
    this.details = q('.details');
    this.message = q('.search-message');
    this.list = q('.search-results');
    this.spacer = q('.results-spacer');
    this.rowsEl = q('.results-rows');
    this.progress = q('.progress');

    const [searchToggles, replaceToggles, excludeToggles] = container.querySelectorAll<HTMLElement>('.input-toggles');
    this.matchCase = this.addToggle(searchToggles, 'codicon-case-sensitive', 'Match Case (Alt+C)');
    this.wholeWord = this.addToggle(searchToggles, 'codicon-whole-word', 'Match Whole Word (Alt+W)');
    this.regex = this.addToggle(searchToggles, 'codicon-regex', 'Use Regular Expression (Alt+R)');
    this.preserveCase = this.addToggle(replaceToggles, 'codicon-preserve-case', 'Preserve Case (Alt+P)', () =>
      this.updateReplace(),
    );
    this.useExcludes = this.addToggle(excludeToggles, 'codicon-exclude', 'Use Exclude Settings and Ignore Files', undefined, true);

    const action = (icon: string, title: string, run: () => void) => {
      const button = document.createElement('button');
      button.className = `icon-button codicon ${icon}`;
      button.title = title;
      button.addEventListener('click', run);
      actionsContainer.append(button);
      return button;
    };
    this.actions = {
      refresh: action('codicon-refresh', 'Refresh', () => this.runSearch(true)),
      cancel: action('codicon-search-stop', 'Cancel Search', () => model.cancel()),
      clear: action('codicon-clear-all', 'Clear Search Results', () => this.clear()),
      collapse: action('codicon-collapse-all', 'Collapse All', () => model.setAllCollapsed(true)),
      expand: action('codicon-expand-all', 'Expand All', () => model.setAllCollapsed(false)),
    };

    this.wireInputs(container);
    this.wireList();
    model.onDidChangeResults.on(() => this.scheduleRender());
    model.onDidChangeState.on(() => this.scheduleRender());
    new ResizeObserver(() => this.renderRows()).observe(this.list);
    this.render();
  }

  setWorkspace(open: boolean): void {
    this.hasWorkspace = open;
    this.lastQuery = null;
    this.notice = null;
    this.render();
  }

  setVisible(visible: boolean): void {
    this.model.setHighlights(visible);
    if (visible) this.render();
  }

  /** Focuses the view (VS Code's Find in Files / Replace in Files). */
  focus({ seed, replace = false }: { seed?: string; replace?: boolean } = {}): void {
    if (replace) this.setReplaceVisible(true);
    if (seed !== undefined && seed !== this.searchInput.value) {
      this.searchInput.value = seed;
      this.autoSize(this.searchInput);
      this.onQueryInput();
    }
    const target = replace && this.searchInput.value ? this.replaceInput : this.searchInput;
    target.focus();
    target.select();
  }

  /** Selects the next/previous match and opens it (F4 / Shift+F4). */
  navigate(direction: 1 | -1): void {
    const from = this.focusedEntry();
    const next = this.model.neighbor(from?.match ? { file: from.file, match: from.match } : null, direction);
    if (!next) return;
    if (next.file.collapsed) this.model.setCollapsed(next.file, false);
    this.focused = { key: next.file.key, id: matchId(next.match) };
    this.rebuildRows();
    this.revealFocused();
    this.renderRows();
    this.onOpenMatch.fire({ file: next.file, match: next.match, preserveFocus: true });
  }

  get hasResults(): boolean {
    return this.model.hasResults;
  }

  // ---- Inputs

  private addToggle(parent: HTMLElement, icon: string, title: string, onChange?: () => void, checked = false): Toggle {
    const el = document.createElement('button');
    el.className = `toggle codicon ${icon}`;
    el.title = title;
    const toggle: Toggle = { el, checked };
    const set = (value: boolean) => {
      toggle.checked = value;
      el.classList.toggle('checked', value);
      el.setAttribute('aria-pressed', String(value));
    };
    set(checked);
    el.addEventListener('click', () => {
      set(!toggle.checked);
      if (onChange) onChange();
      else this.runSearch(false);
    });
    parent.append(el);
    return toggle;
  }

  private flip(toggle: Toggle): void {
    toggle.el.click();
  }

  private wireInputs(container: HTMLElement): void {
    this.searchInput.addEventListener('input', () => {
      this.autoSize(this.searchInput);
      this.onQueryInput();
    });
    this.replaceInput.addEventListener('input', () => {
      this.autoSize(this.replaceInput);
      this.updateReplace();
    });
    for (const input of [this.includeInput, this.excludeInput]) {
      input.addEventListener('input', () => this.scheduleSearch());
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          this.runSearch(true);
        }
      });
    }
    for (const textarea of [this.searchInput, this.replaceInput]) {
      textarea.addEventListener('keydown', (e) => {
        if (e.key !== 'Enter' || e.altKey) return;
        // Ctrl+Enter inserts a line break (multi-line search), as in VS Code.
        if (e.ctrlKey || e.metaKey) {
          e.preventDefault();
          const { selectionStart, selectionEnd, value } = textarea;
          textarea.value = value.slice(0, selectionStart) + '\n' + value.slice(selectionEnd);
          textarea.selectionStart = textarea.selectionEnd = selectionStart + 1;
          textarea.dispatchEvent(new Event('input'));
          return;
        }
        e.preventDefault();
        this.runSearch(true);
      });
    }
    this.replaceToggle.addEventListener('click', () => this.setReplaceVisible(!this.replaceVisible));
    this.replaceAllButton.addEventListener('click', () => void this.replaceAll());
    container.querySelector('.toggle-details')!.addEventListener('click', (e) => {
      const button = e.currentTarget as HTMLElement;
      this.details.hidden = !this.details.hidden;
      button.setAttribute('aria-expanded', String(!this.details.hidden));
      button.classList.toggle('checked', !this.details.hidden);
    });
    this.message.addEventListener('click', (e) => {
      const link = (e.target as HTMLElement).closest<HTMLElement>('[data-action]');
      if (link?.dataset.action === 'open-folder') this.onOpenFolder.fire();
      else if (link?.dataset.action === 'search-again') this.runSearch(true);
    });

    // Option shortcuts anywhere in the view, as in VS Code.
    container.addEventListener('keydown', (e) => {
      if (e.altKey && e.ctrlKey && e.key === 'Enter' && this.replaceVisible) {
        e.preventDefault();
        void this.replaceAll();
        return;
      }
      if (!e.altKey || e.ctrlKey || e.metaKey || e.shiftKey) return;
      const toggle = { c: this.matchCase, w: this.wholeWord, r: this.regex, p: this.preserveCase }[e.code.replace('Key', '').toLowerCase()];
      if (toggle && (toggle !== this.preserveCase || this.replaceVisible)) {
        e.preventDefault();
        this.flip(toggle);
      }
    });
  }

  private setReplaceVisible(visible: boolean): void {
    this.replaceVisible = visible;
    this.replaceRow.hidden = !visible;
    this.replaceToggle.classList.toggle('codicon-chevron-down', visible);
    this.replaceToggle.classList.toggle('codicon-chevron-right', !visible);
    this.replaceToggle.setAttribute('aria-expanded', String(visible));
    this.updateReplace();
  }

  private updateReplace(): void {
    this.model.setReplace(this.replaceInput.value, this.preserveCase.checked);
  }

  private autoSize(textarea: HTMLTextAreaElement): void {
    textarea.style.height = 'auto';
    textarea.style.height = `${Math.min(textarea.scrollHeight, 6 * 18 + 6)}px`;
  }

  private onQueryInput(): void {
    this.scheduleSearch();
  }

  private scheduleSearch(): void {
    clearTimeout(this.searchTimer);
    this.searchTimer = setTimeout(() => this.runSearch(false), SEARCH_ON_TYPE_DELAY);
  }

  private buildQuery(): TextSearchQuery {
    return {
      pattern: this.searchInput.value,
      isRegex: this.regex.checked,
      isCaseSensitive: this.matchCase.checked,
      isWordMatch: this.wholeWord.checked,
      includes: this.includeInput.value,
      excludes: this.excludeInput.value,
      useExcludeSettingsAndIgnoreFiles: this.useExcludes.checked,
    };
  }

  /** Runs the search; unless `force`, only when the query changed. */
  private runSearch(force: boolean): void {
    clearTimeout(this.searchTimer);
    const query = this.buildQuery();
    const key = JSON.stringify(query);
    if (!force && key === this.lastQuery) return;
    this.lastQuery = key;
    this.notice = null;
    this.focused = null;
    this.showInputMessage(null);
    if (!query.pattern || !this.hasWorkspace) {
      this.model.clear();
      return;
    }
    // Like VS Code, the pattern must be a valid JavaScript regular expression.
    if (query.isRegex) {
      try {
        new RegExp(query.pattern, 'u');
      } catch (err) {
        this.showInputMessage(err instanceof Error ? err.message : String(err));
        this.model.clear();
        return;
      }
    }
    void this.model.search(query);
  }

  private showInputMessage(message: string | null): void {
    this.inputMessage.hidden = message === null;
    this.inputMessage.textContent = message ?? '';
    this.searchInput.closest('.input-box')!.classList.toggle('invalid', message !== null);
  }

  private clear(): void {
    this.searchInput.value = '';
    this.replaceInput.value = '';
    this.autoSize(this.searchInput);
    this.autoSize(this.replaceInput);
    this.lastQuery = null;
    this.notice = null;
    this.focused = null;
    this.showInputMessage(null);
    this.model.clear();
    this.updateReplace();
    this.searchInput.focus();
  }

  private async replaceAll(): Promise<void> {
    if (!this.model.canReplace || !this.model.hasResults) return;
    const matches = this.model.matchCount;
    const files = this.model.fileCount;
    const value = this.replaceInput.value;
    if (!(await this.host.confirm(replaceAllConfirmation(matches, files, value), 'Replace'))) return;
    const result = await this.model.replaceAll();
    this.focused = null;
    this.notice = replaceAllDone(result.matches, result.files, value);
    for (const error of result.errors) this.host.report(`Replace failed in ${error}`);
    this.render();
  }

  private async replaceInRow(row: Row): Promise<void> {
    if (!this.model.canReplace) return;
    try {
      await this.model.replace(row.file, row.kind === 'match' ? row.match : undefined);
    } catch (err) {
      this.host.report(`Replace failed in ${row.file.name}: ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  // ---- Results list

  private wireList(): void {
    this.list.addEventListener('scroll', () => this.renderRows());
    this.list.addEventListener('click', (e) => {
      const target = e.target as HTMLElement;
      const rowEl = target.closest<HTMLElement>('.search-row');
      const row = rowEl && this.rows[Number(rowEl.dataset.index)];
      if (!row) return;
      this.setFocus(row);
      const action = target.closest<HTMLElement>('[data-action]')?.dataset.action;
      if (action === 'dismiss') this.model.dismiss(row.file, row.kind === 'match' ? row.match : undefined);
      else if (action === 'replace') void this.replaceInRow(row);
      else if (row.kind === 'file') this.model.setCollapsed(row.file, !row.file.collapsed);
      else this.onOpenMatch.fire({ file: row.file, match: row.match, preserveFocus: true });
    });
    this.list.addEventListener('dblclick', (e) => {
      const rowEl = (e.target as HTMLElement).closest<HTMLElement>('.search-row');
      const row = rowEl && this.rows[Number(rowEl.dataset.index)];
      if (row?.kind === 'match') this.onOpenMatch.fire({ file: row.file, match: row.match, preserveFocus: false });
      else if (row?.kind === 'file' && !(e.target as HTMLElement).closest('[data-action]')) this.onOpenFile.fire(row.file);
    });
    this.list.addEventListener('keydown', (e) => this.handleListKey(e));
    // Focusing the list with the keyboard focuses its first row. Not on a
    // mouse press: re-rendering then would swallow the click.
    let pointerDown = false;
    this.list.addEventListener('pointerdown', () => (pointerDown = true));
    window.addEventListener('pointerup', () => (pointerDown = false));
    this.list.addEventListener('focus', () => {
      if (!pointerDown && !this.focused && this.rows.length > 0) this.setFocus(this.rows[0]);
    });
  }

  private handleListKey(e: KeyboardEvent): void {
    const index = this.focusedIndex();
    const row = index >= 0 ? this.rows[index] : undefined;
    const move = (to: number) => {
      const target = this.rows[Math.max(0, Math.min(this.rows.length - 1, to))];
      if (target) this.setFocus(target);
    };
    let handled = true;
    switch (e.key) {
      case 'ArrowDown':
        move(index + 1);
        break;
      case 'ArrowUp':
        move(index < 0 ? 0 : index - 1);
        break;
      case 'Home':
        move(0);
        break;
      case 'End':
        move(this.rows.length - 1);
        break;
      case 'PageDown':
        move(index + Math.max(1, Math.floor(this.list.clientHeight / ROW_HEIGHT) - 1));
        break;
      case 'PageUp':
        move(index - Math.max(1, Math.floor(this.list.clientHeight / ROW_HEIGHT) - 1));
        break;
      case 'ArrowRight':
        if (row?.kind === 'file') {
          if (row.file.collapsed) this.model.setCollapsed(row.file, false);
          else move(index + 1);
        }
        break;
      case 'ArrowLeft':
        if (row?.kind === 'match') this.setFocus({ kind: 'file', file: row.file });
        else if (row?.kind === 'file' && !row.file.collapsed) this.model.setCollapsed(row.file, true);
        break;
      case 'Enter':
      case ' ':
        if (row?.kind === 'match') this.onOpenMatch.fire({ file: row.file, match: row.match, preserveFocus: e.key === ' ' });
        else if (row?.kind === 'file') this.model.setCollapsed(row.file, !row.file.collapsed);
        break;
      case 'Delete':
        if (row) this.model.dismiss(row.file, row.kind === 'match' ? row.match : undefined);
        break;
      default:
        // Ctrl+Shift+1: replace (VS Code's "Replace" / "Replace All in File").
        if (row && e.code === 'Digit1' && (e.ctrlKey || e.metaKey) && e.shiftKey && this.replaceVisible) {
          void this.replaceInRow(row);
        } else {
          handled = false;
        }
    }
    if (handled) {
      e.preventDefault();
      e.stopPropagation();
    }
  }

  private setFocus(row: Row): void {
    this.focused = { key: row.file.key, id: row.kind === 'match' ? matchId(row.match) : undefined };
    this.revealFocused();
    this.renderRows();
  }

  private focusedIndex(): number {
    const focus = this.focused;
    if (!focus) return -1;
    return this.rows.findIndex((r) =>
      r.file.key === focus.key && (r.kind === 'file' ? focus.id === undefined : focus.id === matchId(r.match)),
    );
  }

  private focusedEntry(): { file: FileResult; match?: TextMatch } | null {
    const index = this.focusedIndex();
    if (index < 0) return null;
    const row = this.rows[index];
    return row.kind === 'match' ? { file: row.file, match: row.match } : { file: row.file };
  }

  private revealFocused(): void {
    const index = this.focusedIndex();
    if (index < 0) return;
    const top = index * ROW_HEIGHT;
    if (top < this.list.scrollTop) this.list.scrollTop = top;
    else if (top + ROW_HEIGHT > this.list.scrollTop + this.list.clientHeight) {
      this.list.scrollTop = top + ROW_HEIGHT - this.list.clientHeight;
    }
  }

  private scheduleRender(): void {
    if (this.renderQueued) return;
    this.renderQueued = true;
    requestAnimationFrame(() => {
      this.renderQueued = false;
      this.render();
    });
  }

  private rebuildRows(): void {
    const rows: Row[] = [];
    for (const file of this.model.results()) {
      rows.push({ kind: 'file', file });
      if (!file.collapsed) for (const match of file.matches) rows.push({ kind: 'match', file, match });
    }
    this.rows = rows;
  }

  private render(): void {
    this.rebuildRows();
    const state = this.model.state;
    const searching = state.kind === 'searching';
    this.progress.hidden = !searching;
    this.actions.refresh.hidden = searching;
    this.actions.cancel.hidden = !searching;
    const has = this.model.hasResults;
    this.actions.clear.disabled = !has && !this.searchInput.value;
    const anyExpanded = this.model.results().some((f) => !f.collapsed);
    this.actions.collapse.hidden = !anyExpanded && has;
    this.actions.expand.hidden = anyExpanded || !has;
    this.actions.collapse.disabled = !has;
    this.replaceAllButton.disabled = !(this.model.canReplace && has);
    this.list.classList.toggle('replace-active', this.replaceVisible);
    this.renderMessage();
    this.renderRows();
  }

  private renderMessage(): void {
    const state = this.model.state;
    const parts: string[] = [];
    if (!this.hasWorkspace) {
      parts.push(`You have not opened a folder. <a data-action="open-folder" href="#">Open Folder</a>`);
    } else if (this.notice) {
      parts.push(escapeHtml(this.notice));
    } else if (state.kind === 'error') {
      parts.push(`<span class="error">${escapeHtml(state.message)}</span>`);
    } else if (this.model.hasResults) {
      if (state.kind === 'done' && state.limitHit) {
        parts.push(
          `<div class="warning">The result set only contains a subset of all matches. Be more specific in your search to narrow down the results.</div>`,
        );
      }
      parts.push(escapeHtml(resultCountMessage(this.model.matchCount, this.model.fileCount)));
    } else if (state.kind === 'done' && state.cancelled) {
      parts.push(`Search was canceled before any results could be found - <a data-action="search-again" href="#">Search again</a>`);
    } else if (state.kind === 'done' && this.model.query) {
      parts.push(escapeHtml(noResultsMessage(this.model.query)));
    }
    this.message.innerHTML = parts.join('');
    this.message.hidden = parts.length === 0;
  }

  private renderRows(): void {
    const total = this.rows.length;
    this.spacer.style.height = `${total * ROW_HEIGHT}px`;
    const first = Math.max(0, Math.floor(this.list.scrollTop / ROW_HEIGHT) - 4);
    const last = Math.min(total, Math.ceil((this.list.scrollTop + this.list.clientHeight) / ROW_HEIGHT) + 4);
    this.rowsEl.style.transform = `translateY(${first * ROW_HEIGHT}px)`;
    const focusedIndex = this.focusedIndex();
    const fragment = document.createDocumentFragment();
    for (let i = first; i < last; i++) fragment.append(this.renderRow(this.rows[i], i, i === focusedIndex));
    this.rowsEl.replaceChildren(fragment);
    if (focusedIndex >= 0) this.list.setAttribute('aria-activedescendant', `search-row-${focusedIndex}`);
    else this.list.removeAttribute('aria-activedescendant');
  }

  private renderRow(row: Row, index: number, focused: boolean): HTMLElement {
    const el = document.createElement('div');
    el.className = `search-row ${row.kind}`;
    el.id = `search-row-${index}`;
    el.dataset.index = String(index);
    el.setAttribute('role', 'treeitem');
    el.classList.toggle('focused', focused);
    const canReplace = this.replaceVisible && this.model.canReplace;

    if (row.kind === 'file') {
      const { file } = row;
      el.setAttribute('aria-expanded', String(!file.collapsed));
      el.title = file.path;
      el.innerHTML = `
        <span class="twistie codicon codicon-chevron-${file.collapsed ? 'right' : 'down'}"></span>
        <span class="file-label"><span class="file-name"></span><span class="file-dir"></span></span>
        <span class="row-actions">
          ${canReplace ? `<button class="icon-button codicon codicon-replace-all" data-action="replace" title="Replace All (Ctrl+Shift+1)"></button>` : ''}
          <button class="icon-button codicon codicon-close" data-action="dismiss" title="Dismiss (Delete)"></button>
        </span>
        <span class="badge"></span>`;
      el.querySelector('.file-name')!.textContent = file.name;
      el.querySelector('.file-dir')!.textContent = file.dir;
      el.querySelector('.badge')!.textContent = String(file.matches.length);
      return el;
    }

    const { match } = row;
    const parts = previewParts(match);
    const lines = match.range.end.line - match.range.start.line;
    el.title = (parts.fullBefore + parts.inside + parts.after).trim();
    el.classList.toggle('current', this.model.isCurrent(row.file, match));
    const preview = document.createElement('span');
    preview.className = 'match-preview';
    const span = (cls: string, text: string) => {
      const s = document.createElement('span');
      s.className = cls;
      s.textContent = text;
      return s;
    };
    preview.append(span('before', parts.before));
    if (this.replaceVisible && this.model.query) {
      preview.append(span('match removed', parts.inside), span('match inserted', this.model.previewReplacement(match)));
    } else {
      preview.append(span('match', parts.inside));
    }
    preview.append(span('after', parts.after));
    el.append(preview);
    if (lines > 0) el.append(span('line-count', `+${lines}`));
    const actions = document.createElement('span');
    actions.className = 'row-actions';
    actions.innerHTML = `
      ${canReplace ? `<button class="icon-button codicon codicon-replace" data-action="replace" title="Replace (Ctrl+Shift+1)"></button>` : ''}
      <button class="icon-button codicon codicon-close" data-action="dismiss" title="Dismiss (Delete)"></button>`;
    el.append(actions);
    return el;
  }
}

/** VS Code's "No results found" variants. */
function noResultsMessage(query: TextSearchQuery): string {
  const includes = query.includes.trim();
  const excludes = query.excludes.trim();
  if (includes && excludes) return `No results found in '${includes}' excluding '${excludes}'.`;
  if (includes) return `No results found in '${includes}'.`;
  if (excludes) return `No results found excluding '${excludes}'.`;
  return 'No results found. Review your configured exclusions and check your gitignore files.';
}

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}
