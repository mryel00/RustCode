// Search result helpers shared by the search model and view. Kept free of
// Monaco imports so they can be tested on their own (scripts/test-search.ts).

import type { SearchPreview, TextMatch } from '../../backend';

/** VS Code's `strings.containsUppercaseCharacter`, used by `search.smartCase`. */
export function containsUppercaseCharacter(target: string, ignoreEscapedChars = false): boolean {
  if (!target) return false;
  if (ignoreEscapedChars) target = target.replace(/\\./g, '');
  return target.toLowerCase() !== target;
}

/**
 * VS Code's `QueryBuilder.isCaseSensitive`: with `search.smartCase`, a
 * pattern with an uppercase letter (not escaped, for regexes) matches case.
 */
export function effectiveCaseSensitive(pattern: string, isRegex: boolean, matchCase: boolean, smartCase: boolean): boolean {
  if (smartCase && containsUppercaseCharacter(pattern, isRegex)) return true;
  return matchCase;
}

/** VS Code's `MatchImpl.MAX_PREVIEW_CHARS`. */
const MAX_PREVIEW_CHARS = 250;

/** VS Code's Search view preview width: `isRegExp ? 10000 : 1000`. */
export function charsPerLine(isRegex: boolean): number {
  return isRegex ? 10000 : 1000;
}

/**
 * A one-line preview like VS Code's `TextSearchMatch` (and the backend's):
 * up to a fifth of `perLine` before the match, `perLine` from its start.
 * `start`/`end` are UTF-16 offsets into `line`; `end` may lie past it.
 */
export function makePreview(line: string, start: number, end: number, perLine: number): SearchPreview {
  let from = Math.max(start - Math.floor(perLine / 5), 0);
  let to = Math.min(start + perLine, line.length);
  // Do not split surrogate pairs.
  if (from > 0 && isLowSurrogate(line.charCodeAt(from)) && isHighSurrogate(line.charCodeAt(from - 1))) from--;
  if (to < line.length && isLowSurrogate(line.charCodeAt(to)) && isHighSurrogate(line.charCodeAt(to - 1))) to++;
  const text = line.slice(from, Math.max(to, from));
  return { text, start: start - from, end: Math.min(Math.max(end - from, 0), text.length) };
}

function isHighSurrogate(code: number): boolean {
  return code >= 0xd800 && code <= 0xdbff;
}

function isLowSurrogate(code: number): boolean {
  return code >= 0xdc00 && code <= 0xdfff;
}

/** The matched text (all of it for multi-line matches). */
export function matchText(match: TextMatch): string {
  return match.text ?? match.preview.text.slice(match.preview.start, match.preview.end);
}

/**
 * Identifies a match within its file, like VS Code's match ids (range +
 * text). Uses the previewed text, which the backend and the editor models
 * compute alike.
 */
export function matchId(match: TextMatch): string {
  const { start, end } = match.range;
  const shown = match.preview.text.slice(match.preview.start, match.preview.end);
  return `${start.line}:${start.character}-${end.line}:${end.character}>${shown}`;
}

export interface PreviewParts {
  before: string;
  fullBefore: string;
  inside: string;
  after: string;
}

/** VS Code's `MatchImpl.preview()`: the text around a match, shortened for display. */
export function previewParts(match: TextMatch): PreviewParts {
  const { text, start, end } = match.preview;
  const fullBefore = text.substring(0, start);
  const before = lcut(fullBefore, 26, '…');
  let inside = text.substring(start, end);
  let after = text.substring(end);
  let remaining = MAX_PREVIEW_CHARS - before.length;
  inside = inside.substr(0, remaining);
  remaining -= inside.length;
  after = after.substr(0, Math.max(remaining, 0));
  return { before, fullBefore, inside, after };
}

/**
 * VS Code's `strings.lcut`: keeps at most about `n` characters from the end
 * of `text`, cutting at a word boundary.
 */
export function lcut(text: string, n: number, prefix = ''): string {
  const trimmed = text.trimStart();
  if (trimmed.length < n) return trimmed;
  const re = /\b/g;
  let i = 0;
  while (re.test(trimmed)) {
    if (trimmed.length - re.lastIndex < n) break;
    i = re.lastIndex;
    re.lastIndex += 1;
  }
  if (i === 0) return trimmed;
  return prefix + trimmed.substring(i).trimStart();
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

/** VS Code's `compareFileNames`: natural order, ignoring case. */
function compareFileNames(a: string, b: string): number {
  const result = collator.compare(a, b);
  return result !== 0 ? result : a < b ? -1 : a > b ? 1 : 0;
}

/**
 * VS Code's `comparePaths` for `/`-separated relative paths: segment by
 * segment, with the files of a folder before its subfolders.
 */
export function comparePaths(one: string, other: string): number {
  const a = one.split('/');
  const b = other.split('/');
  for (let i = 0; ; i++) {
    const endA = i === a.length - 1;
    const endB = i === b.length - 1;
    if (endA && endB) return compareFileNames(a[i], b[i]);
    if (endA) return -1;
    if (endB) return 1;
    const result = compareFileNames(a[i], b[i]);
    if (result !== 0) return result;
  }
}

/** "3 results in 1 file", as in VS Code. */
export function resultCountMessage(matches: number, files: number): string {
  return `${matches} ${matches === 1 ? 'result' : 'results'} in ${files} ${files === 1 ? 'file' : 'files'}`;
}

function occurrences(matches: number, files: number): string {
  return `${matches} ${matches === 1 ? 'occurrence' : 'occurrences'} across ${files} ${files === 1 ? 'file' : 'files'}`;
}

/** VS Code's Replace All confirmation. */
export function replaceAllConfirmation(matches: number, files: number, replaceValue: string): string {
  return replaceValue
    ? `Replace ${occurrences(matches, files)} with '${replaceValue}'?`
    : `Replace ${occurrences(matches, files)}?`;
}

/** VS Code's message after Replace All. */
export function replaceAllDone(matches: number, files: number, replaceValue: string): string {
  return replaceValue
    ? `Replaced ${occurrences(matches, files)} with '${replaceValue}'.`
    : `Replaced ${occurrences(matches, files)}.`;
}
