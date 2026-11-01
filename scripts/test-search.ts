// Unit tests for the search helpers. Run: node --experimental-strip-types scripts/test-search.ts
import assert from 'node:assert/strict';
import { comparePaths, lcut, makePreview, matchId, previewParts, resultCountMessage } from '../src/workbench/search/matches.ts';
import { isMultilineQuery, ReplacePattern, type PatternInfo } from '../src/workbench/search/replace-pattern.ts';

const info = (pattern: string, isRegExp = false, extra: Partial<PatternInfo> = {}): PatternInfo => ({
  pattern,
  isRegExp,
  isCaseSensitive: false,
  isWordMatch: false,
  isMultiline: isMultilineQuery(pattern, isRegExp),
  ...extra,
});

// Plain replacements are taken literally, even with regex syntax in them.
assert.equal(new ReplacePattern('$1\\n', info('foo')).getReplaceString('foo'), '$1\\n');
assert.equal(new ReplacePattern('bar', info('foo')).getReplaceString('nothing'), null);

// Regex replacements: groups, escapes, $0, case operations.
assert.equal(new ReplacePattern('$1-', info('(\\w+)\\s', true)).getReplaceString('hello '), 'hello-');
assert.equal(new ReplacePattern('a\\nb\\tc\\\\', info('x', true)).getReplaceString('x'), 'a\nb\tc\\');
assert.equal(new ReplacePattern('[$0]', info('o+', true)).getReplaceString('oo'), '[oo]');
assert.equal(new ReplacePattern('\\U$1', info('(\\w+)', true)).getReplaceString('hello'), 'HELLO');
assert.equal(new ReplacePattern('\\u$1 \\L$2', info('(\\w+) (\\w+)', true)).getReplaceString('hello WORLD'), 'Hello world');
// A match inside a longer text (lookahead context) yields only the match's replacement.
assert.equal(new ReplacePattern('$1!', info('(b)(?=c)', true)).getReplaceString('abcd'), 'b!');
// $123 is not a group reference.
assert.equal(new ReplacePattern('$123', info('a', true)).hasParameters, false);
assert.equal(new ReplacePattern('$12', info('a', true)).hasParameters, true);

// Preserve case.
const pc = (match: string, replace: string) => new ReplacePattern(replace, info(match)).getReplaceString(match, true);
assert.equal(pc('FOO', 'bar'), 'BAR');
assert.equal(pc('foo', 'Bar'), 'bar');
assert.equal(pc('Foo', 'bar'), 'Bar');
assert.equal(pc('Foo-Bar', 'baz-qux'), 'Baz-Qux');
assert.equal(pc('foo_BAR', 'baz_qux'), 'baz_QUX');

// Whole word and case sensitivity carry over to the replace regex.
assert.equal(new ReplacePattern('x', info('foo', false, { isWordMatch: true })).getReplaceString('food'), null);
assert.equal(new ReplacePattern('x', info('Foo', false, { isCaseSensitive: true })).getReplaceString('foo'), null);
assert.throws(() => new ReplacePattern('x', info('(', true)));

// Multi-line detection follows Monaco.
assert.ok(isMultilineQuery('a\\nb', true) && isMultilineQuery('a\nb', false) && !isMultilineQuery('a\\nb', false));

// Previews: trimmed around the match without splitting surrogate pairs.
const long = 'a'.repeat(500) + 'needle' + 'b'.repeat(2000);
assert.deepEqual(makePreview(long, 500, 506, 1000), { text: long.slice(300, 1500), start: 200, end: 206 });
const emoji = '😀'.repeat(150) + 'x';
const p = makePreview(emoji, 300, 301, 1000);
assert.equal(p.text.slice(p.start, p.end), 'x');
assert.ok(!/^[\udc00-\udfff]/.test(p.text));
assert.deepEqual(makePreview('ab', 1, 5, 1000), { text: 'ab', start: 1, end: 2 });

// Display parts: the text before the match is cut at a word boundary.
const parts = previewParts({
  range: { start: { line: 0, character: 40 }, end: { line: 0, character: 46 } },
  preview: { text: 'const something = anotherThing + yetAnother(needle);', start: 44, end: 50 },
});
assert.equal(parts.inside, 'needle');
assert.equal(parts.after, ');');
assert.equal(parts.before, '…anotherThing + yetAnother(');
assert.equal(lcut('   short', 26), 'short');
assert.equal(matchId({ range: { start: { line: 1, character: 2 }, end: { line: 1, character: 5 } }, preview: { text: 'a foo', start: 2, end: 5 } }), '1:2-1:5>foo');

// Paths: natural order, files before subfolders.
const sorted = ['src/b/x.ts', 'src/a10.ts', 'README.md', 'src/a2.ts', 'a/z.ts', 'B.ts'].sort(comparePaths);
assert.deepEqual(sorted, ['B.ts', 'README.md', 'a/z.ts', 'src/a2.ts', 'src/a10.ts', 'src/b/x.ts']);

assert.equal(resultCountMessage(1, 1), '1 result in 1 file');
assert.equal(resultCountMessage(3, 2), '3 results in 2 files');

console.log('ok: search helpers');
