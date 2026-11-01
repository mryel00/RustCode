// File explorer: a lazily loaded tree of the open folder.

import { backend, errorMessage, type DirEntry, type FsChange } from '../backend';
import { Emitter } from './events';

interface Node {
  entry: DirEntry;
  depth: number;
  expanded: boolean;
  children?: Node[];
}

export class Explorer {
  readonly onOpenFile = new Emitter<string>();
  readonly onError = new Emitter<string>();
  private roots: Node[] = [];
  private rootPath: string | null = null;
  private activePath: string | null = null;
  /** Serializes tree reloads so overlapping refreshes cannot interleave. */
  private updates: Promise<void> = Promise.resolve();

  constructor(private readonly container: HTMLElement) {
    container.addEventListener('click', (e) => this.handleClick(e));
    container.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') this.handleClick(e);
    });
  }

  async setRoot(path: string | null): Promise<void> {
    return this.enqueue(async () => {
      this.rootPath = path;
      this.roots = [];
      if (path) this.roots = await this.load(path, 0);
      this.render();
    });
  }

  /** Reloads every expanded folder, keeping expansion state. */
  refresh(): Promise<void> {
    return this.enqueue(() => this.doRefresh());
  }

  /**
   * Updates the tree for files created or deleted on disk: each affected
   * folder that is loaded is listed again, keeping the state of folders that
   * still exist. Collapsed folders just forget their children.
   */
  applyChanges(changes: FsChange[]): Promise<void> {
    const dirs = new Set<string>();
    for (const c of changes) {
      if (c.kind !== 'changed') dirs.add(parentPath(c.path));
    }
    if (dirs.size === 0) return this.updates;
    // Parents first: a folder deleted with its parent then simply disappears.
    const ordered = [...dirs].sort((a, b) => a.length - b.length);
    return this.enqueue(async () => {
      if (!this.rootPath) return;
      let changed = false;
      for (const dir of ordered) {
        if (dir === this.rootPath) {
          this.roots = await this.relist(dir, 0, this.roots);
          changed = true;
          continue;
        }
        const node = this.find(dir);
        if (!node?.entry.isDir || !node.children) continue;
        node.children = node.expanded ? await this.relist(dir, node.depth + 1, node.children) : undefined;
        changed = true;
      }
      if (changed) this.render();
    });
  }

  private enqueue(task: () => Promise<void>): Promise<void> {
    this.updates = this.updates.then(task, task);
    return this.updates;
  }

  /** Lists `path` again, carrying over state from `previous` for entries that remain. */
  private async relist(path: string, depth: number, previous: Node[]): Promise<Node[]> {
    // The folder may be gone already; its parent's update removes it.
    const fresh = await this.load(path, depth, { quiet: true });
    const old = new Map(previous.map((n) => [n.entry.path, n]));
    return fresh.map((n) => {
      const prev = old.get(n.entry.path);
      return prev && prev.entry.isDir === n.entry.isDir ? { ...n, expanded: prev.expanded, children: prev.children } : n;
    });
  }

  private async doRefresh(): Promise<void> {
    if (!this.rootPath) return;
    const expanded = new Set<string>();
    const collect = (nodes: Node[]) =>
      nodes.forEach((n) => {
        if (n.expanded) expanded.add(n.entry.path);
        if (n.children) collect(n.children);
      });
    collect(this.roots);
    const reload = async (path: string, depth: number): Promise<Node[]> => {
      const nodes = await this.load(path, depth);
      for (const n of nodes) {
        if (n.entry.isDir && expanded.has(n.entry.path)) {
          n.expanded = true;
          n.children = await reload(n.entry.path, depth + 1);
        }
      }
      return nodes;
    };
    this.roots = await reload(this.rootPath, 0);
    this.render();
  }

  setActive(path: string | null): void {
    this.activePath = path;
    for (const el of this.container.querySelectorAll<HTMLElement>('.tree-row')) {
      el.classList.toggle('active', el.dataset.path === path);
    }
  }

  private async load(path: string, depth: number, { quiet = false } = {}): Promise<Node[]> {
    try {
      const entries = await backend.listDir(path);
      return entries.map((entry) => ({ entry, depth, expanded: false }));
    } catch (err) {
      if (!quiet) this.onError.fire(errorMessage(err));
      return [];
    }
  }

  private find(path: string, nodes = this.roots): Node | undefined {
    for (const n of nodes) {
      if (n.entry.path === path) return n;
      const hit = n.children && this.find(path, n.children);
      if (hit) return hit;
    }
    return undefined;
  }

  private handleClick(e: Event): void {
    const path = (e.target as HTMLElement).closest<HTMLElement>('.tree-row')?.dataset.path;
    if (!path) return;
    const file = this.find(path);
    if (file && !file.entry.isDir) {
      this.onOpenFile.fire(path);
      return;
    }
    // Look the node up again inside the queue: an update may have replaced it.
    void this.enqueue(async () => {
      const node = this.find(path);
      if (!node?.entry.isDir) return;
      node.expanded = !node.expanded;
      if (node.expanded && !node.children) node.children = await this.load(path, node.depth + 1);
      this.render();
    });
  }

  private render(): void {
    const fragment = document.createDocumentFragment();
    const add = (nodes: Node[]) => {
      for (const n of nodes) {
        const row = document.createElement('div');
        row.className = 'tree-row';
        row.tabIndex = 0;
        row.setAttribute('role', 'treeitem');
        row.dataset.path = n.entry.path;
        row.style.paddingLeft = `${8 + n.depth * 12}px`;
        row.title = n.entry.path;
        if (n.entry.isDir) row.setAttribute('aria-expanded', String(n.expanded));
        if (n.entry.path === this.activePath) row.classList.add('active');

        const twistie = document.createElement('span');
        twistie.className = n.entry.isDir
          ? `twistie codicon codicon-chevron-${n.expanded ? 'down' : 'right'}`
          : 'twistie';
        const label = document.createElement('span');
        label.className = 'tree-label';
        label.textContent = n.entry.name;
        row.append(twistie, label);
        fragment.append(row);
        if (n.expanded && n.children) add(n.children);
      }
    };
    add(this.roots);
    this.container.replaceChildren(fragment);
  }
}

function parentPath(path: string): string {
  const cut = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
  if (cut < 0) return path;
  // Keep the separator of a root such as "/" or "C:\".
  return cut === 0 || path[cut - 1] === ':' ? path.slice(0, cut + 1) : path.slice(0, cut);
}
