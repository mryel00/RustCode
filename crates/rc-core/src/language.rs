//! Maps file paths to LSP language identifiers.
//! Identifiers follow the LSP spec list, which Monaco also uses.

use std::path::Path;

pub fn language_id_for_path(path: &Path) -> &'static str {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    match file_name {
        "Cargo.lock" => return "toml",
        "Makefile" | "makefile" | "GNUmakefile" => return "makefile",
        "Dockerfile" => return "dockerfile",
        "CMakeLists.txt" => return "cmake",
        ".gitignore" | ".dockerignore" => return "ignore",
        _ => {}
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "go" => "go",
        "java" => "java",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "sh" | "bash" | "zsh" => "shellscript",
        "ps1" => "powershell",
        "json" => "json",
        "jsonc" => "jsonc",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "xml" | "svg" => "xml",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "md" | "markdown" => "markdown",
        "sql" => "sql",
        "dart" => "dart",
        "zig" => "zig",
        "ex" | "exs" => "elixir",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "r" => "r",
        "scala" => "scala",
        _ => "plaintext",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_languages() {
        assert_eq!(language_id_for_path(Path::new("src/main.rs")), "rust");
        assert_eq!(language_id_for_path(Path::new("App.TSX")), "typescriptreact");
        assert_eq!(language_id_for_path(Path::new("Cargo.lock")), "toml");
        assert_eq!(language_id_for_path(Path::new("README")), "plaintext");
    }
}
