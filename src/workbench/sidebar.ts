// Activity bar and side bar: one view at a time, like VS Code's primary side bar.

import { Emitter } from './events';

export type ViewId = 'explorer' | 'search';

export class Sidebar {
  /** Fires with the visible view, or null when the side bar is hidden. */
  readonly onDidChangeView = new Emitter<ViewId | null>();
  private active: ViewId = 'explorer';
  private hidden = false;

  constructor(
    private readonly activityBar: HTMLElement,
    private readonly sidebar: HTMLElement,
    private readonly sash: HTMLElement,
    private readonly views: Record<ViewId, HTMLElement>,
  ) {
    activityBar.addEventListener('click', (e) => {
      const view = (e.target as HTMLElement).closest<HTMLElement>('[data-view]')?.dataset.view as ViewId | undefined;
      if (!view) return;
      // Clicking the active view's icon hides the side bar, as in VS Code.
      if (view === this.active && !this.hidden) this.setHidden(true);
      else this.show(view);
    });
    this.render();
  }

  get visibleView(): ViewId | null {
    return this.hidden ? null : this.active;
  }

  show(view: ViewId): void {
    if (view === this.active && !this.hidden) return;
    this.active = view;
    this.hidden = false;
    this.render();
    this.onDidChangeView.fire(view);
  }

  private setHidden(hidden: boolean): void {
    this.hidden = hidden;
    this.render();
    this.onDidChangeView.fire(this.visibleView);
  }

  private render(): void {
    this.sidebar.hidden = this.hidden;
    this.sash.hidden = this.hidden;
    for (const [id, el] of Object.entries(this.views)) el.hidden = id !== this.active;
    for (const button of this.activityBar.querySelectorAll<HTMLElement>('[data-view]')) {
      const active = button.dataset.view === this.active && !this.hidden;
      button.classList.toggle('active', active);
      button.setAttribute('aria-pressed', String(active));
    }
  }
}
