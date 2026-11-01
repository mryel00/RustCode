// Minimal edit between a model's lines and new text, a port of VS Code's
// `ModelService._computeEdits`. Kept free of Monaco imports so it can be
// tested on its own (scripts/test-text-update.ts).

export interface LineRange {
  startLineNumber: number;
  startColumn: number;
  endLineNumber: number;
  endColumn: number;
}

export interface TextUpdate {
  /** New line ending for the model, if it changes. */
  eol?: '\n' | '\r\n';
  /** One replacement (1-based, Monaco style), if the lines differ. */
  edit?: { range: LineRange; text: string };
}

/**
 * Computes how to turn a model with `lines` (joined by `modelEol`) into
 * `text`: common leading and trailing lines are kept. The line ending of the
 * result is CRLF when most line breaks in `text` contain a CR, as in VS Code.
 */
export function computeTextUpdate(lines: string[], modelEol: string, text: string): TextUpdate {
  const next = text.split(/\r\n|\r|\n/);
  const breaks = next.length - 1;
  const crBreaks = (text.match(/\r/g) ?? []).length;
  const eol = breaks === 0 ? modelEol : crBreaks > breaks / 2 ? '\r\n' : '\n';
  const update: TextUpdate = {};
  if (eol !== modelEol) update.eol = eol as '\n' | '\r\n';

  const oldCount = lines.length;
  const newCount = next.length;
  let prefix = 0;
  while (prefix < Math.min(oldCount, newCount) && lines[prefix] === next[prefix]) prefix++;
  if (prefix === oldCount && prefix === newCount) return update;

  let suffix = 0;
  const maxSuffix = Math.min(oldCount - prefix, newCount - prefix);
  while (suffix < maxSuffix && lines[oldCount - 1 - suffix] === next[newCount - 1 - suffix]) suffix++;

  const maxColumn = (line: number) => lines[line - 1].length + 1;
  if (suffix > 0) {
    update.edit = {
      range: { startLineNumber: prefix + 1, startColumn: 1, endLineNumber: oldCount - suffix + 1, endColumn: 1 },
      text: next.slice(prefix, newCount - suffix).map((l) => l + eol).join(''),
    };
  } else if (prefix > 0) {
    update.edit = {
      range: { startLineNumber: prefix, startColumn: maxColumn(prefix), endLineNumber: oldCount, endColumn: maxColumn(oldCount) },
      text: next.slice(prefix).map((l) => eol + l).join(''),
    };
  } else {
    update.edit = {
      range: { startLineNumber: 1, startColumn: 1, endLineNumber: oldCount, endColumn: maxColumn(oldCount) },
      text: next.join(eol),
    };
  }
  return update;
}
