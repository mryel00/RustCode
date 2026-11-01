// Monaco bootstrap: editor features, syntax highlighting, worker and theme.

import './features';
import * as monaco from 'monaco-esm/editor/editor.api.js';
import EditorWorker from 'monaco-esm/editor/editor.worker.js?worker';

self.MonacoEnvironment = {
  getWorker: () => new EditorWorker(),
};

monaco.editor.defineTheme('rustcode-dark', {
  base: 'vs-dark',
  inherit: true,
  rules: [],
  colors: {
    'editor.background': '#1f1f1f',
    'editor.lineHighlightBackground': '#ffffff0a',
    'editorGutter.background': '#1f1f1f',
    'editorLineNumber.foreground': '#6e7681',
    'editorLineNumber.activeForeground': '#cccccc',
    'editorWidget.background': '#202020',
    'editorWidget.border': '#454545',
    'editorHoverWidget.background': '#202020',
    'editorSuggestWidget.background': '#202020',
    'editorSuggestWidget.selectedBackground': '#04395e',
  },
});

/** Maps an LSP language id to a Monaco language id with a tokenizer, if any. */
export function monacoLanguage(languageId: string): string {
  const aliases: Record<string, string> = {
    typescriptreact: 'typescript',
    javascriptreact: 'javascript',
    shellscript: 'shell',
    jsonc: 'json',
  };
  const id = aliases[languageId] ?? languageId;
  return monaco.languages.getLanguages().some((l) => l.id === id) ? id : 'plaintext';
}

export { monaco };
