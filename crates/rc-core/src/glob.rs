//! VS Code's glob patterns and exclude expressions (`base/common/glob.ts`).
//!
//! An exclude expression is a settings object like `files.exclude`: glob
//! patterns mapped to `true`, `false` (disabled) or a sibling clause
//! (`{ "when": "$(basename).ts" }`). Keys starting with `!` are exclude
//! exceptions: paths they match, and everything beneath them, are never
//! excluded. Paths are relative to the folder and use `/`.

use globset::{GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use serde_json::Value;

/// Glob matching follows the file system's case sensitivity, like VS Code's
/// `ignoreGlobCase`.
pub const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// Folders that exceptions neither look into nor re-include unless an
/// exception names them (VS Code's `getFoldersSkippedByExcludeExceptions`).
const VERSION_CONTROL_FOLDERS: &[&str] = &[".git", ".svn", ".hg", ".jj"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Invalid glob pattern {pattern:?}: {message}")]
pub struct GlobError {
    pub pattern: String,
    pub message: String,
}

/// Compiles a VS Code glob: `*` and `?` stay within a path segment, `**`
/// spans segments, `{a,b}` and `[…]` as usual.
pub fn compile(pattern: &str) -> Result<GlobMatcher, GlobError> {
    build(pattern).map(|g| g.compile_matcher())
}

fn build(pattern: &str) -> Result<globset::Glob, GlobError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(CASE_INSENSITIVE)
        .build()
        .map_err(|e| GlobError { pattern: pattern.to_owned(), message: e.kind().to_string() })
}

fn build_set<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<GlobSet, GlobError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(build(pattern)?);
    }
    builder.build().map_err(|e| GlobError { pattern: String::new(), message: e.to_string() })
}

/// The value of one pattern in an exclude expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExcludeValue {
    /// `true`, or `false` to switch a pattern off (such as a default).
    Enabled(bool),
    /// `{ "when": "$(basename).ts" }`: only when that sibling file exists.
    When(String),
}

impl ExcludeValue {
    /// Reads a settings value. Like VS Code, anything that is neither a
    /// boolean nor a sibling clause counts as enabled.
    pub fn from_json(value: &Value) -> Self {
        match value {
            Value::Bool(b) => ExcludeValue::Enabled(*b),
            Value::Object(map) => match map.get("when") {
                Some(Value::String(when)) => ExcludeValue::When(when.clone()),
                _ => ExcludeValue::Enabled(true),
            },
            _ => ExcludeValue::Enabled(true),
        }
    }
}

/// An ordered exclude expression, as in a `files.exclude` setting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExcludeExpression {
    entries: Vec<(String, ExcludeValue)>,
}

impl ExcludeExpression {
    /// All `patterns` enabled.
    pub fn enabled(patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut expr = Self::default();
        for pattern in patterns {
            expr.set(pattern.into(), ExcludeValue::Enabled(true));
        }
        expr
    }

    /// Sets a pattern, replacing an earlier value for the same key.
    pub fn set(&mut self, pattern: String, value: ExcludeValue) {
        match self.entries.iter_mut().find(|(p, _)| *p == pattern) {
            Some(entry) => entry.1 = value,
            None => self.entries.push((pattern, value)),
        }
    }

    /// `self` with the keys of `other` laid over it (VS Code merges object
    /// settings key by key).
    pub fn merged(&self, other: &ExcludeExpression) -> ExcludeExpression {
        let mut out = self.clone();
        for (pattern, value) in &other.entries {
            out.set(pattern.clone(), value.clone());
        }
        out
    }

    pub fn entries(&self) -> &[(String, ExcludeValue)] {
        &self.entries
    }

    /// Enabled plain patterns, the only ones a ripgrep walk can apply (VS
    /// Code's `resolveExcludePatternsForProvider`).
    pub fn plain_patterns(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(p, v)| *v == ExcludeValue::Enabled(true) && !is_exception(p))
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// The patterns of enabled exceptions, without the `!` (VS Code's
    /// `getExcludeExceptions`). Absolute exceptions are ignored.
    pub fn exceptions(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(p, v)| *v == ExcludeValue::Enabled(true) && is_exception(p))
            .filter_map(|(p, _)| {
                let pattern = p.trim_start()[1..].trim().trim_end_matches('/');
                (!pattern.is_empty() && !is_absolute(pattern)).then(|| pattern.to_owned())
            })
            .collect()
    }

    pub fn parse(&self) -> Result<ParsedExclude, GlobError> {
        ParsedExclude::new(self)
    }
}

/// Whether a key is an exclude exception, i.e. starts with `!`.
pub fn is_exception(pattern: &str) -> bool {
    pattern.trim_start().starts_with('!')
}

/// Makes a leading `!` match literally (VS Code's `escapeExcludeException`):
/// patterns typed in the Search view are not settings.
pub fn escape_exception(pattern: &str) -> String {
    let trimmed = pattern.trim_start();
    match trimmed.strip_prefix('!') {
        Some(rest) => format!("{{!}}{rest}"),
        None => pattern.to_owned(),
    }
}

fn is_absolute(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    pattern.starts_with('/')
        || pattern.starts_with('\\')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// An exclude expression ready for matching.
pub struct ParsedExclude {
    plain: GlobSet,
    siblings: Vec<(GlobMatcher, String)>,
    exceptions: Vec<String>,
    /// Each exception, and everything beneath it.
    exception_set: GlobSet,
    exception_segments: Vec<Vec<Segment>>,
    skipped_folders: Vec<&'static str>,
}

enum Segment {
    Globstar,
    Glob(GlobMatcher),
    Never,
}

impl ParsedExclude {
    fn new(expr: &ExcludeExpression) -> Result<Self, GlobError> {
        let mut plain = Vec::new();
        let mut siblings = Vec::new();
        for (pattern, value) in &expr.entries {
            if is_exception(pattern) {
                continue;
            }
            match value {
                ExcludeValue::Enabled(false) => {}
                ExcludeValue::Enabled(true) => plain.push(pattern.as_str()),
                ExcludeValue::When(when) => siblings.push((compile(pattern)?, when.clone())),
            }
        }
        let exceptions = expr.exceptions();
        let exception_globs: Vec<String> = exceptions.iter().flat_map(|e| [e.clone(), format!("{e}/**")]).collect();
        let exception_set = build_set(exception_globs.iter().map(String::as_str))?;
        let exception_segments = exceptions.iter().map(|e| segment_matchers(e)).collect();
        let skipped_folders = folders_skipped_by_exceptions(&exceptions);
        Ok(Self { plain: build_set(plain)?, siblings, exceptions, exception_set, exception_segments, skipped_folders })
    }

    pub fn exceptions(&self) -> &[String] {
        &self.exceptions
    }

    /// Whether `path` matches an exclude pattern, ignoring exceptions.
    /// `has_sibling` answers whether a file of that name is next to `path`.
    pub fn matches_exclude(&self, path: &str, has_sibling: &dyn Fn(&str) -> bool) -> bool {
        if self.plain.is_match(path) {
            return true;
        }
        if self.siblings.is_empty() {
            return false;
        }
        let basename = path.rsplit('/').next().unwrap_or(path);
        // VS Code's `$(basename)` is the name without its extension.
        let name = match basename.rfind('.') {
            Some(dot) if dot > 0 => &basename[..dot],
            _ => basename,
        };
        self.siblings
            .iter()
            .any(|(glob, when)| glob.is_match(path) && has_sibling(&when.replacen("$(basename)", name, 1)))
    }

    /// Whether `path` or one of its parent folders matches an exception.
    /// Paths in version control folders are never excepted unless an
    /// exception names the folder.
    pub fn is_excepted(&self, path: &str) -> bool {
        !self.exceptions.is_empty() && self.exception_set.is_match(path) && !self.in_skipped_folder(path)
    }

    /// Excluded: matches a pattern and no exception covers it.
    pub fn matches(&self, path: &str, has_sibling: &dyn Fn(&str) -> bool) -> bool {
        self.matches_exclude(path, has_sibling) && !self.is_excepted(path)
    }

    /// Whether an exception could match something below `folder`, so an
    /// excluded folder must still be walked.
    pub fn could_exception_match_beneath(&self, folder: &str) -> bool {
        if self.exceptions.is_empty() || self.in_skipped_folder(folder) {
            return false;
        }
        let segments: Vec<&str> = folder.trim_end_matches('/').split('/').filter(|s| !s.is_empty()).collect();
        self.exception_segments.iter().any(|matchers| could_match_beneath(matchers, &segments))
    }

    fn in_skipped_folder(&self, path: &str) -> bool {
        path.split('/').any(|s| self.skipped_folders.iter().any(|f| equals(s, f)))
    }
}

fn equals(a: &str, b: &str) -> bool {
    if CASE_INSENSITIVE {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Version control folders that no exception names.
pub fn folders_skipped_by_exceptions(exceptions: &[String]) -> Vec<&'static str> {
    let named: Vec<String> =
        exceptions.iter().flat_map(|e| expand_braces(e)).flat_map(|p| split_glob_aware(&p, '/')).collect();
    VERSION_CONTROL_FOLDERS.iter().copied().filter(|f| !named.iter().any(|n| n == f)).collect()
}

/// One matcher per path segment of `pattern` (VS Code's `parseSegmentMatchers`).
fn segment_matchers(pattern: &str) -> Vec<Segment> {
    split_glob_aware(pattern, '/')
        .into_iter()
        .map(|segment| {
            if segment == "**" || segment.contains('/') || has_globstar_alternative(&segment) {
                Segment::Globstar
            } else {
                compile(&segment).map(Segment::Glob).unwrap_or(Segment::Never)
            }
        })
        .collect()
}

/// A brace alternative that is `**`, such as `{**,c}`, can span segments.
fn has_globstar_alternative(segment: &str) -> bool {
    let mut rest = segment;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else { break };
        if split_glob_aware(&rest[open + 1..open + close], ',').iter().any(|a| a == "**") {
            return true;
        }
        rest = &rest[open + close + 1..];
    }
    false
}

/// Whether a pattern could match a path starting with `folder` segments.
fn could_match_beneath(matchers: &[Segment], folder: &[&str]) -> bool {
    fn visit(m: &[Segment], f: &[&str], mi: usize, fi: usize, seen: &mut Vec<bool>) -> bool {
        if fi == f.len() {
            return true;
        }
        if mi == m.len() {
            return false;
        }
        let key = mi * (f.len() + 1) + fi;
        if seen[key] {
            return false;
        }
        seen[key] = true;
        match &m[mi] {
            Segment::Globstar => visit(m, f, mi + 1, fi, seen) || visit(m, f, mi, fi + 1, seen),
            Segment::Glob(g) => g.is_match(f[fi]) && visit(m, f, mi + 1, fi + 1, seen),
            Segment::Never => false,
        }
    }
    let mut seen = vec![false; (matchers.len() + 1) * (folder.len() + 1)];
    visit(matchers, folder, 0, 0, &mut seen)
}

/// How a walk of a folder tree treats an entry (VS Code's
/// `evaluateExcludeTreeEntry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeEntryState {
    /// It, or its parent folder, is excluded and no exception covers it.
    pub excluded: bool,
    /// Not excluded, or a folder an exception could match beneath.
    pub visited: bool,
}

pub fn evaluate_tree_entry(
    exclude: &ParsedExclude,
    path: &str,
    is_dir: bool,
    parent_excluded: bool,
    has_sibling: &dyn Fn(&str) -> bool,
) -> TreeEntryState {
    let matched = parent_excluded || exclude.matches_exclude(path, has_sibling);
    let excluded = matched && !exclude.is_excepted(path);
    TreeEntryState { excluded, visited: !excluded || (is_dir && exclude.could_exception_match_beneath(path)) }
}

/// VS Code's `glob.splitGlobAware`: splits on `split` outside `{}` and `[]`.
pub fn split_glob_aware(pattern: &str, split: char) -> Vec<String> {
    let mut segments = Vec::new();
    let (mut in_braces, mut in_brackets) = (false, false);
    let mut current = String::new();
    for c in pattern.chars() {
        if c == split && !in_braces && !in_brackets {
            segments.push(std::mem::take(&mut current));
            continue;
        }
        match c {
            '{' => in_braces = true,
            '}' => in_braces = false,
            '[' => in_brackets = true,
            ']' => in_brackets = false,
            _ => {}
        }
        current.push(c);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// Expands `{a,b}` alternatives (nested ones too); `\{` is literal.
pub fn expand_braces(pattern: &str) -> Vec<String> {
    let mut depth = 0usize;
    let mut open = 0;
    let mut escaped = false;
    for (i, c) in pattern.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '{' => {
                if depth == 0 {
                    open = i;
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let (prefix, inner, suffix) = (&pattern[..open], &pattern[open + 1..i], &pattern[i + 1..]);
                    return split_alternatives(inner)
                        .into_iter()
                        .flat_map(|alt| expand_braces(&format!("{prefix}{alt}{suffix}")))
                        .collect();
                }
            }
            _ => {}
        }
    }
    vec![pattern.to_owned()]
}

/// Splits brace contents on commas that are not nested or escaped.
fn split_alternatives(inner: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start, mut escaped) = (0usize, 0, false);
    for (i, c) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn expr(value: Value) -> ExcludeExpression {
        let mut e = ExcludeExpression::default();
        for (k, v) in value.as_object().unwrap() {
            e.set(k.clone(), ExcludeValue::from_json(v));
        }
        e
    }

    const NO_SIBLINGS: &dyn Fn(&str) -> bool = &|_| false;

    #[test]
    fn exceptions_override_excludes() {
        let e = expr(json!({ "**/node_modules": true, "!node_modules/keep": true, "!/abs": true, "!off": false }))
            .parse()
            .unwrap();
        assert_eq!(e.exceptions(), ["node_modules/keep"]);
        assert!(e.matches("node_modules", NO_SIBLINGS));
        assert!(e.matches_exclude("x/node_modules", NO_SIBLINGS));
        assert!(e.is_excepted("node_modules/keep"));
        assert!(e.is_excepted("node_modules/keep/deep/a.js"));
        assert!(!e.is_excepted("node_modules/keeper"));
        let both = expr(json!({ "**/*.log": true, "!logs/keep.log": true })).parse().unwrap();
        assert!(both.matches_exclude("logs/keep.log", NO_SIBLINGS));
        assert!(!both.matches("logs/keep.log", NO_SIBLINGS));
        assert!(both.matches("logs/other.log", NO_SIBLINGS));
    }

    #[test]
    fn tree_evaluation_walks_into_folders_leading_to_exceptions() {
        let e = expr(json!({ "**/build": true, "!build/keep/*.txt": true })).parse().unwrap();
        let eval = |path, is_dir, parent| evaluate_tree_entry(&e, path, is_dir, parent, NO_SIBLINGS);
        assert_eq!(eval("build", true, false), TreeEntryState { excluded: true, visited: true });
        assert_eq!(eval("build/other", true, true), TreeEntryState { excluded: true, visited: false });
        assert_eq!(eval("build/keep", true, true), TreeEntryState { excluded: true, visited: true });
        assert_eq!(eval("build/keep/a.txt", false, true), TreeEntryState { excluded: false, visited: true });
        assert_eq!(eval("build/keep/a.js", false, true), TreeEntryState { excluded: true, visited: false });
        assert_eq!(eval("src", true, false), TreeEntryState { excluded: false, visited: true });
        let any_depth = expr(json!({ "**/out": true, "!**/keep.txt": true })).parse().unwrap();
        assert!(any_depth.could_exception_match_beneath("a/b/out"));
    }

    #[test]
    fn version_control_folders_are_skipped_unless_named() {
        let e = expr(json!({ "**/.git": true, "!**/hooks": true })).parse().unwrap();
        assert!(!e.is_excepted(".git/hooks"));
        assert!(!e.could_exception_match_beneath(".git"));
        assert!(e.is_excepted("tools/hooks"));
        let named = expr(json!({ "**/.git": true, "!.git/hooks": true })).parse().unwrap();
        assert!(named.is_excepted(".git/hooks/pre-commit"));
        assert!(named.could_exception_match_beneath(".git"));
    }

    #[test]
    fn sibling_clauses() {
        let e = expr(json!({ "**/*.js": { "when": "$(basename).ts" } })).parse().unwrap();
        let siblings = |name: &str| name == "a.ts" || name == "x.min.ts";
        assert!(e.matches("src/a.js", &siblings));
        assert!(!e.matches("src/b.js", &siblings));
        assert!(e.matches("x.min.js", &siblings));
        assert!(expr(json!({ "**/*.js": { "other": 1 } })).parse().unwrap().matches("b.js", NO_SIBLINGS));
        // Sibling clauses and disabled patterns never reach the ripgrep walk.
        let mixed = expr(json!({ "a": true, "b": false, "c": { "when": "x" }, "!d": true }));
        assert_eq!(mixed.plain_patterns(), ["a"]);
    }

    #[test]
    fn helpers() {
        assert_eq!(escape_exception("!foo"), "{!}foo");
        assert_eq!(escape_exception("foo"), "foo");
        assert!(compile("{!}foo").unwrap().is_match("!foo"));
        assert_eq!(split_glob_aware("a,{b,c},[d,e]", ','), ["a", "{b,c}", "[d,e]"]);
        assert_eq!(expand_braces("a/{b,c{d,e}}/f"), ["a/b/f", "a/cd/f", "a/ce/f"]);
        let base = ExcludeExpression::enabled(["a", "b"]);
        let mut over = ExcludeExpression::default();
        over.set("a".into(), ExcludeValue::Enabled(false));
        over.set("c".into(), ExcludeValue::Enabled(true));
        assert_eq!(base.merged(&over).plain_patterns(), ["b", "c"]);
    }
}
