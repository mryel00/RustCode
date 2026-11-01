//! Settings, read from VS Code's own settings files so projects and habits
//! set up for VS Code carry over: defaults, then VS Code's user settings,
//! then the folder's `.vscode/settings.json`, each laid over the one before.
//! Like VS Code, object settings (`files.exclude`, …) are merged key by key,
//! so `"**/.git": false` shows `.git` again; other settings are replaced.
//!
//! Only the settings RustCode acts on are read; see [`Settings`].

use crate::{
    fs::FILES_EXCLUDE,
    glob::{self, ExcludeExpression, ExcludeValue},
    search::{DEFAULT_MAX_RESULTS, SEARCH_EXCLUDE},
    watcher::DEFAULT_EXCLUDES,
};
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The settings file inside the folder.
pub fn workspace_settings_path(root: &Path) -> PathBuf {
    root.join(".vscode").join("settings.json")
}

/// VS Code's user settings file: the first existing one of VS Code, VS Code
/// Insiders, VSCodium and Code - OSS, or VS Code's path when none exists.
pub fn vscode_user_settings_path() -> Option<PathBuf> {
    let env = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let config_dir = if cfg!(windows) {
        env("APPDATA")?
    } else if cfg!(target_os = "macos") {
        env("HOME")?.join("Library/Application Support")
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))?
    };
    Some(user_settings_in(&config_dir, |p| p.is_file()))
}

/// Product folders, in order of preference.
const VSCODE_PRODUCTS: &[&str] = &["Code", "Code - Insiders", "VSCodium", "Code - OSS"];

fn user_settings_in(config_dir: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let path = |product: &str| config_dir.join(product).join("User").join("settings.json");
    VSCODE_PRODUCTS.iter().map(|p| path(p)).find(|p| exists(p)).unwrap_or_else(|| path(VSCODE_PRODUCTS[0]))
}

/// `editor.find.seedSearchStringFromSelection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SeedSearchString {
    Never,
    Always,
    Selection,
}

/// The `search.*` settings RustCode uses, with VS Code's defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSettings {
    pub use_ignore_files: bool,
    /// Only with `use_ignore_files`.
    pub use_global_ignore_files: bool,
    /// Only with `use_ignore_files`.
    pub use_parent_ignore_files: bool,
    pub follow_symlinks: bool,
    pub smart_case: bool,
    /// `None`: no limit (`null` in the settings).
    pub max_results: Option<usize>,
    pub search_on_type: bool,
    /// Milliseconds.
    pub search_on_type_debounce_period: u64,
    pub seed_with_nearest_word: bool,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            use_ignore_files: true,
            use_global_ignore_files: false,
            use_parent_ignore_files: false,
            follow_symlinks: true,
            smart_case: false,
            max_results: Some(DEFAULT_MAX_RESULTS),
            search_on_type: true,
            search_on_type_debounce_period: 300,
            seed_with_nearest_word: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub files_exclude: ExcludeExpression,
    pub search_exclude: ExcludeExpression,
    pub watcher_exclude: ExcludeExpression,
    pub search: SearchSettings,
    pub seed_search_string_from_selection: SeedSearchString,
    /// Problems with the settings files: unreadable or invalid JSON (that
    /// file is ignored), values of the wrong type, patterns that are not
    /// valid globs (each left out).
    pub warnings: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            files_exclude: ExcludeExpression::enabled(FILES_EXCLUDE.iter().map(|name| format!("**/{name}"))),
            search_exclude: ExcludeExpression::enabled(SEARCH_EXCLUDE.iter().copied()),
            watcher_exclude: ExcludeExpression::enabled(DEFAULT_EXCLUDES.iter().copied()),
            search: SearchSettings::default(),
            seed_search_string_from_selection: SeedSearchString::Always,
            warnings: Vec::new(),
        }
    }
}

/// The settings the UI applies itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSettings {
    pub smart_case: bool,
    pub max_results: Option<usize>,
    pub search_on_type: bool,
    pub search_on_type_debounce_period: u64,
    pub seed_with_nearest_word: bool,
    pub seed_search_string_from_selection: SeedSearchString,
}

impl Settings {
    /// Defaults, then the user settings file (if given), then the folder's
    /// settings file. Missing files are skipped.
    pub fn load(user_settings: Option<&Path>, root: &Path) -> Self {
        let mut settings = Self::default();
        if let Some(user) = user_settings {
            settings.apply_file(user);
        }
        settings.apply_file(&workspace_settings_path(root));
        settings
    }

    /// Defaults with one settings file's text laid over them.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut settings = Self::default();
        settings.apply(text)?;
        Ok(settings)
    }

    fn apply_file(&mut self, path: &Path) {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let before = self.warnings.len();
                if let Err(e) = self.apply(&text) {
                    self.warnings.push(format!("{}: {e}. Its settings are ignored.", path.display()));
                }
                for warning in &mut self.warnings[before..] {
                    *warning = format!("{}: {warning}", path.display());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => self.warnings.push(format!("Cannot read {}: {e}", path.display())),
        }
    }

    /// Lays one settings file (JSON with comments and trailing commas) over
    /// these settings. Invalid JSON changes nothing.
    pub fn apply(&mut self, text: &str) -> Result<(), String> {
        let value: Value = serde_json::from_str(&strip_jsonc(text)).map_err(|e| e.to_string())?;
        let Value::Object(root) = value else { return Err("settings must be a JSON object".into()) };
        let mut layer = Layer { root: &root, warnings: Vec::new() };

        layer.excludes("files.exclude", &mut self.files_exclude);
        layer.excludes("search.exclude", &mut self.search_exclude);
        layer.excludes("files.watcherExclude", &mut self.watcher_exclude);
        let search = &mut self.search;
        layer.bool("search.useIgnoreFiles", &mut search.use_ignore_files);
        layer.bool("search.useGlobalIgnoreFiles", &mut search.use_global_ignore_files);
        layer.bool("search.useParentIgnoreFiles", &mut search.use_parent_ignore_files);
        layer.bool("search.followSymlinks", &mut search.follow_symlinks);
        layer.bool("search.smartCase", &mut search.smart_case);
        layer.bool("search.searchOnType", &mut search.search_on_type);
        layer.bool("search.seedWithNearestWord", &mut search.seed_with_nearest_word);
        match layer.get("search.maxResults") {
            None => {}
            Some(Value::Null) => search.max_results = None,
            Some(v) => match v.as_f64().filter(|n| *n >= 0.0) {
                Some(n) => search.max_results = Some(n as usize),
                None => layer.wrong_type("search.maxResults", "a number or null"),
            },
        }
        match layer.get("search.searchOnTypeDebouncePeriod") {
            None => {}
            Some(v) => match v.as_f64().filter(|n| *n >= 0.0) {
                Some(n) => search.search_on_type_debounce_period = n as u64,
                None => layer.wrong_type("search.searchOnTypeDebouncePeriod", "a number"),
            },
        }
        match layer.get("editor.find.seedSearchStringFromSelection") {
            None => {}
            // Older VS Code versions used booleans.
            Some(Value::Bool(true)) => self.seed_search_string_from_selection = SeedSearchString::Always,
            Some(Value::Bool(false)) => self.seed_search_string_from_selection = SeedSearchString::Never,
            Some(Value::String(s)) if s == "always" => {
                self.seed_search_string_from_selection = SeedSearchString::Always
            }
            Some(Value::String(s)) if s == "never" => self.seed_search_string_from_selection = SeedSearchString::Never,
            Some(Value::String(s)) if s == "selection" => {
                self.seed_search_string_from_selection = SeedSearchString::Selection
            }
            Some(_) => {
                layer.wrong_type("editor.find.seedSearchStringFromSelection", "\"never\", \"always\" or \"selection\"")
            }
        }
        self.warnings.append(&mut layer.warnings);
        Ok(())
    }

    /// The excludes of a text search: `files.exclude` with `search.exclude`
    /// laid over it (VS Code's `getExcludes`).
    pub fn search_excludes(&self) -> ExcludeExpression {
        self.files_exclude.merged(&self.search_exclude)
    }

    pub fn ui(&self) -> UiSettings {
        UiSettings {
            smart_case: self.search.smart_case,
            max_results: self.search.max_results,
            search_on_type: self.search.search_on_type,
            search_on_type_debounce_period: self.search.search_on_type_debounce_period,
            seed_with_nearest_word: self.search.seed_with_nearest_word,
            seed_search_string_from_selection: self.seed_search_string_from_selection,
        }
    }
}

/// One settings file being applied.
struct Layer<'a> {
    root: &'a Map<String, Value>,
    warnings: Vec<String>,
}

impl<'a> Layer<'a> {
    fn get(&self, key: &str) -> Option<&'a Value> {
        lookup(self.root, key)
    }

    fn wrong_type(&mut self, key: &str, expected: &str) {
        self.warnings.push(format!("{key} must be {expected}; the value is ignored"));
    }

    fn bool(&mut self, key: &str, target: &mut bool) {
        match self.get(key) {
            None => {}
            Some(Value::Bool(b)) => *target = *b,
            Some(_) => self.wrong_type(key, "true or false"),
        }
    }

    fn excludes(&mut self, key: &str, target: &mut ExcludeExpression) {
        match self.get(key) {
            None => {}
            Some(Value::Object(map)) => {
                for (pattern, value) in map {
                    // A broken pattern must not break the explorer or search.
                    let glob = if glob::is_exception(pattern) { &pattern.trim_start()[1..] } else { pattern.as_str() };
                    if let Err(e) = glob::compile(glob.trim()) {
                        self.warnings.push(format!("{key}: ignoring {e}"));
                        continue;
                    }
                    target.set(pattern.clone(), ExcludeValue::from_json(value));
                }
            }
            Some(_) => self.wrong_type(key, "an object"),
        }
    }
}

/// A setting as a dotted key (`"editor.find.x"`) or nested at any dot
/// (`"editor": { "find": { "x": … } }`, `"editor.find": { "x": … }`).
fn lookup<'a>(root: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    if let Some(value) = root.get(key) {
        return Some(value);
    }
    key.match_indices('.').find_map(|(i, _)| match root.get(&key[..i]) {
        Some(Value::Object(map)) => lookup(map, &key[i + 1..]),
        _ => None,
    })
}

/// Turns JSON with comments and trailing commas into plain JSON.
fn strip_jsonc(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // Pass 1: drop comments, keeping strings intact.
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push(c);
                while let Some(c) = chars.next() {
                    out.push(c);
                    match c {
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                out.push(escaped);
                            }
                        }
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for c in chars.by_ref() {
                    if previous == '*' && c == '/' {
                        break;
                    }
                    previous = c;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    // Pass 2: drop commas that are followed only by whitespace and `}` or `]`.
    let bytes = out.as_bytes();
    let mut result = String::with_capacity(out.len());
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in out.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
        } else if c == ',' {
            let next = bytes[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
            if matches!(next, Some(b'}' | b']')) {
                continue;
            }
        }
        result.push(c);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonc_and_merges_excludes_with_defaults() {
        let settings = Settings::parse(
            r#"{
                // line comment with "quotes"
                "files.exclude": { "**/.git": false, "**/*.tmp": true, /* block */ },
                "search": { "exclude": { "!node_modules/keep": true, "**/gen": { "when": "$(basename).src" }, }, },
                "url": "http://example.com/*not a comment*/",
            }"#,
        )
        .unwrap();
        assert_eq!(
            settings.files_exclude.plain_patterns(),
            ["**/.svn", "**/.hg", "**/.jj", "**/.DS_Store", "**/Thumbs.db", "**/*.tmp"]
        );
        let search = settings.search_excludes();
        assert_eq!(search.exceptions(), ["node_modules/keep"]);
        assert!(search.plain_patterns().contains(&"**/node_modules".to_owned()));
        assert!(search.entries().contains(&("**/gen".into(), ExcludeValue::When("$(basename).src".into()))));
        assert!(settings.warnings.is_empty());
    }

    #[test]
    fn reads_search_watcher_and_seed_settings() {
        let settings = Settings::parse(
            r#"{
                "search.useIgnoreFiles": false, "search.useGlobalIgnoreFiles": true,
                "search": { "useParentIgnoreFiles": true, "followSymlinks": false },
                "search.smartCase": true, "search.maxResults": null, "search.searchOnType": false,
                "search.searchOnTypeDebouncePeriod": 50, "search.seedWithNearestWord": true,
                "editor.find": { "seedSearchStringFromSelection": "selection" },
                "files.watcherExclude": { "**/target/**": true, ".git/objects/**": false }
            }"#,
        )
        .unwrap();
        assert_eq!(
            settings.search,
            SearchSettings {
                use_ignore_files: false,
                use_global_ignore_files: true,
                use_parent_ignore_files: true,
                follow_symlinks: false,
                smart_case: true,
                max_results: None,
                search_on_type: false,
                search_on_type_debounce_period: 50,
                seed_with_nearest_word: true,
            }
        );
        assert_eq!(settings.seed_search_string_from_selection, SeedSearchString::Selection);
        let watcher = settings.watcher_exclude.plain_patterns();
        assert!(watcher.contains(&"**/target/**".to_owned()) && !watcher.contains(&".git/objects/**".to_owned()));
        assert_eq!(
            Settings::parse(r#"{ "editor.find.seedSearchStringFromSelection": false }"#)
                .unwrap()
                .ui()
                .seed_search_string_from_selection,
            SeedSearchString::Never
        );
    }

    #[test]
    fn wrong_types_are_reported_and_ignored() {
        let settings = Settings::parse(
            r#"{ "search.smartCase": "yes", "search.maxResults": -1, "files.exclude": [],
                 "editor.find.seedSearchStringFromSelection": "sometimes",
                 "files.watcherExclude": { "a[": true, "!b[": true, "ok": true } }"#,
        )
        .unwrap();
        assert_eq!(settings.search, SearchSettings::default());
        assert_eq!(settings.files_exclude, Settings::default().files_exclude);
        assert!(settings.watcher_exclude.plain_patterns().contains(&"ok".to_owned()));
        assert_eq!(settings.warnings.len(), 6, "{:#?}", settings.warnings);
        assert!(Settings::parse("[]").is_err());
    }

    #[test]
    fn user_then_workspace_and_broken_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join(".vscode")).unwrap();
        assert_eq!(Settings::load(None, &root), Settings::default());

        let user = dir.path().join("user.json");
        std::fs::write(&user, r#"{ "search.smartCase": true, "search.maxResults": 5, "files.exclude": { "**/dist": true, "**/out": true } }"#).unwrap();
        std::fs::write(
            workspace_settings_path(&root),
            r#"{ "search.maxResults": 7, "files.exclude": { "**/out": false } }"#,
        )
        .unwrap();
        let settings = Settings::load(Some(&user), &root);
        assert!(settings.search.smart_case);
        assert_eq!(settings.search.max_results, Some(7));
        let files = settings.files_exclude.plain_patterns();
        assert!(files.contains(&"**/dist".to_owned()) && !files.contains(&"**/out".to_owned()));

        // A broken workspace file is ignored on its own; the user settings stay.
        std::fs::write(workspace_settings_path(&root), "{ \"files.exclude\": ").unwrap();
        let settings = Settings::load(Some(&user), &root);
        assert_eq!(settings.search.max_results, Some(5));
        assert_eq!(settings.warnings.len(), 1);
        assert!(settings.warnings[0].contains("Its settings are ignored"), "{:?}", settings.warnings);
        // Problems name the file they come from.
        std::fs::write(&user, r#"{ "search.smartCase": 1 }"#).unwrap();
        let settings = Settings::load(Some(&user), &dir.path().join("none"));
        assert!(settings.warnings[0].starts_with(&user.display().to_string()), "{:?}", settings.warnings);
    }

    #[test]
    fn finds_the_user_settings_of_installed_vs_code_variants() {
        let config = Path::new("/cfg");
        let stable = config.join("Code/User/settings.json");
        let codium = config.join("VSCodium/User/settings.json");
        assert_eq!(user_settings_in(config, |_| false), stable);
        assert_eq!(user_settings_in(config, |p| p == codium), codium);
        assert_eq!(user_settings_in(config, |p| p == codium || p == stable), stable);
    }

    #[test]
    fn strips_comments_and_trailing_commas_outside_strings() {
        assert_eq!(strip_jsonc("{\"a\": \"//,}\", // c\n \"b\": [1,],}"), "{\"a\": \"//,}\", \n \"b\": [1]}");
        assert_eq!(strip_jsonc("\u{feff}{\"a\\\"//\": 1 /* x */}"), "{\"a\\\"//\": 1  }");
    }
}
