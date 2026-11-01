// The editor area: a tab strip over a single Monaco editor that switches models.

import { ask } from '@tauri-apps/plugin-dialog';
import { monaco } from '../monaco/setup';
import type { Doc, Documents } from './documents';
import { Emitter } from './events';

export class EditorGroup {
  readonly editor: monaco.editor.IStandaloneCodeEditor;
  readonly onDidChangeActive = new Emitter<Doc | null>();
  private tabs: Doc[] = [];
  private active: Doc | null = null;
  private readonly viewStates = new Map<Doc, monaco.editor.ICodeEditorViewState | null>();

  constructor(
    private readonly docs: Documents,
    private readonly tabStrip: HTMLElement,
    editorEl: HTMLElement,
    private readonly welcomeEl: HTMLElement,
  ) {
    this.editor = monaco.editor.create(editorEl, {
      model: null,
      theme: 'rustcode-dark',
      automaticLayout: true,
      fontFamily: "'JetBrains Mono', 'Cascadia Code', 'Fira Code', Menlo, Consolas, monospace",
      fontSize: 14,
      fontLigatures: true,
      minimap: { enabled: true },
      smoothScrolling: true,
      cursorSmoothCaretAnimation: 'on',
      bracketPairColorization: { enabled: true },
      guides: { bracketPairs: 'active', indentation: true },
      renderWhitespace: 'selection',
      scrollBeyondLastLine: false,
      fixedOverflowWidgets: true,
      'semanticHighlighting.enabled': false,
    });

    // Go to Definition into another file: show that document in a tab.
    monaco.editor.registerEditorOpener({
      openCodeEditor: (_source, resource, selectionOrPosition) => {
        const doc = this.docs.get(resource);
        if (!doc) return false;
        this.show(doc);
        if (selectionOrPosition) {
          if ('startLineNumber' in selectionOrPosition) {
            this.editor.setSelection(selectionOrPosition);
            this.editor.revealRangeInCenterIfOutsideViewport(selectionOrPosition);
          } else {
            this.editor.setPosition(selectionOrPosition);
            this.editor.revealPositionInCenterIfOutsideViewport(selectionOrPosition);
          }
        }
        return true;
      },
    });

    this.tabStrip.addEventListener('click', (e) => this.handleTabClick(e));
    this.tabStrip.addEventListener('auxclick', (e) => {
      if (e.button === 1) this.handleTabClick(e, true);
    });
    docs.onDidChangeDirty.on(() => this.renderTabs());
    docs.onDidChangeOrphaned.on(() => this.renderTabs());
    this.renderTabs();
  }

  get activeDoc(): Doc | null {
    return this.active;
  }

  show(doc: Doc): void {
    if (!this.tabs.includes(doc)) {
      const at = this.active ? this.tabs.indexOf(this.active) + 1 : this.tabs.length;
      this.tabs.splice(at, 0, doc);
    }
    if (this.active !== doc) {
      if (this.active) this.viewStates.set(this.active, this.editor.saveViewState());
      this.active = doc;
      this.editor.setModel(doc.model);
      const state = this.viewStates.get(doc);
      if (state) this.editor.restoreViewState(state);
      this.onDidChangeActive.fire(doc);
    }
    this.renderTabs();
    this.editor.focus();
  }

  /** Closes a tab, asking first if it has unsaved changes. Returns false if cancelled. */
  async close(doc: Doc): Promise<boolean> {
    if (this.docs.isDirty(doc)) {
      const discard = await ask(`Do you want to discard your changes to ${doc.name}?`, {
        title: 'Unsaved changes',
        kind: 'warning',
        okLabel: "Don't Save",
        cancelLabel: 'Cancel',
      });
      if (!discard) return false;
    }
    const index = this.tabs.indexOf(doc);
    if (index >= 0) this.tabs.splice(index, 1);
    this.viewStates.delete(doc);
    if (this.active === doc) {
      const next = this.tabs[Math.min(index, this.tabs.length - 1)] ?? null;
      this.active = null;
      if (next) {
        this.show(next);
      } else {
        this.editor.setModel(null);
        this.onDidChangeActive.fire(null);
      }
    }
    await this.docs.close(doc);
    this.renderTabs();
    return true;
  }

  /** Closes every tab and every background document. Returns false if cancelled. */
  async closeAll(): Promise<boolean> {
    for (const doc of [...this.tabs]) {
      if (!(await this.close(doc))) return false;
    }
    // Documents opened in the background (e.g. by a definition peek) can still be edited.
    for (const doc of this.docs.all()) {
      if (this.docs.isDirty(doc)) {
        this.show(doc);
        if (!(await this.close(doc))) return false;
      } else {
        await this.docs.close(doc);
      }
    }
    return true;
  }

  private handleTabClick(e: MouseEvent, middle = false): void {
    const tab = (e.target as HTMLElement).closest<HTMLElement>('.tab');
    const doc = this.tabs.find((d) => d.uri === tab?.dataset.uri);
    if (!doc) return;
    if (middle || (e.target as HTMLElement).closest('.tab-close')) {
      void this.close(doc);
    } else {
      this.show(doc);
    }
  }

  private renderTabs(): void {
    this.welcomeEl.hidden = this.tabs.length > 0;
    const fragment = document.createDocumentFragment();
    for (const doc of this.tabs) {
      const tab = document.createElement('div');
      tab.className = 'tab';
      tab.setAttribute('role', 'tab');
      tab.dataset.uri = doc.uri;
      tab.title = doc.orphaned ? `${doc.path} (deleted)` : doc.path;
      const dirty = this.docs.isDirty(doc);
      tab.classList.toggle('active', doc === this.active);
      tab.classList.toggle('dirty', dirty);
      tab.classList.toggle('deleted', doc.orphaned);
      tab.setAttribute('aria-selected', String(doc === this.active));

      const label = document.createElement('span');
      label.className = 'tab-label';
      label.textContent = doc.name;
      const close = document.createElement('button');
      close.className = `tab-close codicon ${dirty ? 'codicon-circle-filled' : 'codicon-close'}`;
      close.title = 'Close (Ctrl+W)';
      tab.append(label, close);
      fragment.append(tab);
    }
    this.tabStrip.replaceChildren(fragment);
    this.tabStrip.querySelector('.tab.active')?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  }
}
