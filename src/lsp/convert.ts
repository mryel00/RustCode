// Conversions between LSP (0-based lines/UTF-16 columns) and Monaco (1-based).

import type * as L from 'vscode-languageserver-types';
import { monaco } from '../monaco/setup';

type CompletionItemKind = monaco.languages.CompletionItemKind;

export function toLspPosition(p: monaco.IPosition): L.Position {
  return { line: p.lineNumber - 1, character: p.column - 1 };
}

export function toMonacoRange(r: L.Range): monaco.IRange {
  return {
    startLineNumber: r.start.line + 1,
    startColumn: r.start.character + 1,
    endLineNumber: r.end.line + 1,
    endColumn: r.end.character + 1,
  };
}

/** Escapes text so markdown renders it literally, like VS Code's appendText. */
export function escapeMarkdown(text: string): string {
  return text.replace(/[\\`*_{}[\]()#+\-.!|<>~]/g, '\\$&').replace(/\n/g, '\n\n');
}

export function toMarkdown(content: string | L.MarkupContent | L.MarkedString, plainStrings = false): monaco.IMarkdownString {
  if (typeof content === 'string') {
    return { value: plainStrings ? escapeMarkdown(content) : content };
  }
  if ('kind' in content) {
    return { value: content.kind === 'markdown' ? content.value : escapeMarkdown(content.value) };
  }
  return { value: '```' + content.language + '\n' + content.value + '\n```' };
}

export function toHover(hover: L.Hover | null): monaco.languages.Hover | null {
  if (!hover) return null;
  const parts = Array.isArray(hover.contents) ? hover.contents : [hover.contents];
  const contents = parts.map((c) => toMarkdown(c)).filter((m) => m.value.trim() !== '');
  if (contents.length === 0) return null;
  return { contents, range: hover.range ? toMonacoRange(hover.range) : undefined };
}

const KIND: Record<number, keyof typeof monaco.languages.CompletionItemKind> = {
  1: 'Text', 2: 'Method', 3: 'Function', 4: 'Constructor', 5: 'Field', 6: 'Variable', 7: 'Class',
  8: 'Interface', 9: 'Module', 10: 'Property', 11: 'Unit', 12: 'Value', 13: 'Enum', 14: 'Keyword',
  15: 'Snippet', 16: 'Color', 17: 'File', 18: 'Reference', 19: 'Folder', 20: 'EnumMember',
  21: 'Constant', 22: 'Struct', 23: 'Event', 24: 'Operator', 25: 'TypeParameter',
};

export function toCompletionKind(kind: L.CompletionItemKind | undefined): CompletionItemKind {
  const name = kind !== undefined ? KIND[kind] : undefined;
  return monaco.languages.CompletionItemKind[name ?? 'Property'] as CompletionItemKind;
}

/** Monaco completion item carrying the original LSP item for `completionItem/resolve`. */
export interface LspCompletionItem extends monaco.languages.CompletionItem {
  lspItem: L.CompletionItem;
  docUri: string;
}

type EditRange = monaco.IRange | monaco.languages.CompletionItemRanges;

function toEditRange(r: L.Range | { insert: L.Range; replace: L.Range }): EditRange {
  return 'insert' in r ? { insert: toMonacoRange(r.insert), replace: toMonacoRange(r.replace) } : toMonacoRange(r);
}

export function toCompletionList(
  result: L.CompletionList | L.CompletionItem[] | null,
  model: monaco.editor.ITextModel,
  position: monaco.Position,
  docUri: string,
): monaco.languages.CompletionList {
  if (!result) return { suggestions: [] };
  const list: L.CompletionList = Array.isArray(result) ? { isIncomplete: false, items: result } : result;
  const defaults = list.itemDefaults;

  // Without an explicit edit range, replace the word around the cursor.
  const word = model.getWordUntilPosition(position);
  const wordAt = model.getWordAtPosition(position);
  const wordRange: EditRange = {
    insert: new monaco.Range(position.lineNumber, word.startColumn, position.lineNumber, position.column),
    replace: new monaco.Range(position.lineNumber, word.startColumn, position.lineNumber, wordAt?.endColumn ?? position.column),
  };
  const defaultRange = defaults?.editRange ? toEditRange(defaults.editRange) : wordRange;

  const suggestions = list.items.map((item): LspCompletionItem => {
    let insertText: string;
    let range: EditRange;
    if (item.textEdit) {
      insertText = item.textEdit.newText;
      range = toEditRange('range' in item.textEdit ? item.textEdit.range : item.textEdit);
    } else {
      insertText = item.textEditText ?? item.insertText ?? item.label;
      range = defaultRange;
    }
    const format = item.insertTextFormat ?? defaults?.insertTextFormat;
    const deprecated = item.deprecated || item.tags?.includes(1);
    return {
      label: item.labelDetails
        ? { label: item.label, detail: item.labelDetails.detail, description: item.labelDetails.description }
        : item.label,
      kind: toCompletionKind(item.kind),
      detail: item.detail,
      documentation: item.documentation ? toMarkdown(item.documentation, true) : undefined,
      sortText: item.sortText,
      filterText: item.filterText,
      preselect: item.preselect,
      insertText,
      insertTextRules: format === 2 ? monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet : undefined,
      range,
      commitCharacters: item.commitCharacters ?? defaults?.commitCharacters,
      additionalTextEdits: item.additionalTextEdits?.map((e) => ({ range: toMonacoRange(e.range), text: e.newText })),
      tags: deprecated ? [monaco.languages.CompletionItemTag.Deprecated] : undefined,
      lspItem: item,
      docUri,
    };
  });
  return { suggestions, incomplete: list.isIncomplete };
}

/** Copies the fields `completionItem/resolve` may fill in. */
export function applyResolved(item: LspCompletionItem, resolved: L.CompletionItem): LspCompletionItem {
  return {
    ...item,
    detail: resolved.detail ?? item.detail,
    documentation: resolved.documentation ? toMarkdown(resolved.documentation, true) : item.documentation,
    additionalTextEdits:
      resolved.additionalTextEdits?.map((e) => ({ range: toMonacoRange(e.range), text: e.newText })) ??
      item.additionalTextEdits,
    lspItem: resolved,
  };
}

export type LspDefinition = L.Location | L.Location[] | L.LocationLink[] | null;

/** Target URIs of a definition result, in order. */
export function definitionTargets(result: LspDefinition): string[] {
  if (!result) return [];
  const items = Array.isArray(result) ? result : [result];
  return items.map((d) => ('targetUri' in d ? d.targetUri : d.uri));
}

export function toLocationLinks(result: LspDefinition, resolveUri: (uri: string) => monaco.Uri | undefined): monaco.languages.LocationLink[] {
  if (!result) return [];
  const items = Array.isArray(result) ? result : [result];
  const links: monaco.languages.LocationLink[] = [];
  for (const d of items) {
    if ('targetUri' in d) {
      const uri = resolveUri(d.targetUri);
      if (!uri) continue;
      links.push({
        uri,
        range: toMonacoRange(d.targetRange),
        targetSelectionRange: toMonacoRange(d.targetSelectionRange),
        originSelectionRange: d.originSelectionRange ? toMonacoRange(d.originSelectionRange) : undefined,
      });
    } else {
      const uri = resolveUri(d.uri);
      if (uri) links.push({ uri, range: toMonacoRange(d.range) });
    }
  }
  return links;
}

const SEVERITY: Record<number, monaco.MarkerSeverity> = {
  1: monaco.MarkerSeverity.Error,
  2: monaco.MarkerSeverity.Warning,
  3: monaco.MarkerSeverity.Info,
  4: monaco.MarkerSeverity.Hint,
};

export function toMarker(d: L.Diagnostic): monaco.editor.IMarkerData {
  const code =
    d.code === undefined
      ? undefined
      : d.codeDescription
        ? { value: String(d.code), target: monaco.Uri.parse(d.codeDescription.href) }
        : String(d.code);
  return {
    ...toMonacoRange(d.range),
    // VS Code shows diagnostics without a severity as errors.
    severity: SEVERITY[d.severity ?? 1] ?? monaco.MarkerSeverity.Error,
    // LSP 3.18 allows markup messages; markers only show plain text.
    message: typeof d.message === 'string' ? d.message : d.message.value,
    source: d.source,
    code,
    tags: d.tags?.map((t) => (t === 1 ? monaco.MarkerTag.Unnecessary : monaco.MarkerTag.Deprecated)),
    relatedInformation: d.relatedInformation?.map((r) => ({
      resource: monaco.Uri.parse(r.location.uri),
      message: r.message,
      ...toMonacoRange(r.location.range),
    })),
  };
}
