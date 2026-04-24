//! Local file-reading tools for the LLM reviewer.
//!
//! These tools allow the LLM to request additional source-code context
//! during a review when the diff alone is insufficient.  All I/O is
//! **read-only** and restricted to files inside `source_root`.

use std::path::{Path, PathBuf};

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

// ── Shared safety limit ───────────────────────────────────────────────────────

/// Maximum lines returned by any single tool call.
const MAX_LINES: usize = 300;

// ── ReadFileTool ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ReadFileArgs {
    /// Relative path from the repository root, e.g. `"src/auth.rs"`.
    pub path: String,
    /// Optional 1-indexed first line to return (inclusive).
    pub start_line: Option<usize>,
    /// Optional 1-indexed last line to return (inclusive).
    pub end_line: Option<usize>,
}

pub struct ReadFileTool {
    source_root: PathBuf,
}

impl ReadFileTool {
    pub fn new(source_root: PathBuf) -> Self {
        Self { source_root }
    }
}

impl Tool for ReadFileTool {
    const NAME: &'static str = "read_file";

    type Error = ToolExecError;
    type Args = ReadFileArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description:
                "Read source code from a file in the repository. \
                 Use this when you need to see the full definition of a \
                 function, struct, or module that is referenced in the diff \
                 but not fully shown in the review context. \
                 Returns the file content with 1-indexed line numbers prepended."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path from the repository root (e.g. \"src/auth.rs\")"
                    },
                    "start_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "First line to return (1-indexed, inclusive). Omit to start from line 1."
                    },
                    "end_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Last line to return (1-indexed, inclusive). Omit to read to end of file."
                    }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let abs_path = safe_join(&self.source_root, &args.path)?;
        let content = std::fs::read_to_string(&abs_path)
            .map_err(|e| ToolExecError(format!("Cannot read `{}`: {e}", args.path)))?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        // Resolve line range (convert to 0-indexed).
        let start = args.start_line.map(|n| n.saturating_sub(1)).unwrap_or(0);
        let end = args
            .end_line
            .map(|n| n.min(total))
            .unwrap_or(total)
            .min(start + MAX_LINES);

        if start >= total {
            return Err(ToolExecError(format!(
                "start_line {start_1} exceeds file length ({total} lines)",
                start_1 = start + 1
            )));
        }

        let slice = &lines[start..end];
        let mut out = format!("// File: {} (lines {}–{})\n", args.path, start + 1, end);
        for (i, line) in slice.iter().enumerate() {
            out.push_str(&format!("{:>4} | {}\n", start + 1 + i, line));
        }

        if end < total {
            out.push_str(&format!(
                "\n// … {} more lines not shown (use start_line/end_line to read further)\n",
                total - end
            ));
        }

        Ok(out)
    }
}

// ── FindSymbolTool ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct FindSymbolArgs {
    /// The symbol name to search for (function, struct, class, etc.).
    pub name: String,
    /// Optional: restrict the search to this relative file path.
    pub file: Option<String>,
}

pub struct FindSymbolTool {
    source_root: PathBuf,
}

impl FindSymbolTool {
    pub fn new(source_root: PathBuf) -> Self {
        Self { source_root }
    }
}

impl Tool for FindSymbolTool {
    const NAME: &'static str = "find_symbol";

    type Error = ToolExecError;
    type Args = FindSymbolArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description:
                "Search for the definition of a named symbol (function, struct, class, \
                 trait, etc.) across the repository. Returns up to 5 matches with \
                 surrounding context lines. Use this when you see a name referenced in \
                 the diff but need to find where it is defined."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The symbol name to search for (exact, case-sensitive)"
                    },
                    "file": {
                        "type": "string",
                        "description": "Optional: relative path to restrict the search to a single file"
                    }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut matches: Vec<SymbolMatch> = Vec::new();

        if let Some(ref rel) = args.file {
            // Search a single file.
            let abs = safe_join(&self.source_root, rel)?;
            search_file_for_symbol(&abs, rel, &args.name, &mut matches);
        } else {
            // Walk the source root.
            walk_dir_for_symbol(&self.source_root, &self.source_root, &args.name, &mut matches);
        }

        if matches.is_empty() {
            return Ok(format!("// No definition found for symbol `{}`.\n", args.name));
        }

        let mut out = format!(
            "// Found {} match(es) for symbol `{}`:\n\n",
            matches.len().min(5),
            args.name
        );
        for m in matches.into_iter().take(5) {
            out.push_str(&format!(
                "// {}:{}\n{}",
                m.file, m.line_number, m.context
            ));
        }
        Ok(out)
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

struct SymbolMatch {
    file: String,
    line_number: usize,
    context: String,
}

/// Validate and resolve a relative path against `root`.
/// Returns an error if the resolved path escapes the root (path traversal).
fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, ToolExecError> {
    // Strip leading `/` or `./` to make join behave correctly.
    let rel = relative.trim_start_matches('/').trim_start_matches("./");
    let abs = root.join(rel);

    // Canonicalize the root (resolves symlinks, makes absolute).
    // For the candidate path we use canonicalize when the file exists, or
    // normalise manually when it doesn't — canonicalize only works on
    // existing paths, so a missing file must not be treated as a traversal.
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let canon_abs = if abs.exists() {
        abs.canonicalize().unwrap_or_else(|_| abs.clone())
    } else {
        // File doesn't exist yet; normalise without canonicalize by resolving
        // the parent directory (which must exist) and appending the filename.
        // This correctly handles `root/src/missing.rs` without false-positives.
        let parent = abs.parent().and_then(|p| p.canonicalize().ok());
        match (parent, abs.file_name()) {
            (Some(p), Some(name)) => p.join(name),
            _ => abs.clone(),
        }
    };

    if !canon_abs.starts_with(&canon_root) {
        return Err(ToolExecError(format!(
            "Path `{relative}` escapes the repository root — access denied."
        )));
    }
    Ok(abs)
}

/// Source-file extensions worth searching.
fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(
            "rs" | "py" | "go" | "js" | "ts" | "jsx" | "tsx" | "java" | "c" | "cpp" | "h"
                | "hpp" | "cs" | "rb" | "sh" | "scala"
        )
    )
}

fn search_file_for_symbol(abs: &Path, rel: &str, name: &str, out: &mut Vec<SymbolMatch>) {
    if out.len() >= 5 {
        return;
    }
    let Ok(content) = std::fs::read_to_string(abs) else {
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if out.len() >= 5 {
            break;
        }
        // Match lines that look like definition sites (not just call sites).
        if line.contains(name) && looks_like_definition(line, name) {
            let ctx_start = i.saturating_sub(1);
            let ctx_end = (i + 6).min(lines.len());
            let mut ctx = String::new();
            for (j, l) in lines[ctx_start..ctx_end].iter().enumerate() {
                ctx.push_str(&format!("{:>4} | {}\n", ctx_start + 1 + j, l));
            }
            ctx.push('\n');
            out.push(SymbolMatch {
                file: rel.to_string(),
                line_number: i + 1,
                context: ctx,
            });
        }
    }
}

fn walk_dir_for_symbol(dir: &Path, root: &Path, name: &str, out: &mut Vec<SymbolMatch>) {
    if out.len() >= 5 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= 5 {
            break;
        }
        let path = entry.path();
        // Skip hidden dirs and common non-source dirs.
        if let Some(fname) = path.file_name().and_then(|f| f.to_str())
            && (fname.starts_with('.') || matches!(fname, "target" | "node_modules" | "vendor"))
        {
            continue;
        }
        if path.is_dir() {
            walk_dir_for_symbol(&path, root, name, out);
        } else if path.is_file() && is_source_file(&path) {
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            search_file_for_symbol(&path, &rel, name, out);
        }
    }
}

/// Heuristic: does this line look like a *definition* of `name` rather than a call?
fn looks_like_definition(line: &str, name: &str) -> bool {
    let trimmed = line.trim();
    // Rust: fn name, struct name, enum name, trait name, impl name, type name
    // Python: def name, class name
    // Go: func name, type name
    // Java/C#: class name, interface name, void name(, public … name(
    let prefixes = [
        "fn ", "pub fn ", "async fn ", "pub async fn ",
        "struct ", "pub struct ", "enum ", "pub enum ",
        "trait ", "pub trait ", "impl ", "type ", "pub type ",
        "def ", "class ", "func ", "interface ",
    ];
    for prefix in prefixes {
        if let Some(rest) = trimmed.strip_prefix(prefix)
            && let Some(after) = rest.strip_prefix(name)
        {
            // Must be followed by non-ident char (space, <, (, {, :)
            if after.is_empty()
                || after.starts_with(|c: char| !c.is_alphanumeric() && c != '_')
            {
                return true;
            }
        }
    }
    false
}

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolExecError(String);

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn make_temp(files: &[(&str, &str)]) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(content.as_bytes()).unwrap();
        }
        dir
    }

    // ── safe_join ─────────────────────────────────────────────────────────────

    #[test]
    fn test_safe_join_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let result = safe_join(dir.path(), "../../etc/passwd");
        assert!(result.is_err(), "path traversal must be rejected");
        assert!(result.unwrap_err().0.contains("escapes"));
    }

    #[test]
    fn test_safe_join_normal_accepted() {
        let dir = make_temp(&[("src/lib.rs", "fn foo() {}")]);
        let result = safe_join(dir.path(), "src/lib.rs");
        assert!(result.is_ok());
    }

    // ── ReadFileTool ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_read_file_full() {
        let content = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let dir = make_temp(&[("foo.rs", &content)]);
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let result = tool.call(ReadFileArgs { path: "foo.rs".into(), start_line: None, end_line: None }).await.unwrap();
        assert!(result.contains("line 1"));
        assert!(result.contains("line 10"));
    }

    #[tokio::test]
    async fn test_read_file_line_range() {
        let content = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let dir = make_temp(&[("foo.rs", &content)]);
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let result = tool.call(ReadFileArgs { path: "foo.rs".into(), start_line: Some(3), end_line: Some(5) }).await.unwrap();
        assert!(result.contains("line 3"));
        assert!(result.contains("line 5"));
        assert!(!result.contains("line 1"));
        assert!(!result.contains("line 6"));
    }

    #[tokio::test]
    async fn test_read_file_truncation() {
        let content = (1..=400).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let dir = make_temp(&[("big.rs", &content)]);
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let result = tool.call(ReadFileArgs { path: "big.rs".into(), start_line: None, end_line: None }).await.unwrap();
        assert!(result.contains("more lines not shown"));
    }

    #[tokio::test]
    async fn test_read_file_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let result = tool.call(ReadFileArgs { path: "../../etc/passwd".into(), start_line: None, end_line: None }).await;
        assert!(result.is_err());
    }

    // ── FindSymbolTool ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_find_symbol_found() {
        let src = "use std::io;\n\npub fn authenticate(token: &str) -> bool {\n    true\n}\n";
        let dir = make_temp(&[("src/auth.rs", src)]);
        let tool = FindSymbolTool::new(dir.path().to_path_buf());
        let result = tool.call(FindSymbolArgs { name: "authenticate".into(), file: None }).await.unwrap();
        assert!(result.contains("authenticate"), "should find the symbol: {result}");
        assert!(result.contains("src/auth.rs"), "should report the file: {result}");
    }

    #[tokio::test]
    async fn test_find_symbol_not_found() {
        let dir = make_temp(&[("src/lib.rs", "fn foo() {}")]);
        let tool = FindSymbolTool::new(dir.path().to_path_buf());
        let result = tool.call(FindSymbolArgs { name: "nonexistent_xyz".into(), file: None }).await.unwrap();
        assert!(result.contains("No definition found"));
    }

    #[tokio::test]
    async fn test_find_symbol_in_specific_file() {
        let src = "pub struct MyStruct { x: i32 }\n";
        let dir = make_temp(&[("src/types.rs", src)]);
        let tool = FindSymbolTool::new(dir.path().to_path_buf());
        let result = tool.call(FindSymbolArgs { name: "MyStruct".into(), file: Some("src/types.rs".into()) }).await.unwrap();
        assert!(result.contains("MyStruct"));
    }
}
