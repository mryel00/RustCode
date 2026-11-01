// Drag handle that resizes the sidebar.

const KEY = 'rustcode.sidebarWidth';

export function initSash(sash: HTMLElement, sidebar: HTMLElement): void {
  try {
    const saved = Number(localStorage.getItem(KEY));
    if (saved >= 160) sidebar.style.width = `${saved}px`;
  } catch {
    // Storage unavailable: keep the default width.
  }
  sash.addEventListener('pointerdown', (down) => {
    down.preventDefault();
    sash.setPointerCapture(down.pointerId);
    sash.classList.add('dragging');
    const startX = down.clientX;
    const startWidth = sidebar.getBoundingClientRect().width;
    const move = (e: PointerEvent) => {
      const width = Math.min(Math.max(startWidth + e.clientX - startX, 160), window.innerWidth * 0.6);
      sidebar.style.width = `${width}px`;
    };
    const up = () => {
      sash.classList.remove('dragging');
      sash.removeEventListener('pointermove', move);
      sash.removeEventListener('pointerup', up);
      try {
        localStorage.setItem(KEY, String(Math.round(sidebar.getBoundingClientRect().width)));
      } catch {
        // ignore
      }
    };
    sash.addEventListener('pointermove', move);
    sash.addEventListener('pointerup', up);
  });
}
