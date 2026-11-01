// Status bar and toast notifications.

import type { LanguageFeatures, ProblemCounts } from '../lsp/features';
import { monaco } from '../monaco/setup';
import type { Doc } from './documents';

export class StatusBar {
  private readonly problems = document.getElementById('status-problems')!;
  private readonly servers = document.getElementById('status-servers')!;
  private readonly cursor = document.getElementById('status-cursor')!;
  private readonly eol = document.getElementById('status-eol')!;
  private readonly language = document.getElementById('status-language')!;

  constructor(private readonly features: LanguageFeatures) {
    features.onDidChangeServers.on(() => this.renderServers());
    features.onDidChangeProblems.on((c) => this.renderProblems(c));
    this.renderProblems({ errors: 0, warnings: 0 });
  }

  setDocument(doc: Doc | null, editor: monaco.editor.ICodeEditor): void {
    if (!doc) {
      this.cursor.textContent = this.eol.textContent = this.language.textContent = '';
      return;
    }
    const pos = editor.getPosition();
    this.cursor.textContent = pos ? `Ln ${pos.lineNumber}, Col ${pos.column}` : '';
    this.eol.textContent = doc.model.getEOL() === '\r\n' ? 'CRLF' : 'LF';
    this.language.textContent = doc.languageId;
  }

  private renderProblems({ errors, warnings }: ProblemCounts): void {
    this.problems.innerHTML =
      `<span class="codicon codicon-error"></span> ${errors} ` + `<span class="codicon codicon-warning"></span> ${warnings}`;
  }

  private renderServers(): void {
    const parts: HTMLElement[] = [];
    for (const [name, { status, progress }] of this.features.servers) {
      const el = document.createElement('span');
      el.className = 'server-status';
      let icon = 'codicon-loading codicon-modifier-spin';
      let text = name;
      let title = name;
      switch (status.state) {
        case 'ready':
          icon = progress ? 'codicon-sync codicon-modifier-spin' : 'codicon-check';
          text = progress ? `${name}: ${progress}` : name;
          title = `${name} is running`;
          break;
        case 'starting':
          title = `${name} is starting`;
          break;
        case 'notInstalled':
          icon = 'codicon-circle-slash';
          title = `${status.command} was not found on PATH`;
          text = `${name} not installed`;
          break;
        case 'failed':
          icon = 'codicon-error';
          title = status.message;
          break;
        case 'exited':
          icon = status.restarting ? 'codicon-loading codicon-modifier-spin' : 'codicon-debug-stop';
          title = `${name} exited (code ${status.code ?? 'unknown'}). ${status.message}`;
          break;
      }
      el.title = title;
      const iconEl = document.createElement('span');
      iconEl.className = `codicon ${icon}`;
      el.append(iconEl, ` ${text}`);
      parts.push(el);
    }
    this.servers.replaceChildren(...parts);
  }
}

export function toast(message: string, level: 'error' | 'warning' | 'info' = 'info'): void {
  const container = document.getElementById('toasts')!;
  const el = document.createElement('div');
  el.className = `toast toast-${level}`;
  const icon = document.createElement('span');
  icon.className = `codicon codicon-${level === 'info' ? 'info' : level}`;
  const text = document.createElement('span');
  text.textContent = message;
  el.append(icon, text);
  el.addEventListener('click', () => el.remove());
  container.append(el);
  while (container.children.length > 4) container.firstElementChild?.remove();
  setTimeout(() => el.remove(), level === 'error' ? 10000 : 5000);
}
