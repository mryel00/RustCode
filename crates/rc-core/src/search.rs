//! Text search across the open folder ("Find in Files").
//!
//! Modelled on VS Code, which runs ripgrep: the query is turned into the same
//! walk and matching options VS Code passes to `rg` (`ripgrepTextSearchEngine.ts`,
//! `getRgArgs`), and include/exclude patterns are expanded the way its
//! `QueryBuilder` does. The walk uses ripgrep's own `ignore` crate, so
//! `.gitignore`, `.ignore`, `.rgignore` and `-g` style globs behave exactly as
//! with `rg`.
//!
//! Exclude exceptions (`!`-prefixed keys of `files.exclude`/`search.exclude`)
//! re-include paths that other excludes or ignore files leave out. ripgrep
//! does not descend into excluded folders, so, like VS Code, a second walk
//! visits only the paths the exceptions lead to.
//!
//! Positions are reported as (line, UTF-16 column), like every other position
//! in RustCode. Line breaks are `\n`, `\r\n` and `\r`.

use crate::{
    glob::{self, expand_braces, split_glob_aware, ExcludeExpression, ExcludeValue, ParsedExclude},
    settings::Settings,
    uri,
};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::{
    overrides::{Override, OverrideBuilder},
    WalkBuilder, WalkState,
};
use lsp_types::{Position, Range};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
};
use url::Url;

/// VS Code's default `search.exclude` (on top of `files.exclude`).
pub const SEARCH_EXCLUDE: &[&str] = &["**/node_modules", "**/bower_components", "**/*.code-search"];

/// VS Code's `search.maxResults` default.
pub const DEFAULT_MAX_RESULTS: usize = 20_000;

/// Longest full match text sent along with a multi-line match.
const MAX_MATCH_TEXT: usize = 10_000;

const GLOB_CASE_INSENSITIVE: bool = glob::CASE_INSENSITIVE;

/// A search as entered in the Search view.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSearchQuery {
    pub pattern: String,
    #[serde(default)]
    pub is_regex: bool,
    #[serde(default)]
    pub is_case_sensitive: bool,
    #[serde(default)]
    pub is_word_match: bool,
    /// "files to include", comma separated (VS Code syntax).
    #[serde(default)]
    pub includes: String,
    /// "files to exclude", comma separated (VS Code syntax).
    #[serde(default)]
    pub excludes: String,
    /// VS Code's "Use Exclude Settings and Ignore Files" toggle.
    #[serde(default = "default_true")]
    pub use_exclude_settings_and_ignore_files: bool,
    #[serde(default)]
    pub max_results: Option<usize>,
}

fn default_true() -> bool {
    true
}

impl TextSearchQuery {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            is_regex: false,
            is_case_sensitive: false,
            is_word_match: false,
            includes: String::new(),
            excludes: String::new(),
            use_exclude_settings_and_ignore_files: true,
            max_results: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SearchError(pub String);

/// One match. `range` is in document coordinates; `preview` is the start of
/// the match's first line, trimmed like VS Code's search previews.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMatch {
    pub range: Range,
    pub preview: Preview,
    /// The full matched text, for matches spanning several lines (the
    /// preview only shows the first line). Capped at 10 000 bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A piece of the match's first line; `start..end` (UTF-16 offsets into
/// `text`) is the matched part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub text: String,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMatches {
    pub uri: Url,
    pub path: String,
    pub matches: Vec<TextMatch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchComplete {
    /// More matches exist than were reported (`maxResults` was reached).
    pub limit_hit: bool,
}

/// A compiled search over one folder.
pub struct TextSearch {
    root: PathBuf,
    matcher: Matcher,
    multiline: bool,
    chars_per_line: u32,
    /// The ripgrep `-g` globs VS Code passes: a pre-filter for the walk.
    overrides: Override,
    /// VS Code's own glob check of every result (`QueryGlobTester`): the
    /// globs above can let through more than the patterns mean.
    filter: ResultFilter,
    /// Globs of the second walk, which visits what exclude exceptions
    /// re-include (`None` without exceptions).
    exception_overrides: Option<Override>,
    /// Local (`.gitignore`, `.ignore`, `.rgignore`), parent folder and global
    /// ignore files.
    ignore_files: (bool, bool, bool),
    follow_symlinks: bool,
    max_results: usize,
    /// The whole folder is excluded (`./` in "files to exclude").
    root_excluded: bool,
}

impl TextSearch {
    /// Compiles `query` for searching `root` (an absolute, canonical folder).
    /// From `settings` come `files.exclude` and `search.exclude` (unless the
    /// query turns exclude settings off), the ignore file and symlink
    /// settings, and `search.maxResults` when the query sets no limit.
    pub fn new(root: &Path, query: &TextSearchQuery, settings: &Settings) -> Result<Self, SearchError> {
        if query.pattern.is_empty() {
            return Err(SearchError("the search pattern is empty".into()));
        }
        let (source, multiline) = regex_source(query);
        let matcher = Matcher::compile(&source, query.is_case_sensitive)?;

        let includes = parse_patterns(&query.includes, root)?;
        let excludes = parse_patterns(&query.excludes, root)?;
        // Typed patterns are not settings: a leading `!` is literal there.
        let mut exclude_expr = ExcludeExpression::default();
        for pattern in &excludes.globs {
            exclude_expr.set(glob::escape_exception(pattern), ExcludeValue::Enabled(true));
        }
        let use_settings = query.use_exclude_settings_and_ignore_files;
        if use_settings {
            exclude_expr = exclude_expr.merged(&settings.search_excludes());
        }
        let include_globs = if includes.whole_folder { Vec::new() } else { includes.globs };
        let overrides = build_overrides(root, &include_globs, &exclude_expr.plain_patterns())?;
        let parsed = exclude_expr.parse().map_err(|e| SearchError(e.to_string()))?;
        let exceptions = exclude_expr.exceptions();
        let exception_overrides =
            if exceptions.is_empty() { None } else { Some(build_exception_overrides(root, &exceptions)?) };
        let filter = ResultFilter::new(&include_globs, parsed)?;

        Ok(Self {
            root: root.to_path_buf(),
            matcher,
            multiline,
            // VS Code's Search view previews: `charsPerLine = isRegExp ? 10000 : 1000`.
            chars_per_line: if query.is_regex { 10_000 } else { 1_000 },
            overrides,
            filter,
            exception_overrides,
            ignore_files: {
                let local = use_settings && settings.search.use_ignore_files;
                (
                    local,
                    local && settings.search.use_parent_ignore_files,
                    local && settings.search.use_global_ignore_files,
                )
            },
            follow_symlinks: settings.search.follow_symlinks,
            max_results: query.max_results.or(settings.search.max_results).unwrap_or(usize::MAX),
            root_excluded: excludes.whole_folder,
        })
    }

    /// Whether a file at `path` is covered by the query's folder and
    /// include/exclude patterns. Ignore files are not consulted: like VS Code,
    /// open documents are searched even when they are git-ignored.
    pub fn in_scope(&self, path: &Path) -> bool {
        if self.root_excluded {
            return false;
        }
        let Ok(rel) = path.strip_prefix(&self.root) else { return false };
        if rel.as_os_str().is_empty() {
            return false;
        }
        // What an exception re-includes stays in, whatever excludes its folders.
        if !self.filter.is_excepted(rel) {
            let components: Vec<_> = rel.components().collect();
            let mut current = self.root.clone();
            for (i, component) in components.iter().enumerate() {
                current.push(component);
                if self.overrides.matched(&current, i + 1 < components.len()).is_ignore() {
                    return false;
                }
            }
        }
        self.filter.includes(path, rel)
    }

    /// Searches the folder on disk, calling `on_file` (from several threads)
    /// for every file with matches. Files in `skip` (open documents, which
    /// are searched in their editors) are left out.
    pub fn run<F>(&self, skip: &HashSet<PathBuf>, cancel: &AtomicBool, on_file: F) -> SearchComplete
    where
        F: Fn(FileMatches) + Sync,
    {
        if self.root_excluded {
            return SearchComplete { limit_hit: false };
        }
        let mut state = RunState {
            total: AtomicUsize::new(0),
            limit_hit: AtomicBool::new(false),
            reported: self.exception_overrides.as_ref().map(|_| Mutex::new(HashSet::new())),
        };
        let (use_ignore, use_parent, use_global) = self.ignore_files;

        // The equivalent of VS Code's `rg --hidden --no-require-git [--follow]
        // [--no-ignore-parent] [--no-ignore-global] [--no-ignore] -g …`.
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .hidden(false)
            .parents(use_parent)
            .ignore(use_ignore)
            .git_ignore(use_ignore)
            .git_exclude(use_ignore)
            .git_global(use_global)
            .require_git(false)
            .follow_links(self.follow_symlinks)
            .ignore_case_insensitive(GLOB_CASE_INSENSITIVE)
            .overrides(self.overrides.clone())
            .threads(0);
        if use_ignore {
            builder.add_custom_ignore_filename(".rgignore");
        }
        self.walk(&builder, &state, skip, cancel, &on_file, &|path, rel| self.filter.includes(path, rel));

        // VS Code's exception run: only the paths that exceptions lead to,
        // without ignore files (exceptions override them), skipping files
        // the first walk reported.
        if let Some(overrides) = &self.exception_overrides
            && !state.limit_hit.load(Ordering::Relaxed)
            && !cancel.load(Ordering::Relaxed)
        {
            let reported = state.reported.take().map(|r| r.into_inner().unwrap()).unwrap_or_default();
            let mut builder = WalkBuilder::new(&self.root);
            builder
                .standard_filters(false)
                .parents(false)
                .require_git(false)
                .follow_links(self.follow_symlinks)
                .overrides(overrides.clone())
                .threads(0);
            self.walk(&builder, &state, skip, cancel, &on_file, &|path, rel| {
                !reported.contains(path) && self.filter.is_excepted(rel) && self.filter.includes(path, rel)
            });
        }
        SearchComplete { limit_hit: state.limit_hit.load(Ordering::Relaxed) }
    }

    /// One parallel walk: searches the files `accept` lets through.
    fn walk(
        &self,
        builder: &WalkBuilder,
        state: &RunState,
        skip: &HashSet<PathBuf>,
        cancel: &AtomicBool,
        on_file: &(dyn Fn(FileMatches) + Sync),
        accept: &(dyn Fn(&Path, &Path) -> bool + Sync),
    ) {
        builder.build_parallel().run(|| {
            Box::new(|entry| {
                if cancel.load(Ordering::Relaxed) || state.limit_hit.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                let Ok(entry) = entry else { return WalkState::Continue };
                let path = entry.path();
                if !entry.file_type().is_some_and(|t| t.is_file()) || skip.contains(path) {
                    return WalkState::Continue;
                }
                if !path.strip_prefix(&self.root).is_ok_and(|rel| accept(path, rel)) {
                    return WalkState::Continue;
                }
                // One more than the remaining budget, to learn whether the
                // limit is exceeded.
                let cap = self.max_results.saturating_sub(state.total.load(Ordering::Relaxed)).saturating_add(1);
                let Some(mut matches) = self.search_file(path, cap) else { return WalkState::Continue };
                if matches.is_empty() {
                    return WalkState::Continue;
                }
                let before = state.total.fetch_add(matches.len(), Ordering::Relaxed);
                if before.saturating_add(matches.len()) > self.max_results {
                    state.limit_hit.store(true, Ordering::Relaxed);
                    matches.truncate(self.max_results.saturating_sub(before));
                }
                if !matches.is_empty()
                    && let Some(uri) = uri::path_to_uri(path)
                {
                    if let Some(reported) = &state.reported {
                        reported.lock().unwrap().insert(path.to_path_buf());
                    }
                    on_file(FileMatches { uri, path: path.to_string_lossy().into_owned(), matches });
                }
                if state.limit_hit.load(Ordering::Relaxed) {
                    WalkState::Quit
                } else {
                    WalkState::Continue
                }
            })
        });
    }

    /// Searches one file on disk. `None` for unreadable and binary files.
    fn search_file(&self, path: &Path, cap: usize) -> Option<Vec<TextMatch>> {
        let bytes = std::fs::read(path).ok()?;
        // ripgrep's default binary detection: a NUL byte means binary.
        if memchr::memchr(0, &bytes).is_some() {
            return None;
        }
        let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        let text = String::from_utf8_lossy(body);
        Some(self.search_text(&text, cap))
    }

    /// Finds up to `cap` matches in `text`.
    pub fn search_text(&self, text: &str, cap: usize) -> Vec<TextMatch> {
        let mut out = Vec::new();
        if cap == 0 {
            return out;
        }
        if self.multiline {
            self.search_multiline(text, cap, &mut out);
        } else {
            // Like ripgrep without --multiline: each line is searched on its
            // own, so a pattern can never match a line break.
            if !self.matcher.may_match_lines(text) {
                return out;
            }
            let mut spans = Vec::new();
            for (line_number, (start, end, _)) in lines(text).enumerate() {
                let line = &text[start..end];
                spans.clear();
                self.matcher.find_all(line, cap - out.len(), &mut spans);
                if spans.is_empty() {
                    continue;
                }
                let map = Utf16Map::new(line);
                for &(s, e) in &spans {
                    let (sc, ec) = (map.utf16(s), map.utf16(e));
                    out.push(TextMatch {
                        range: Range::new(Position::new(line_number as u32, sc), Position::new(line_number as u32, ec)),
                        preview: make_preview(line, &map, sc, ec, self.chars_per_line),
                        text: None,
                    });
                }
                if out.len() >= cap {
                    break;
                }
            }
        }
        out
    }

    /// Multi-line search runs over the whole text with every line break
    /// turned into `\n`, so `\n` in a pattern matches any line ending (as
    /// with VS Code's `\r?\n` rewrite, and like Monaco's multi-line find).
    fn search_multiline(&self, text: &str, cap: usize, out: &mut Vec<TextMatch>) {
        let normalized = normalize_line_breaks(text);
        let mut spans = Vec::new();
        self.matcher.find_all(&normalized.text, cap, &mut spans);
        if spans.is_empty() {
            return;
        }
        let index = LineIndex::new(text);
        let mut cached: Option<(usize, Utf16Map)> = None;
        for (s, e) in spans {
            let (s, e) = (normalized.original_offset(s), normalized.original_offset(e));
            let start_line = index.line_of(s);
            let end_line = index.line_of(e);
            let line = index.content(text, start_line);
            let line_start = index.starts[start_line];
            if cached.as_ref().is_none_or(|(l, _)| *l != start_line) {
                cached = Some((start_line, Utf16Map::new(line)));
            }
            let map = &cached.as_ref().unwrap().1;
            let sc = map.utf16(s - line_start);
            let end = if end_line == start_line {
                Position::new(start_line as u32, map.utf16(e - line_start))
            } else {
                let end_line_start = index.starts[end_line];
                Position::new(end_line as u32, Utf16Map::new(&text[end_line_start..e]).utf16(e - end_line_start))
            };
            let multi = end_line != start_line;
            let ec = if multi { map.utf16(line.len()) } else { end.character };
            out.push(TextMatch {
                range: Range::new(Position::new(start_line as u32, sc), end),
                preview: make_preview(line, map, sc, ec, self.chars_per_line),
                text: multi.then(|| truncate_at_char_boundary(&text[s..e], MAX_MATCH_TEXT).to_owned()),
            });
        }
    }
}

/// Builds the regex source the way VS Code's `getRgArgs` does. Returns the
/// source and whether the search is multi-line.
fn regex_source(query: &TextSearchQuery) -> (String, bool) {
    let multiline = (query.is_regex && is_multiline_regex_source(&query.pattern)) || query.pattern.contains('\n');
    let mut source = if query.is_regex { query.pattern.clone() } else { regex::escape(&query.pattern) };
    if query.is_word_match {
        // `strings.createRegExp(…, { wholeWord: true })`: add `\b` where the
        // pattern starts or ends with a word character.
        let is_word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if is_word(source.chars().next()) {
            source.insert_str(0, "\\b");
        }
        if is_word(source.chars().last()) {
            source.push_str("\\b");
        }
    }
    (source, multiline)
}

/// Monaco's `isMultilineRegexSource`: a line feed, or `\n`, `\r` or `\W`.
fn is_multiline_regex_source(source: &str) -> bool {
    let mut chars = source.chars();
    while let Some(c) = chars.next() {
        match c {
            '\n' => return true,
            '\\' => match chars.next() {
                Some('n' | 'r' | 'W') => return true,
                Some(_) => {}
                None => break,
            },
            _ => {}
        }
    }
    false
}

enum Matcher {
    /// `whole_text_check`: a match on any line implies a match in the whole
    /// text, so a file without a whole-text match can be skipped.
    Std { re: regex::Regex, whole_text_check: bool },
    /// For look-around and backreferences, like ripgrep's `--engine auto`
    /// falling back to PCRE2.
    Fancy(fancy_regex::Regex),
}

impl Matcher {
    fn compile(source: &str, case_sensitive: bool) -> Result<Self, SearchError> {
        // ripgrep always enables multi-line anchors: `^`/`$` match at line boundaries.
        match regex::RegexBuilder::new(source).case_insensitive(!case_sensitive).multi_line(true).build() {
            Ok(re) => {
                // Per-line and whole-text matching only differ at line edges:
                // `^`, `$` and word boundaries see a text edge in one and a
                // `\n` in the other, which they treat alike. Only text anchors
                // (`\A`, `\z`, or `^`/`$` with multi-line mode off) differ.
                let whole_text_check = regex_syntax::ParserBuilder::new()
                    .case_insensitive(!case_sensitive)
                    .multi_line(true)
                    .build()
                    .parse(source)
                    .is_ok_and(|hir| {
                        let looks = hir.properties().look_set();
                        !looks.contains(regex_syntax::hir::Look::Start) && !looks.contains(regex_syntax::hir::Look::End)
                    });
                Ok(Matcher::Std { re, whole_text_check })
            }
            Err(err) => fancy_regex::RegexBuilder::new(source)
                .case_insensitive(!case_sensitive)
                .multi_line(true)
                .build()
                .map(Matcher::Fancy)
                .map_err(|_| SearchError(regex_error_message(&err))),
        }
    }

    /// Whether `text` (line breaks normalized to `\n`) can contain a line
    /// with a match. Only a cheap pre-check; `true` when unsure.
    fn may_match_lines(&self, text: &str) -> bool {
        match self {
            Matcher::Std { re, whole_text_check: true } => re.is_match(&normalize_line_breaks(text).text),
            _ => true,
        }
    }

    /// Appends up to `cap` match spans (byte offsets) in `haystack`.
    fn find_all(&self, haystack: &str, cap: usize, out: &mut Vec<(usize, usize)>) {
        match self {
            Matcher::Std { re, .. } => out.extend(re.find_iter(haystack).take(cap).map(|m| (m.start(), m.end()))),
            Matcher::Fancy(re) => {
                // A match error (backtracking limit) ends the search of this text.
                out.extend(re.find_iter(haystack).map_while(Result::ok).take(cap).map(|m| (m.start(), m.end())))
            }
        }
    }
}

/// The relevant line of a `regex` error ("unclosed group" rather than the
/// multi-line rendering with a caret).
fn regex_error_message(err: &regex::Error) -> String {
    let text = err.to_string();
    text.lines()
        .find_map(|l| l.trim().strip_prefix("error: "))
        .map(|m| format!("Invalid regular expression: {m}"))
        .unwrap_or_else(|| text.lines().next().unwrap_or("invalid regular expression").to_owned())
}

// ---------------------------------------------------------------------------
// Include/exclude patterns (VS Code's QueryBuilder.parseSearchPaths)

#[derive(Debug, Default, PartialEq)]
struct PatternSet {
    /// Globs relative to the folder.
    globs: Vec<String>,
    /// A search path that names the folder itself (`./`).
    whole_folder: bool,
}

fn parse_patterns(input: &str, root: &Path) -> Result<PatternSet, SearchError> {
    let mut set = PatternSet::default();
    for segment in split_glob_aware(input, ',').iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let segment = untildify(segment);
        if is_search_path(&segment) {
            let (path_portion, glob_portion) = split_glob_from_path(&segment);
            let glob_portion = glob_portion.map(|g| normalize_glob_pattern(&g)).filter(|g| !g.is_empty());
            let base = search_path_pattern(&path_portion, root)?;
            let pattern = match (base, glob_portion) {
                (Some(base), Some(glob)) => Some(format!("{base}/{glob}")),
                (base, glob) => base.or(glob),
            };
            match pattern {
                None => set.whole_folder = true,
                Some(pattern) => {
                    let folder_glob = (!pattern.ends_with("**")).then(|| format!("{pattern}/**"));
                    set.globs.push(pattern);
                    set.globs.extend(folder_glob);
                }
            }
        } else {
            let trimmed = segment.trim_end_matches('/').trim_end_matches('\\');
            // ".js" means "*.js".
            let pattern = if trimmed.starts_with('.') { format!("*{trimmed}") } else { trimmed.to_owned() };
            set.globs.extend(expand_global_glob(&pattern));
        }
    }
    Ok(set)
}

/// The folder-relative pattern a search path (`./src`, `../x`, an absolute
/// path) stands for, `None` for the folder itself.
fn search_path_pattern(path_portion: &str, root: &Path) -> Result<Option<String>, SearchError> {
    let path = normalize_slashes(path_portion);
    let absolute = if Path::new(path_portion).is_absolute() {
        lexical_normalize(Path::new(path_portion))
    } else if path == ".." || path.starts_with("../") {
        lexical_normalize(&root.join(&path))
    } else {
        let cleaned = normalize_glob_pattern(&path);
        return Ok((!cleaned.is_empty() && cleaned != ".").then_some(cleaned));
    };
    let rel =
        absolute.strip_prefix(root).map_err(|_| SearchError(format!("{path_portion} is outside the open folder")))?;
    let rel = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
    Ok((!rel.is_empty()).then_some(rel))
}

fn is_search_path(segment: &str) -> bool {
    if Path::new(segment).is_absolute() {
        return true;
    }
    // ./ ../ .\ ..\ . ..
    let rest = segment.strip_prefix("..").or_else(|| segment.strip_prefix('.'));
    rest.is_some_and(|r| r.is_empty() || r.starts_with('/') || r.starts_with('\\'))
}

/// `~/x` → `$HOME/x`.
fn untildify(segment: &str) -> String {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    match (home, segment.strip_prefix('~')) {
        (Some(home), Some(rest)) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') => {
            format!("{}{rest}", home.to_string_lossy())
        }
        _ => segment.to_owned(),
    }
}

/// Splits `a/b/*.ts` into the path before the first glob character and the
/// glob after it.
fn split_glob_from_path(search_path: &str) -> (String, Option<String>) {
    if let Some(glob_idx) = search_path.find(['*', '{', '}', '(', ')', '[', ']', '?']) {
        let head = &search_path[..glob_idx];
        if let Some(slash) = head.rfind(['/', '\\', '|']) {
            let mut path_portion = search_path[..slash].to_owned();
            if !path_portion.contains(['/', '\\']) {
                path_portion.push('/');
            }
            return (path_portion, Some(search_path[slash + 1..].to_owned()));
        }
    }
    (search_path.to_owned(), None)
}

fn expand_global_glob(pattern: &str) -> [String; 2] {
    [format!("**/{pattern}/**"), format!("**/{pattern}")].map(|p| p.replace("**/**", "**"))
}

fn normalize_slashes(pattern: &str) -> String {
    pattern.replace('\\', "/")
}

/// Normalizes slashes, removes a leading `./` and trailing slashes.
fn normalize_glob_pattern(pattern: &str) -> String {
    let pattern = normalize_slashes(pattern);
    let pattern = pattern.strip_prefix("./").unwrap_or(&pattern);
    pattern.trim_end_matches('/').to_owned()
}

/// Resolves `.` and `..` without touching the file system.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `foo/*bar/x` → `foo`, `foo/*bar`, `foo/*bar/x`, so that ripgrep descends
/// into the folders leading to an anchored include.
fn spread_glob_components(glob: &str) -> Vec<String> {
    expand_braces(glob)
        .into_iter()
        .flat_map(|g| {
            let parts = split_glob_aware(&g, '/');
            (1..=parts.len()).map(move |n| parts[..n].join("/")).collect::<Vec<_>>()
        })
        .collect()
}

fn anchor_glob(glob: &str) -> String {
    if glob.starts_with("**") || glob.starts_with('/') {
        glob.to_owned()
    } else {
        format!("/{glob}")
    }
}

/// VS Code's `pushIncludeAndExcludeGlobs`, as ripgrep overrides (`-g`).
fn build_overrides(root: &Path, includes: &[String], excludes: &[String]) -> Result<Override, SearchError> {
    let mut builder = OverrideBuilder::new(root);
    builder.case_insensitive(GLOB_CASE_INSENSITIVE).map_err(glob_error)?;
    let mut add = |glob: &str| builder.add(glob).map(|_| ()).map_err(glob_error);

    let (double_star, others): (Vec<_>, Vec<_>) = includes.iter().partition(|g| g.starts_with("**"));
    if !others.is_empty() {
        add("!*")?;
        let mut seen = HashSet::new();
        for other in others.into_iter().filter(|o| seen.insert(o.as_str())) {
            for glob in spread_glob_components(other) {
                add(&anchor_glob(&glob))?;
            }
        }
    }
    for glob in double_star {
        add(glob)?;
    }
    for glob in excludes {
        add(&format!("!{}", anchor_glob(glob)))?;
    }
    builder.build().map_err(glob_error)
}

/// VS Code's `getExcludeExceptionGlobs`: ripgrep globs that walk only the
/// folders leading to the exceptions and the paths beneath them.
fn build_exception_overrides(root: &Path, exceptions: &[String]) -> Result<Override, SearchError> {
    let expanded: Vec<String> = exceptions.iter().flat_map(|e| expand_braces(e)).collect();
    let mut globs: Vec<String> = Vec::new();
    let mut push = |glob: String| {
        if !globs.contains(&glob) {
            globs.push(glob);
        }
    };
    // Unless an exception can match at any depth (which needs the whole
    // folder anyway), only walk the folders that lead to the exceptions.
    if !expanded.iter().any(|e| e.starts_with("**")) {
        push("!*".into());
        for exception in &expanded {
            let segments = split_glob_aware(exception, '/');
            for i in 1..segments.len() {
                // A trailing slash only matches folders.
                push(format!("{}/", anchor_glob(&segments[..i].join("/"))));
            }
        }
    }
    for exception in &expanded {
        push(anchor_glob(exception));
        push(anchor_glob(&format!("{exception}/**")));
    }
    // Later globs take precedence.
    for folder in glob::folders_skipped_by_exceptions(exceptions) {
        push(format!("!**/{folder}"));
    }
    let mut builder = OverrideBuilder::new(root);
    builder.case_insensitive(GLOB_CASE_INSENSITIVE).map_err(glob_error)?;
    for glob in &globs {
        builder.add(glob).map_err(glob_error)?;
    }
    builder.build().map_err(glob_error)
}

/// Counters shared by the threads of a search.
struct RunState {
    total: AtomicUsize,
    limit_hit: AtomicBool,
    /// Files reported so far, when a second walk must skip them.
    reported: Option<Mutex<HashSet<PathBuf>>>,
}

/// Include patterns and the exclude expression, with VS Code's glob
/// semantics, tested against a file's path relative to the folder.
struct ResultFilter {
    include: Option<GlobSet>,
    exclude: ParsedExclude,
}

impl ResultFilter {
    fn new(includes: &[String], exclude: ParsedExclude) -> Result<Self, SearchError> {
        let include = if includes.is_empty() {
            None
        } else {
            let mut builder = GlobSetBuilder::new();
            for pattern in includes {
                builder.add(
                    GlobBuilder::new(pattern)
                        .literal_separator(true)
                        .case_insensitive(GLOB_CASE_INSENSITIVE)
                        .build()
                        .map_err(|e| SearchError(format!("Invalid glob pattern: {e}")))?,
                );
            }
            Some(builder.build().map_err(|e| SearchError(format!("Invalid glob pattern: {e}")))?)
        };
        Ok(Self { include, exclude })
    }

    /// `path` is the absolute path (for sibling clauses), `rel` the same
    /// path relative to the folder.
    fn includes(&self, path: &Path, rel: &Path) -> bool {
        let rel = slashed(rel);
        let has_sibling = |name: &str| path.parent().is_some_and(|dir| dir.join(name).exists());
        !self.exclude.matches(&rel, &has_sibling) && self.include.as_ref().is_none_or(|inc| inc.is_match(&rel))
    }

    fn is_excepted(&self, rel: &Path) -> bool {
        self.exclude.is_excepted(&slashed(rel))
    }
}

fn slashed(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

fn glob_error(err: ignore::Error) -> SearchError {
    SearchError(format!("Invalid glob pattern: {err}"))
}

// ---------------------------------------------------------------------------
// Text helpers

/// Lines of `text` as (start, content end, next line start) byte offsets.
/// Like ripgrep, a final line break does not start another (empty) line.
fn lines(text: &str) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
    let bytes = text.as_bytes();
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos >= bytes.len() {
            return None;
        }
        let start = pos;
        match memchr::memchr2(b'\n', b'\r', &bytes[pos..]) {
            None => {
                pos = bytes.len();
                Some((start, bytes.len(), bytes.len()))
            }
            Some(i) => {
                let end = pos + i;
                let crlf = bytes[end] == b'\r' && bytes.get(end + 1) == Some(&b'\n');
                pos = end + if crlf { 2 } else { 1 };
                Some((start, end, pos))
            }
        }
    })
}

/// Start offsets of all lines (`\n`, `\r\n`, `\r`), for multi-line matches.
struct LineIndex {
    starts: Vec<usize>,
    ends: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let (mut starts, mut ends) = (Vec::new(), Vec::new());
        for (start, end, _) in lines(text) {
            starts.push(start);
            ends.push(end);
        }
        // An empty last line after a final line break, where a match can end.
        if starts.is_empty() || text.ends_with(['\n', '\r']) {
            starts.push(text.len());
            ends.push(text.len());
        }
        Self { starts, ends }
    }

    fn line_of(&self, offset: usize) -> usize {
        self.starts.partition_point(|&s| s <= offset).saturating_sub(1)
    }

    fn content<'a>(&self, text: &'a str, line: usize) -> &'a str {
        &text[self.starts[line]..self.ends[line]]
    }
}

/// Text with every line break turned into `\n`, and the positions of the
/// `\r`s removed from `\r\n` pairs.
struct Normalized<'a> {
    text: Cow<'a, str>,
    /// Offsets in the normalized text of the `\n`s that were `\r\n`.
    collapsed: Vec<usize>,
}

impl Normalized<'_> {
    fn original_offset(&self, offset: usize) -> usize {
        offset + self.collapsed.partition_point(|&p| p < offset)
    }
}

fn normalize_line_breaks(text: &str) -> Normalized<'_> {
    if !text.contains('\r') {
        return Normalized { text: Cow::Borrowed(text), collapsed: Vec::new() };
    }
    let mut out = String::with_capacity(text.len());
    let mut collapsed = Vec::new();
    for (start, end, next) in lines(text) {
        out.push_str(&text[start..end]);
        if next > end {
            if next - end == 2 {
                collapsed.push(out.len());
            }
            out.push('\n');
        }
    }
    Normalized { text: Cow::Owned(out), collapsed }
}

/// Byte offset → UTF-16 offset within one line.
struct Utf16Map {
    /// `(byte, utf16)` at every char boundary, or `None` for ASCII lines.
    boundaries: Option<Vec<(usize, u32)>>,
}

impl Utf16Map {
    fn new(line: &str) -> Self {
        if line.is_ascii() {
            return Self { boundaries: None };
        }
        let mut boundaries = Vec::with_capacity(line.len() + 1);
        let mut units = 0u32;
        for (i, c) in line.char_indices() {
            boundaries.push((i, units));
            units += c.len_utf16() as u32;
        }
        boundaries.push((line.len(), units));
        Self { boundaries: Some(boundaries) }
    }

    fn utf16(&self, byte: usize) -> u32 {
        match &self.boundaries {
            None => byte as u32,
            Some(b) => b[b.partition_point(|&(i, _)| i < byte).min(b.len() - 1)].1,
        }
    }

    /// The char boundary at or before `units`.
    fn byte_floor(&self, units: u32) -> usize {
        match &self.boundaries {
            None => units as usize,
            Some(b) => b[b.partition_point(|&(_, u)| u <= units).saturating_sub(1)].0,
        }
    }

    /// The char boundary at or after `units`.
    fn byte_ceil(&self, units: u32) -> usize {
        match &self.boundaries {
            None => units as usize,
            Some(b) => b[b.partition_point(|&(_, u)| u < units).min(b.len() - 1)].0,
        }
    }
}

/// VS Code's one-line preview (`TextSearchMatch`): up to a fifth of
/// `chars_per_line` before the match, `chars_per_line` from its start.
fn make_preview(line: &str, map: &Utf16Map, start: u32, end: u32, chars_per_line: u32) -> Preview {
    let len = map.utf16(line.len());
    let from = map.byte_floor(start.saturating_sub(chars_per_line / 5));
    let to = map.byte_ceil(start.saturating_add(chars_per_line).min(len)).max(from);
    let offset = map.utf16(from);
    let text = &line[from..to];
    let text_len = map.utf16(to) - offset;
    Preview { text: text.to_owned(), start: start - offset, end: end.saturating_sub(offset).min(text_len) }
}

fn truncate_at_char_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `relative path → [(line, start column, end column)]`, sorted.
    type Results = Vec<(String, Vec<(u32, u32, u32)>)>;

    fn defaults() -> Settings {
        Settings::default()
    }

    fn query(pattern: &str) -> TextSearchQuery {
        TextSearchQuery::new(pattern)
    }

    fn folder(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = uri::canonicalize(dir.path()).unwrap();
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        (dir, root)
    }

    /// Runs a search and returns `relative path → [(line, start col, end col)]`, sorted.
    fn run(root: &Path, q: &TextSearchQuery) -> Results {
        run_full(root, q, &HashSet::new()).0
    }

    fn run_full(root: &Path, q: &TextSearchQuery, skip: &HashSet<PathBuf>) -> (Results, SearchComplete) {
        run_with(root, q, skip, &defaults())
    }

    fn run_with(
        root: &Path,
        q: &TextSearchQuery,
        skip: &HashSet<PathBuf>,
        excludes: &Settings,
    ) -> (Results, SearchComplete) {
        let search = TextSearch::new(root, q, excludes).unwrap();
        let found = Mutex::new(Vec::new());
        let complete = search.run(skip, &AtomicBool::new(false), |f| found.lock().unwrap().push(f));
        let mut found: Vec<_> = found
            .into_inner()
            .unwrap()
            .into_iter()
            .map(|f| {
                let rel = Path::new(&f.path).strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                let ranges =
                    f.matches.iter().map(|m| (m.range.start.line, m.range.start.character, m.range.end.character));
                (rel, ranges.collect())
            })
            .collect();
        found.sort();
        (found, complete)
    }

    fn files(results: &Results) -> Vec<&str> {
        results.iter().map(|(f, _)| f.as_str()).collect()
    }

    fn matches_in(text: &str, q: &TextSearchQuery) -> Vec<TextMatch> {
        TextSearch::new(Path::new("/"), q, &defaults()).unwrap().search_text(text, usize::MAX)
    }

    #[test]
    fn literal_case_and_word_options() {
        let text = "Foo foo food\n";
        let spans =
            |q: &TextSearchQuery| -> Vec<u32> { matches_in(text, q).iter().map(|m| m.range.start.character).collect() };
        assert_eq!(spans(&query("foo")), [0, 4, 8]);
        assert_eq!(spans(&TextSearchQuery { is_case_sensitive: true, ..query("foo") }), [4, 8]);
        assert_eq!(spans(&TextSearchQuery { is_word_match: true, ..query("foo") }), [0, 4]);
        // Regex characters are literal unless regex mode is on.
        assert!(matches_in("a.c abc", &query("a.c")).len() == 1);
        assert_eq!(matches_in("a.c abc", &TextSearchQuery { is_regex: true, ..query("a.c") }).len(), 2);
        // Whole word next to punctuation: no \b is added after a non-word char.
        assert_eq!(matches_in("x-> y->", &TextSearchQuery { is_word_match: true, ..query("->") }).len(), 2);
    }

    #[test]
    fn columns_are_utf16_and_lines_follow_all_line_breaks() {
        let text = "😀 needle\r\nx\rneedle é needle\n";
        let m = matches_in(text, &query("needle"));
        let ranges: Vec<_> =
            m.iter().map(|m| (m.range.start.line, m.range.start.character, m.range.end.character)).collect();
        assert_eq!(ranges, [(0, 3, 9), (2, 0, 6), (2, 9, 15)]);
        assert_eq!(m[0].preview, Preview { text: "😀 needle".into(), start: 3, end: 9 });
    }

    #[test]
    fn single_line_patterns_never_cross_lines() {
        let q = TextSearchQuery { is_regex: true, ..query("a\\s+b") };
        assert!(matches_in("a\nb", &q).is_empty());
        assert_eq!(matches_in("a  b", &q).len(), 1);
        // `$` is the end of each line, also before \r\n.
        let q = TextSearchQuery { is_regex: true, ..query("x$") };
        assert_eq!(matches_in("ax\r\nbx\r\n", &q).len(), 2);
    }

    #[test]
    fn multiline_matches_any_line_ending() {
        let q = TextSearchQuery { is_regex: true, ..query("b\\nc") };
        for text in ["ab\nc", "ab\r\nc", "ab\rc"] {
            let m = matches_in(text, &q);
            assert_eq!(m.len(), 1, "{text:?}");
            assert_eq!(m[0].range, Range::new(Position::new(0, 1), Position::new(1, 1)));
            assert_eq!(m[0].preview, Preview { text: "ab".into(), start: 1, end: 2 });
        }
        assert_eq!(matches_in("ab\r\nc", &q)[0].text.as_deref(), Some("b\r\nc"));
        // A literal pattern containing a newline is multi-line too.
        let m = matches_in("x\r\ny\r\nz", &query("y\nz"));
        assert_eq!(m[0].range, Range::new(Position::new(1, 0), Position::new(2, 1)));
        // Matches after collapsed \r\n pairs land on the right lines.
        let m = matches_in("a\r\nb\r\ncc\ndd", &TextSearchQuery { is_regex: true, ..query("c\\nd") });
        assert_eq!(m[0].range, Range::new(Position::new(2, 1), Position::new(3, 1)));
    }

    #[test]
    fn lookaround_and_backreferences_fall_back_to_fancy_regex() {
        let q = TextSearchQuery { is_regex: true, ..query("foo(?=bar)") };
        assert_eq!(matches_in("foobar foobaz", &q).len(), 1);
        let q = TextSearchQuery { is_regex: true, ..query("(\\w)\\1") };
        assert_eq!(matches_in("abba", &q).len(), 1);
        let err = TextSearch::new(Path::new("/"), &TextSearchQuery { is_regex: true, ..query("(") }, &defaults())
            .err()
            .unwrap();
        assert!(err.0.contains("unclosed group"), "{err}");
    }

    #[test]
    fn previews_are_trimmed_around_the_match() {
        let line = format!("{}needle{}", "a".repeat(500), "b".repeat(2000));
        let m = &matches_in(&line, &query("needle"))[0];
        assert_eq!(m.preview.start, 200);
        assert_eq!(m.preview.end, 206);
        assert_eq!(m.preview.text.len(), 1200);
        assert_eq!(&m.preview.text[200..206], "needle");
    }

    #[test]
    fn respects_ignore_files_hidden_files_and_default_excludes() {
        let (_dir, root) = folder(&[
            ("a.txt", "hit"),
            (".hidden/b.txt", "hit"),
            ("ignored.txt", "hit"),
            ("node_modules/m.txt", "hit"),
            (".git/config", "hit"),
            (".gitignore", "ignored.txt\n"),
            ("bin.dat", "hit\0"),
        ]);
        assert_eq!(files(&run(&root, &query("hit"))), [".hidden/b.txt", "a.txt"]);
        let all = TextSearchQuery { use_exclude_settings_and_ignore_files: false, ..query("hit") };
        assert_eq!(
            files(&run(&root, &all)),
            [".git/config", ".hidden/b.txt", "a.txt", "ignored.txt", "node_modules/m.txt"]
        );
    }

    #[test]
    fn include_and_exclude_patterns() {
        let (_dir, root) = folder(&[
            ("src/a.rs", "hit"),
            ("src/b.ts", "hit"),
            ("src/sub/c.rs", "hit"),
            ("test/src/d.rs", "hit"),
            ("e.rs", "hit"),
        ]);
        let with = |includes: &str, excludes: &str| {
            let q = TextSearchQuery { includes: includes.into(), excludes: excludes.into(), ..query("hit") };
            files(&run(&root, &q)).into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        assert_eq!(with("*.rs", ""), ["e.rs", "src/a.rs", "src/sub/c.rs", "test/src/d.rs"]);
        assert_eq!(with(".ts", ""), ["src/b.ts"]);
        // A bare name matches folders at any depth.
        assert_eq!(with("src", ""), ["src/a.rs", "src/b.ts", "src/sub/c.rs", "test/src/d.rs"]);
        // ./ anchors at the folder.
        assert_eq!(with("./src", ""), ["src/a.rs", "src/b.ts", "src/sub/c.rs"]);
        assert_eq!(with("./src/*.rs", ""), ["src/a.rs"]);
        assert_eq!(with("./src/**/*.rs", ""), ["src/a.rs", "src/sub/c.rs"]);
        assert_eq!(with("src/{a,b}.*", ""), ["src/a.rs", "src/b.ts"]);
        assert_eq!(with("e.rs, ./src/sub", ""), ["e.rs", "src/sub/c.rs"]);
        assert_eq!(with("", "sub, *.ts"), ["e.rs", "src/a.rs", "test/src/d.rs"]);
        assert_eq!(with("./src", "./src/sub"), ["src/a.rs", "src/b.ts"]);
        assert_eq!(with(&root.join("src/sub").to_string_lossy(), ""), ["src/sub/c.rs"]);
        assert!(with("", "./").is_empty());
        assert_eq!(with("./", "").len(), 5);
        let outside = TextSearchQuery { includes: "../elsewhere".into(), ..query("hit") };
        assert!(TextSearch::new(&root, &outside, &defaults()).is_err());
    }

    #[test]
    fn scope_check_for_open_documents_ignores_gitignore_but_not_globs() {
        let (_dir, root) = folder(&[(".gitignore", "gen/\n")]);
        let q = TextSearchQuery { excludes: "*.lock".into(), ..query("x") };
        let search = TextSearch::new(&root, &q, &defaults()).unwrap();
        assert!(search.in_scope(&root.join("gen/a.rs")));
        assert!(!search.in_scope(&root.join("Cargo.lock")));
        assert!(!search.in_scope(&root.join("node_modules/x/a.js")));
        assert!(!search.in_scope(Path::new("/elsewhere/a.rs")));
        let q = TextSearchQuery { includes: "./src".into(), ..query("x") };
        let search = TextSearch::new(&root, &q, &defaults()).unwrap();
        assert!(search.in_scope(&root.join("src/deep/a.rs")));
        assert!(!search.in_scope(&root.join("other/a.rs")));
    }

    #[test]
    fn limit_skip_and_bom() {
        let (_dir, root) = folder(&[("a.txt", "x x x"), ("b.txt", "x x x"), ("c.txt", "\u{feff}x")]);
        let q = TextSearchQuery { max_results: Some(4), ..query("x") };
        let (results, complete) = run_full(&root, &q, &HashSet::new());
        assert!(complete.limit_hit);
        assert_eq!(results.iter().map(|(_, m)| m.len()).sum::<usize>(), 4);

        let (results, complete) = run_full(&root, &query("x"), &HashSet::from([root.join("a.txt")]));
        assert!(!complete.limit_hit);
        assert_eq!(files(&results), ["b.txt", "c.txt"]);
        // The BOM is not part of the text: the match is at column 0.
        assert_eq!(results[1].1, [(0, 0, 1)]);

        let exact = TextSearchQuery { max_results: Some(7), ..query("x") };
        assert!(!run_full(&root, &exact, &HashSet::new()).1.limit_hit);
    }

    #[test]
    fn glob_helpers_match_vs_code() {
        assert_eq!(split_glob_aware("a,{b,c},[d,e]", ','), ["a", "{b,c}", "[d,e]"]);
        assert_eq!(expand_braces("a/{b,c{d,e}}/f"), ["a/b/f", "a/cd/f", "a/ce/f"]);
        assert_eq!(spread_glob_components("foo/*bar/x"), ["foo", "foo/*bar", "foo/*bar/x"]);
        assert_eq!(split_glob_from_path("./src/*.ts"), ("./src".into(), Some("*.ts".into())));
        assert_eq!(split_glob_from_path("./*.ts"), ("./".into(), Some("*.ts".into())));
        assert_eq!(expand_global_glob("**/x"), ["**/x/**", "**/x"]);
        assert!(is_search_path("./a") && is_search_path("..") && is_search_path(".") && !is_search_path(".js"));
        assert!(
            is_multiline_regex_source("a\\nb")
                && is_multiline_regex_source("\\W")
                && !is_multiline_regex_source("\\\\n")
        );
    }

    fn settings(json: &str) -> Settings {
        Settings::parse(json).unwrap()
    }

    fn found_with(root: &Path, q: &TextSearchQuery, excludes: &Settings) -> Vec<String> {
        files(&run_with(root, q, &HashSet::new(), excludes).0).into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn exclude_exceptions_reinclude_excluded_and_ignored_paths() {
        let (_dir, root) = folder(&[
            ("node_modules/keep/a.txt", "hit"),
            ("node_modules/keep/sub/b.txt", "hit"),
            ("node_modules/other/c.txt", "hit"),
            ("src/d.txt", "hit"),
            ("gen/out.txt", "hit"),
            ("gen/skip.txt", "hit"),
            (".gitignore", "gen/\n"),
        ]);
        let excludes =
            settings(r#"{ "search.exclude": { "!node_modules/keep": true, "!gen/out.txt": true, "!src": true } }"#);
        assert_eq!(
            found_with(&root, &query("hit"), &excludes),
            ["gen/out.txt", "node_modules/keep/a.txt", "node_modules/keep/sub/b.txt", "src/d.txt"]
        );
        // Include patterns still apply to what exceptions re-include.
        let q = TextSearchQuery { includes: "./node_modules/keep/sub".into(), ..query("hit") };
        assert_eq!(found_with(&root, &q, &excludes), ["node_modules/keep/sub/b.txt"]);
        // Exceptions are settings: turning exclude settings off drops them along with the excludes.
        let off = TextSearchQuery { use_exclude_settings_and_ignore_files: false, ..query("hit") };
        assert_eq!(found_with(&root, &off, &excludes).len(), 6);
        // Typed excludes are overridden too, like in VS Code.
        let typed = TextSearchQuery { excludes: "node_modules".into(), ..query("hit") };
        assert!(found_with(&root, &typed, &excludes).contains(&"node_modules/keep/a.txt".to_owned()));
    }

    #[test]
    fn exceptions_at_any_depth_skip_version_control_folders() {
        let (_dir, root) = folder(&[("a/keep.log", "hit"), ("a/other.log", "hit"), (".git/keep.log", "hit")]);
        let excludes = settings(r#"{ "files.exclude": { "**/*.log": true, "!**/keep.log": true } }"#);
        assert_eq!(found_with(&root, &query("hit"), &excludes), ["a/keep.log"]);
        let named = settings(r#"{ "files.exclude": { "!.git/keep.log": true } }"#);
        assert_eq!(found_with(&root, &query("hit"), &named), [".git/keep.log", "a/keep.log", "a/other.log"]);
    }

    #[test]
    fn typed_exclamation_marks_are_literal() {
        let (_dir, root) = folder(&[("!odd/a.txt", "hit"), ("odd/b.txt", "hit")]);
        for excludes in ["./!odd", "!odd"] {
            let q = TextSearchQuery { excludes: excludes.into(), ..query("hit") };
            assert_eq!(found_with(&root, &q, &defaults()), ["odd/b.txt"], "{excludes}");
        }
    }

    #[test]
    fn sibling_clauses_hide_generated_files() {
        let (_dir, root) = folder(&[("a.ts", "hit"), ("a.js", "hit"), ("b.js", "hit")]);
        let excludes = settings(r#"{ "files.exclude": { "**/*.js": { "when": "$(basename).ts" } } }"#);
        assert_eq!(found_with(&root, &query("hit"), &excludes), ["a.ts", "b.js"]);
    }

    #[test]
    fn open_documents_reincluded_by_exceptions_are_in_scope() {
        let (_dir, root) = folder(&[]);
        let excludes = settings(r#"{ "search.exclude": { "!node_modules/keep": true } }"#);
        let search = TextSearch::new(&root, &query("x"), &excludes).unwrap();
        assert!(search.in_scope(&root.join("node_modules/keep/a.js")));
        assert!(!search.in_scope(&root.join("node_modules/other/a.js")));
        let q = TextSearchQuery { includes: "*.ts".into(), ..query("x") };
        assert!(!TextSearch::new(&root, &q, &excludes).unwrap().in_scope(&root.join("node_modules/keep/a.js")));
    }

    #[test]
    fn ignore_file_symlink_and_limit_settings() {
        let dir = tempfile::tempdir().unwrap();
        let outer = uri::canonicalize(dir.path()).unwrap();
        std::fs::write(outer.join(".gitignore"), "parent-ignored.txt\n").unwrap();
        let root = outer.join("project");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        for file in ["ignored.txt", "parent-ignored.txt", "a.txt", "real/b.txt"] {
            std::fs::write(root.join(file), "hit hit").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let q = query("hit");
        let all = found_with(&root, &q, &defaults());
        assert!(!all.contains(&"ignored.txt".to_owned()) && all.contains(&"parent-ignored.txt".to_owned()));
        #[cfg(unix)]
        assert!(all.contains(&"link/b.txt".to_owned()));

        let no_ignore = settings(r#"{ "search.useIgnoreFiles": false, "search.useParentIgnoreFiles": true }"#);
        let found = found_with(&root, &q, &no_ignore);
        assert!(found.contains(&"ignored.txt".to_owned()) && found.contains(&"parent-ignored.txt".to_owned()));
        let parent = settings(r#"{ "search.useParentIgnoreFiles": true }"#);
        assert!(!found_with(&root, &q, &parent).contains(&"parent-ignored.txt".to_owned()));
        #[cfg(unix)]
        assert!(!found_with(&root, &q, &settings(r#"{ "search.followSymlinks": false }"#))
            .contains(&"link/b.txt".to_owned()));

        let count = |s: &Settings| {
            let (results, complete) = run_with(&root, &q, &HashSet::new(), s);
            (results.iter().map(|(_, m)| m.len()).sum::<usize>(), complete.limit_hit)
        };
        assert_eq!(count(&settings(r#"{ "search.maxResults": 3 }"#)), (3, true));
        let total = count(&defaults()).0;
        assert_eq!(count(&settings(r#"{ "search.maxResults": null }"#)), (total, false));
    }
}
