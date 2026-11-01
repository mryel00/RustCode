import './styles.css';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { ask, message, open as openDialog } from '@tauri-apps/plugin-dialog';
import { backend, errorMessage } from './backend';
import { LanguageFeatures } from './lsp/features';
import { monaco } from './monaco/setup';
import { Documents, type Doc } from './workbench/documents';
import { EditorGroup } from './workbench/editor-group';
import { Explorer } from './workbench/explorer';
import { initSash } from './workbench/sash';
import { SearchModel, toMonacoRange } from './workbench/search/search-model';
import { SearchView } from './workbench/search/search-view';
import { Sidebar } from './workbench/sidebar';
import { StatusBar, toast } from './workbench/statusbar';

const $ = (id: string) => document.getElementById(id)!;

const docs = new Documents();
const features = new LanguageFeatures(docs);
const group = new EditorGroup(docs, $('tabs'), $('editor'), $('welcome'));
const explorer = new Explorer($('explorer'));
const statusBar = new StatusBar(features);
const sidebar = new Sidebar($('activitybar'), $('sidebar'), $('sash'), {
  explorer: $('view-explorer'),
  search: $('view-search'),
});
const searchModel = new SearchModel({ docs, save: (doc) => save(doc), isShown: (doc) => group.isShown(doc) });
const searchView = new SearchView($('search-view'), $('search-actions'), searchModel, {
  confirm: (text, okLabel) => ask(text, { title: 'Replace All', kind: 'warning', okLabel, cancelLabel: 'Cancel' }),
  report: (text) => toast(text, 'error'),
});
const appWindow = getCurrentWindow();
let workspaceName: string | null = null;

const report = (err: unknown) => toast(errorMessage(err), 'error');
docs.onError.on((m) => toast(m, 'error'));
explorer.onError.on((m) => toast(m, 'error'));
features.onMessage.on(({ level, message }) =>
  toast(message, level === 'error' ? 'error' : level === 'warning' ? 'warning' : 'info'),
);
void backend.onLspEvent((event) => features.handle(event));
void backend.onFsEvent((event) => {
  switch (event.kind) {
    case 'changes':
      docs.handleDiskChanges(event.documents);
      void explorer.applyChanges(event.changes);
      break;
    case 'rescan':
      docs.handleDiskChanges(event.documents);
      void explorer.refresh();
      break;
    case 'error':
      toast(`File changes may not be picked up: ${event.message}`, 'warning');
      break;
    case 'settingsChanged':
      event.warnings.forEach((w) => toast(w, 'warning'));
      void explorer.refresh();
      break;
  }
});

function updateTitle(): void {
  const doc = group.activeDoc;
  const name = doc ? `${docs.isDirty(doc) ? '● ' : ''}${doc.name}${doc.orphaned ? ' (deleted)' : ''}` : null;
  const parts = [name, workspaceName, 'RustCode'];
  void appWindow.setTitle(parts.filter(Boolean).join(' — '));
}

group.onDidChangeActive.on((doc) => {
  explorer.setActive(doc?.path ?? null);
  statusBar.setDocument(doc, group.editor);
  updateTitle();
});
group.editor.onDidChangeCursorPosition(() => statusBar.setDocument(group.activeDoc, group.editor));
docs.onDidChangeDirty.on((doc) => {
  if (doc === group.activeDoc) updateTitle();
});
docs.onDidChangeOrphaned.on((doc) => {
  if (doc === group.activeDoc) updateTitle();
});

explorer.onOpenFile.on((path) => {
  docs.open({ path }).then((doc) => group.show(doc), report);
});

sidebar.onDidChangeView.on((view) => searchView.setVisible(view === 'search'));
searchView.onOpenFolder.on(() => void pickFolder());
searchView.onOpenFile.on((file) => {
  docs.open({ uri: file.uri }).then((doc) => group.show(doc), report);
});
searchView.onOpenMatch.on(({ file, match, preserveFocus }) => {
  docs.open({ uri: file.uri }).then((doc) => {
    group.show(doc, { preserveFocus });
    const range = toMonacoRange(match.range);
    group.editor.setSelection(range);
    group.editor.revealRangeInCenterIfOutsideViewport(range);
    searchModel.setCurrent(file, match);
  }, report);
});

/** A single-line selection in the editor, to seed the search with (VS Code's default). */
function selectedText(): string | undefined {
  const model = group.editor.getModel();
  const selection = group.editor.getSelection();
  if (!model || !selection || selection.isEmpty() || selection.startLineNumber !== selection.endLineNumber) return undefined;
  return model.getValueInRange(selection);
}

function showSearch(replace: boolean): void {
  const seed = selectedText();
  sidebar.show('search');
  searchView.focus({ seed, replace });
}

async function openWorkspace(path: string): Promise<void> {
  if (!(await group.closeAll())) return;
  features.reset();
  try {
    const info = await backend.openWorkspace(path);
    workspaceName = info.name;
    info.settingsWarnings.forEach((w) => toast(w, 'warning'));
    searchModel.setRoot(info.root);
    searchView.setWorkspace(true);
    $('workspace-name').textContent = info.name;
    $('workspace-name').title = info.root;
    document.body.classList.add('has-workspace');
    await explorer.setRoot(info.root);
    updateTitle();
  } catch (err) {
    report(err);
  }
}

async function pickFolder(): Promise<void> {
  const selected = await openDialog({ directory: true, multiple: false, title: 'Open Folder' });
  if (typeof selected === 'string') await openWorkspace(selected);
}

/** Saves a document; if the file changed on disk meanwhile, asks how to resolve it. */
async function save(doc: Doc): Promise<void> {
  try {
    if ((await docs.save(doc)) === 'saved') return;
    const choice = await message(
      `Failed to save '${doc.name}': the content of the file is newer. ` +
        'Overwrite the file with your changes, or revert to the file contents and discard your changes?',
      { title: 'File changed on disk', kind: 'warning', buttons: { yes: 'Overwrite', no: 'Revert', cancel: 'Cancel' } },
    );
    if (choice === 'Overwrite') await docs.save(doc, true);
    else if (choice === 'Revert') await docs.revert(doc);
  } catch (err) {
    report(err);
  }
}

async function revertActive(): Promise<void> {
  const doc = group.activeDoc;
  if (!doc) return;
  try {
    await docs.revert(doc);
  } catch (err) {
    report(err);
  }
}

// Keyboard shortcuts. Capture phase so they work whether or not Monaco has focus.
window.addEventListener(
  'keydown',
  (e) => {
    const mod = e.ctrlKey || e.metaKey;
    // F4 / Shift+F4: next / previous search result.
    if (e.key === 'F4' && !mod && !e.altKey && searchView.hasResults) {
      e.preventDefault();
      e.stopPropagation();
      searchView.navigate(e.shiftKey ? -1 : 1);
      return;
    }
    if (!mod || e.altKey) return;
    const key = e.key.toLowerCase();
    if (e.shiftKey && (key === 'f' || key === 'h' || key === 'e')) {
      e.preventDefault();
      e.stopPropagation();
      if (key === 'e') {
        sidebar.show('explorer');
        $('explorer').querySelector<HTMLElement>('.tree-row.active, .tree-row')?.focus();
      } else {
        showSearch(key === 'h');
      }
      return;
    }
    if (key === 's' && !e.shiftKey) {
      e.preventDefault();
      e.stopPropagation();
      if (group.activeDoc) void save(group.activeDoc);
    } else if (key === 'o' && !e.shiftKey) {
      e.preventDefault();
      e.stopPropagation();
      void pickFolder();
    } else if (key === 'w' && !e.shiftKey) {
      e.preventDefault();
      e.stopPropagation();
      if (group.activeDoc) void group.close(group.activeDoc);
    }
  },
  true,
);

$('open-folder').addEventListener('click', () => void pickFolder());
$('welcome-open').addEventListener('click', () => void pickFolder());
$('refresh-explorer').addEventListener('click', () => void explorer.refresh());
initSash($('sash'), $('sidebar'));

// Ask before closing the window with unsaved changes.
void appWindow.onCloseRequested(async (event) => {
  const dirty = docs.all().filter((d) => docs.isDirty(d));
  if (dirty.length === 0) return;
  const names = dirty.map((d) => d.name).join(', ');
  const quit = await ask(`You have unsaved changes in ${names}. Quit anyway?`, {
    title: 'Unsaved changes',
    kind: 'warning',
    okLabel: 'Quit',
    cancelLabel: 'Cancel',
  });
  if (!quit) event.preventDefault();
});

void backend.initialPath().then((path) => {
  if (path) void openWorkspace(path);
});

// Exposed for debugging from the devtools console.
Object.assign(window, { rustcode: { monaco, docs, group, features, explorer, searchModel, revertActive } });
