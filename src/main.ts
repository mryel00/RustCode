import './styles.css';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { ask, open as openDialog } from '@tauri-apps/plugin-dialog';
import { backend, errorMessage } from './backend';
import { LanguageFeatures } from './lsp/features';
import { monaco } from './monaco/setup';
import { Documents } from './workbench/documents';
import { EditorGroup } from './workbench/editor-group';
import { Explorer } from './workbench/explorer';
import { initSash } from './workbench/sash';
import { StatusBar, toast } from './workbench/statusbar';

const $ = (id: string) => document.getElementById(id)!;

const docs = new Documents();
const features = new LanguageFeatures(docs);
const group = new EditorGroup(docs, $('tabs'), $('editor'), $('welcome'));
const explorer = new Explorer($('explorer'));
const statusBar = new StatusBar(features);
const appWindow = getCurrentWindow();
let workspaceName: string | null = null;

const report = (err: unknown) => toast(errorMessage(err), 'error');
docs.onError.on((m) => toast(m, 'error'));
explorer.onError.on((m) => toast(m, 'error'));
features.onMessage.on(({ level, message }) =>
  toast(message, level === 'error' ? 'error' : level === 'warning' ? 'warning' : 'info'),
);
void backend.onLspEvent((event) => features.handle(event));

function updateTitle(): void {
  const doc = group.activeDoc;
  const parts = [doc ? `${docs.isDirty(doc) ? '● ' : ''}${doc.name}` : null, workspaceName, 'RustCode'];
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

explorer.onOpenFile.on((path) => {
  docs.open({ path }).then((doc) => group.show(doc), report);
});

async function openWorkspace(path: string): Promise<void> {
  if (!(await group.closeAll())) return;
  features.reset();
  try {
    const info = await backend.openWorkspace(path);
    workspaceName = info.name;
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

async function saveActive(): Promise<void> {
  const doc = group.activeDoc;
  if (!doc) return;
  try {
    await docs.save(doc);
  } catch (err) {
    report(err);
  }
}

// Keyboard shortcuts. Capture phase so they work whether or not Monaco has focus.
window.addEventListener(
  'keydown',
  (e) => {
    const mod = e.ctrlKey || e.metaKey;
    if (!mod || e.altKey) return;
    const key = e.key.toLowerCase();
    if (key === 's' && !e.shiftKey) {
      e.preventDefault();
      e.stopPropagation();
      void saveActive();
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
Object.assign(window, { rustcode: { monaco, docs, group, features } });
