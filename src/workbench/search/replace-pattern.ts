// Replace strings for search results, a port of VS Code's `ReplacePattern`
// (workbench/services/search/common/replace.ts) and
// `buildReplaceStringWithCasePreserved` (base/common/search.ts). Like VS Code,
// replacements use JavaScript regular expression semantics. Kept free of
// Monaco imports so it can be tested on its own (scripts/test-search.ts).

export interface PatternInfo {
  pattern: string;
  isRegExp: boolean;
  isCaseSensitive: boolean;
  isWordMatch: boolean;
  isMultiline: boolean;
}

export function escapeRegExpCharacters(value: string): string {
  return value.replace(/[\\{}*+?|^$.[\]()]/g, '\\$&');
}

/** VS Code's `strings.createRegExp`. Throws for an empty or invalid pattern. */
export function createRegExp(
  searchString: string,
  isRegex: boolean,
  options: { matchCase?: boolean; wholeWord?: boolean; multiline?: boolean; global?: boolean; unicode?: boolean } = {},
): RegExp {
  if (!searchString) throw new Error('Cannot create regex from empty string');
  if (!isRegex) searchString = escapeRegExpCharacters(searchString);
  if (options.wholeWord) {
    if (!/\B/.test(searchString.charAt(0))) searchString = '\\b' + searchString;
    if (!/\B/.test(searchString.charAt(searchString.length - 1))) searchString = searchString + '\\b';
  }
  let modifiers = '';
  if (options.global) modifiers += 'g';
  if (!options.matchCase) modifiers += 'i';
  if (options.multiline) modifiers += 'm';
  if (options.unicode) modifiers += 'u';
  return new RegExp(searchString, modifiers);
}

/** Monaco's `isMultilineRegexSource`: a line feed, or `\n`, `\r` or `\W`. */
export function isMultilineRegexSource(searchString: string): boolean {
  for (let i = 0; i < searchString.length; i++) {
    const ch = searchString[i];
    if (ch === '\n') return true;
    if (ch === '\\') {
      i++;
      if (i >= searchString.length) break;
      const next = searchString[i];
      if (next === 'n' || next === 'r' || next === 'W') return true;
    }
  }
  return false;
}

/** VS Code's `QueryBuilder.isMultiline`. */
export function isMultilineQuery(pattern: string, isRegExp: boolean): boolean {
  return (isRegExp && isMultilineRegexSource(pattern)) || pattern.includes('\n');
}

export function buildReplaceStringWithCasePreserved(matches: string[] | null, pattern: string): string {
  if (matches && matches[0] !== '') {
    const containsHyphens = validateSpecificSpecialCharacter(matches, pattern, '-');
    const containsUnderscores = validateSpecificSpecialCharacter(matches, pattern, '_');
    if (containsHyphens && !containsUnderscores) {
      return buildReplaceStringForSpecificSpecialCharacter(matches, pattern, '-');
    } else if (!containsHyphens && containsUnderscores) {
      return buildReplaceStringForSpecificSpecialCharacter(matches, pattern, '_');
    }
    if (matches[0].toUpperCase() === matches[0]) {
      return pattern.toUpperCase();
    } else if (matches[0].toLowerCase() === matches[0]) {
      return pattern.toLowerCase();
    } else if (containsUppercaseCharacter(matches[0][0]) && pattern.length > 0) {
      return pattern[0].toUpperCase() + pattern.substr(1);
    } else if (matches[0][0].toUpperCase() !== matches[0][0] && pattern.length > 0) {
      return pattern[0].toLowerCase() + pattern.substr(1);
    }
    // We don't understand its pattern yet.
    return pattern;
  }
  return pattern;
}

function containsUppercaseCharacter(target: string): boolean {
  return !!target && target.toLowerCase() !== target;
}

function validateSpecificSpecialCharacter(matches: string[], pattern: string, ch: string): boolean {
  const contains = matches[0].indexOf(ch) !== -1 && pattern.indexOf(ch) !== -1;
  return contains && matches[0].split(ch).length === pattern.split(ch).length;
}

function buildReplaceStringForSpecificSpecialCharacter(matches: string[], pattern: string, ch: string): string {
  const patternParts = pattern.split(ch);
  const matchParts = matches[0].split(ch);
  let result = '';
  patternParts.forEach((part, index) => {
    result += buildReplaceStringWithCasePreserved([matchParts[index]], part) + ch;
  });
  return result.slice(0, -1);
}

export class ReplacePattern {
  private replacePattern: string;
  private parameters = false;
  private regExp: RegExp;
  private readonly caseOpsRegExp = /([\s\S]*?)((?:\\[uUlL])+?|)(\$[0-9]+)([\s\S]*?)/g;

  /** Throws if the search pattern is not a valid JavaScript regular expression. */
  constructor(replaceString: string, info: PatternInfo) {
    this.replacePattern = replaceString;
    this.regExp = createRegExp(info.pattern, info.isRegExp, {
      matchCase: info.isCaseSensitive,
      wholeWord: info.isWordMatch,
      multiline: info.isMultiline,
      global: false,
      unicode: true,
    });
    if (info.isRegExp) this.parseReplaceString(replaceString);
  }

  get hasParameters(): boolean {
    return this.parameters;
  }

  get pattern(): string {
    return this.replacePattern;
  }

  /** The replace string for the first match in `text`, or null if nothing matches. */
  getReplaceString(text: string, preserveCase?: boolean): string | null {
    this.regExp.lastIndex = 0;
    const match = this.regExp.exec(text);
    if (!match) return null;
    if (this.parameters) {
      const replaced = this.replaceWithCaseOperations(text, this.regExp, this.buildReplaceString(match, preserveCase));
      if (match[0] === text) return replaced;
      return replaced.substr(match.index, match[0].length - (text.length - replaced.length));
    }
    return this.buildReplaceString(match, preserveCase);
  }

  /**
   * Applies `\u` `\U` `\l` `\L` case operations to `$N` references, then
   * lets `String.replace` substitute the rest.
   */
  private replaceWithCaseOperations(text: string, regex: RegExp, replaceString: string): string {
    if (!/\\[uUlL]/.test(replaceString)) return text.replace(regex, replaceString);
    const firstMatch = regex.exec(text);
    if (firstMatch === null) return text.replace(regex, replaceString);

    let patMatch: RegExpExecArray | null;
    let newReplaceString = '';
    let lastIndex = 0;
    let lastMatch = '';
    this.caseOpsRegExp.lastIndex = 0;
    while ((patMatch = this.caseOpsRegExp.exec(replaceString)) !== null) {
      lastIndex = patMatch.index;
      const fullMatch = patMatch[0];
      lastMatch = fullMatch;
      let caseOps = patMatch[2];
      const money = patMatch[3];
      if (!caseOps) {
        newReplaceString += fullMatch;
        continue;
      }
      const replacement = firstMatch[parseInt(money.slice(1))];
      if (!replacement) {
        newReplaceString += fullMatch;
        continue;
      }
      const replacementLen = replacement.length;
      newReplaceString += patMatch[1];
      caseOps = caseOps.replace(/\\/g, '');
      let i = 0;
      for (; i < caseOps.length; i++) {
        switch (caseOps[i]) {
          case 'U':
            newReplaceString += replacement.slice(i).toUpperCase();
            i = replacementLen;
            break;
          case 'u':
            newReplaceString += replacement[i].toUpperCase();
            break;
          case 'L':
            newReplaceString += replacement.slice(i).toLowerCase();
            i = replacementLen;
            break;
          case 'l':
            newReplaceString += replacement[i].toLowerCase();
            break;
        }
      }
      if (i < replacementLen) newReplaceString += replacement.slice(i);
      newReplaceString += patMatch[4];
    }
    newReplaceString += replaceString.slice(lastIndex + lastMatch.length);
    return text.replace(regex, newReplaceString);
  }

  private buildReplaceString(matches: string[] | null, preserveCase?: boolean): string {
    return preserveCase ? buildReplaceStringWithCasePreserved(matches, this.replacePattern) : this.replacePattern;
  }

  /** `\n` → LF, `\t` → TAB, `\\` → `\`, `$0` → `$&`; notes `$n` references. */
  private parseReplaceString(replaceString: string): void {
    if (!replaceString) return;
    let substrFrom = 0;
    let result = '';
    const len = replaceString.length;
    const isDigit = (c: string, from: string) => c >= from && c <= '9';
    for (let i = 0; i < len; i++) {
      const ch = replaceString[i];
      if (ch === '\\') {
        i++;
        if (i >= len) break;
        const next = replaceString[i];
        const replacement = next === '\\' ? '\\' : next === 'n' ? '\n' : next === 't' ? '\t' : null;
        if (replacement) {
          result += replaceString.substring(substrFrom, i - 1) + replacement;
          substrFrom = i + 1;
        }
      }
      if (ch === '$') {
        i++;
        if (i >= len) break;
        const next = replaceString[i];
        let replacement: string | null = null;
        if (next === '0') {
          replacement = '$&';
          this.parameters = true;
        } else if (next === '`' || next === "'") {
          this.parameters = true;
        } else if (isDigit(next, '1')) {
          // $n with 1 <= n <= 99
          if (i === len - 1) {
            this.parameters = true;
          } else if (!isDigit(replaceString[++i], '0')) {
            this.parameters = true;
            --i;
          } else if (i === len - 1) {
            this.parameters = true;
          } else if (!isDigit(replaceString[++i], '0')) {
            this.parameters = true;
            --i;
          }
        }
        if (replacement) {
          result += replaceString.substring(substrFrom, i - 1) + replacement;
          substrFrom = i + 1;
        }
      }
    }
    if (substrFrom === 0) return;
    this.replacePattern = result + replaceString.substring(substrFrom);
  }
}
