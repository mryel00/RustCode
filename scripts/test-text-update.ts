// Randomized check of computeTextUpdate: applying the update to the old lines
// must produce the new text. Run: node --experimental-strip-types scripts/test-text-update.ts
import { computeTextUpdate, type LineRange } from '../src/workbench/text-update.ts';

function apply(lines: string[], eol: string, range: LineRange, text: string): string {
  const offset = (line: number, column: number) => {
    let off = 0;
    for (let i = 0; i < line - 1; i++) off += lines[i].length + eol.length;
    return off + column - 1;
  };
  const whole = lines.join(eol);
  return whole.slice(0, offset(range.startLineNumber, range.startColumn)) + text + whole.slice(offset(range.endLineNumber, range.endColumn));
}

const pieces = ['a', 'b', 'cc', '', 'x y'];
const randomText = (breaks: string[]) => {
  const n = Math.floor(Math.random() * 6);
  let out = '';
  for (let i = 0; i < n; i++) {
    out += pieces[Math.floor(Math.random() * pieces.length)];
    if (Math.random() < 0.7) out += breaks[Math.floor(Math.random() * breaks.length)];
  }
  return out;
};

let checked = 0;
for (let i = 0; i < 20000; i++) {
  const oldEol = Math.random() < 0.5 ? '\n' : '\r\n';
  const oldLines = randomText(['\n']).split('\n');
  const text = randomText(Math.random() < 0.5 ? ['\n'] : ['\r\n', '\n', '\r']);
  const update = computeTextUpdate(oldLines, oldEol, text);
  const eol = update.eol ?? oldEol;
  const result = update.edit ? apply(oldLines, eol, update.edit.range, update.edit.text) : oldLines.join(eol);
  const expected = text.split(/\r\n|\r|\n/).join(eol);
  if (result !== expected) {
    console.error('mismatch', JSON.stringify({ oldLines, oldEol, text, update, result, expected }));
    process.exit(1);
  }
  checked++;
}
// Minimality: a change in the middle line touches only that line.
const mid = computeTextUpdate(['a', 'b', 'c'], '\n', 'a\nB\nc');
if (JSON.stringify(mid.edit) !== JSON.stringify({ range: { startLineNumber: 2, startColumn: 1, endLineNumber: 3, endColumn: 1 }, text: 'B\n' })) {
  console.error('not minimal', JSON.stringify(mid));
  process.exit(1);
}
if (computeTextUpdate(['a', 'b'], '\n', 'a\nb').edit !== undefined) process.exit(1);
console.log(`ok: ${checked} random cases`);
