// File explorer: a lazily loaded tree of the open folder.

import { backend, errorMessage, type DirEntry } from '../backend';
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

  constructor(private readonly container: HTMLElement) {
    container.addEventListener('click', (e) => this.handleClick(e));
    container.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') this.handleClick(e);
    });
  }

  async setRoot(path: string | null): Promise<void> {
    this.rootPath = path;
    this.roots = [];
    if (path) this.roots = await this.load(path, 0);
    this.render();
  }

  /** Reloads every expanded folder, keeping expansion state. */
  async refresh(): Promise<void> {
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

  private async load(path: string, depth: number): Promise<Node[]> {
    try {
      const entries = await backend.listDir(path);
      return entries.map((entry) => ({ entry, depth, expanded: false }));
    } catch (err) {
      this.onError.fire(errorMessage(err));
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

  private async handleClick(e: Event): Promise<void> {
    const row = (e.target as HTMLElement).closest<HTMLElement>('.tree-row');
    const node = row?.dataset.path ? this.find(row.dataset.path) : undefined;
    if (!node) return;
    if (!node.entry.isDir) {
      this.onOpenFile.fire(node.entry.path);
      return;
    }
    node.expanded = !node.expanded;
    if (node.expanded && !node.children) node.children = await this.load(node.entry.path, node.depth + 1);
    this.render();
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
