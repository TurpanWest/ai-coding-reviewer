use anyhow::Result;
use std::path::PathBuf;

// ── Public types ──────────────────────────────────────────────────────────────

/// A contiguous range of lines in the *new* version of a file that were touched
/// by this hunk (added or modified). Line numbers are 1-indexed.
#[derive(Debug, Clone)]
pub struct HunkRange {
    pub start_line: u32,
    pub end_line: u32,
}

/// Everything we extracted from one file's diff.
#[derive(Debug, Clone)]
pub struct FileDiff {
    /// Path as it appears in the `--- a/…` header (None for /dev/null).
    pub original_path: Option<PathBuf>,
    /// Path as it appears in the `+++ b/…` header (None for /dev/null).
    pub new_path: Option<PathBuf>,
    pub hunks: Vec<HunkRange>,
    /// The raw unified-diff text for this one file, captured during parsing.
    /// Includes the `diff --git`, `---`, `+++`, `@@` headers and all hunk lines.
    pub raw_chunk: String,
}

impl FileDiff {
    /// The canonical path to use when reading the on-disk source file.
    pub fn source_path(&self) -> Option<&PathBuf> {
        self.new_path.as_ref().or(self.original_path.as_ref())
    }

    /// Returns true when this diff represents actual text changes (not a pure
    /// rename or permission-change with no hunks).
    pub fn has_changes(&self) -> bool {
        !self.hunks.is_empty()
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

/// Parse a unified diff string (e.g. the output of `git diff`) into a list of
/// per-file descriptors.
///
/// We implement the parser manually rather than pulling in a full diff crate
/// because we only need line-number extraction plus per-file raw-chunk capture,
/// and want precise control over edge cases (new-file, deleted-file, binary).
pub fn parse_diff(diff_text: &str) -> Result<Vec<FileDiff>> {
    let mut results: Vec<FileDiff> = Vec::new();

    // State machine over lines
    let mut orig_path: Option<PathBuf> = None;
    let mut new_path: Option<PathBuf> = None;
    let mut hunks: Vec<HunkRange> = Vec::new();
    let mut raw_chunk = String::new();

    // Inside a hunk: track the current new-file line cursor and whether we
    // have seen at least one `+` line (to build the tight range).
    let mut in_hunk = false;
    let mut hunk_new_start: u32 = 0;
    let mut hunk_new_cursor: u32 = 0;
    let mut hunk_first_change: Option<u32> = None;
    let mut hunk_last_change: Option<u32> = None;

    let flush_hunk = |hunks: &mut Vec<HunkRange>,
                      first: &mut Option<u32>,
                      last: &mut Option<u32>| {
        if let (Some(s), Some(e)) = (*first, *last) {
            hunks.push(HunkRange {
                start_line: s,
                end_line: e,
            });
        }
        *first = None;
        *last = None;
    };

    let flush_file = |results: &mut Vec<FileDiff>,
                      orig: &mut Option<PathBuf>,
                      new: &mut Option<PathBuf>,
                      hunks: &mut Vec<HunkRange>,
                      raw: &mut String| {
        if orig.is_some() || new.is_some() {
            results.push(FileDiff {
                original_path: orig.take(),
                new_path: new.take(),
                hunks: std::mem::take(hunks),
                raw_chunk: std::mem::take(raw),
            });
        }
    };

    for line in diff_text.lines() {
        // ── New file header ────────────────────────────────────────────────
        if line.starts_with("diff --git ") {
            // Flush any previous hunk and file
            flush_hunk(&mut hunks, &mut hunk_first_change, &mut hunk_last_change);
            flush_file(
                &mut results,
                &mut orig_path,
                &mut new_path,
                &mut hunks,
                &mut raw_chunk,
            );
            in_hunk = false;
            raw_chunk.push_str(line);
            raw_chunk.push('\n');
            continue;
        }

        // Capture every line that belongs to the current file chunk.
        raw_chunk.push_str(line);
        raw_chunk.push('\n');

        // ── --- / +++ headers ─────────────────────────────────────────────
        if let Some(rest) = line.strip_prefix("--- ") {
            flush_hunk(&mut hunks, &mut hunk_first_change, &mut hunk_last_change);
            in_hunk = false;
            orig_path = parse_path(rest);
            continue;
        }

        if let Some(rest) = line.strip_prefix("+++ ") {
            new_path = parse_path(rest);
            continue;
        }

        // ── @@ hunk header ────────────────────────────────────────────────
        // Format: @@ -L[,S] +L[,S] @@[ optional context ]
        if line.starts_with("@@") {
            flush_hunk(&mut hunks, &mut hunk_first_change, &mut hunk_last_change);
            match parse_hunk_header(line) {
                Some((new_start, _new_len)) => {
                    in_hunk = true;
                    hunk_new_start = new_start;
                    hunk_new_cursor = new_start;
                    hunk_first_change = None;
                    hunk_last_change = None;
                }
                None => {
                    in_hunk = false;
                }
            }
            continue;
        }

        if !in_hunk {
            continue;
        }

        // ── Hunk body lines ───────────────────────────────────────────────
        if line.starts_with('+') && !line.starts_with("+++") {
            // Added line — counts in new file
            if hunk_first_change.is_none() {
                hunk_first_change = Some(hunk_new_cursor);
            }
            hunk_last_change = Some(hunk_new_cursor);
            hunk_new_cursor += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            // Removed line — mark the surrounding context line as changed
            // (the removal point appears at the current new-file cursor)
            if hunk_first_change.is_none() {
                hunk_first_change = Some(hunk_new_cursor.saturating_sub(1).max(hunk_new_start));
            }
            hunk_last_change = Some(hunk_new_cursor.saturating_sub(1).max(hunk_new_start));
            // Removed lines do NOT advance the new-file cursor
        } else if line.starts_with(' ') || line.is_empty() {
            // Context line — advances new-file cursor
            hunk_new_cursor += 1;
        }
        // Lines starting with `\` (e.g. "\ No newline at end of file") are ignored
    }

    // Flush tail
    flush_hunk(&mut hunks, &mut hunk_first_change, &mut hunk_last_change);
    flush_file(
        &mut results,
        &mut orig_path,
        &mut new_path,
        &mut hunks,
        &mut raw_chunk,
    );

    Ok(results)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Strip the `a/` or `b/` prefix added by `git diff` and return a `PathBuf`.
/// Returns `None` for `/dev/null`.
fn parse_path(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw == "/dev/null" {
        return None;
    }
    // git diff prefixes with a/ or b/; strip exactly one such prefix.
    let stripped = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    Some(PathBuf::from(stripped))
}

/// Parse `@@ -L[,S] +L[,S] @@ ...` → `(new_start, new_len)`.
/// Returns `None` on parse failure.
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    // Find the second `@@`-delimited section
    let inner = line.strip_prefix("@@")?;
    let end = inner.find("@@")?;
    let ranges = inner[..end].trim();

    // ranges is like `-L,S +L,S` or `-L +L`
    let plus_part = ranges.split_whitespace().find(|s| s.starts_with('+'))?;
    let plus_part = plus_part.trim_start_matches('+');

    if let Some((start_str, len_str)) = plus_part.split_once(',') {
        let start = start_str.parse::<u32>().ok()?;
        let len = len_str.parse::<u32>().ok()?;
        Some((start, len))
    } else {
        let start = plus_part.parse::<u32>().ok()?;
        Some((start, 1))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DIFF: &str = r#"diff --git a/src/lib.rs b/src/lib.rs
index abc1234..def5678 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,7 +10,9 @@ use std::collections::HashMap;
 fn foo() {
     let x = 1;
-    let y = 2;
+    let y = 3;
+    let z = x + y;
     println!("{}", y);
 }
"#;

    #[test]
    fn test_parse_diff_basic() {
        let files = parse_diff(SAMPLE_DIFF).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.new_path.as_ref().unwrap().to_str().unwrap(), "src/lib.rs");
        assert!(!f.hunks.is_empty());
    }

    #[test]
    fn test_parse_hunk_header() {
        assert_eq!(parse_hunk_header("@@ -10,7 +10,9 @@ context"), Some((10, 9)));
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("@@ -0,0 +1,5 @@"), Some((1, 5)));
        assert_eq!(parse_hunk_header("not a hunk"), None);
    }

    #[test]
    fn test_empty_diff_returns_empty() {
        assert!(parse_diff("").unwrap().is_empty());
    }

    #[test]
    fn test_new_file() {
        let diff = "diff --git a/new.rs b/new.rs\n\
                    new file mode 100644\n\
                    --- /dev/null\n\
                    +++ b/new.rs\n\
                    @@ -0,0 +1 @@\n\
                    +fn main() {}\n";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].original_path.is_none());
        assert!(files[0].has_changes());
    }

    #[test]
    fn test_deleted_file() {
        let diff = "diff --git a/old.rs b/old.rs\n\
                    deleted file mode 100644\n\
                    --- a/old.rs\n\
                    +++ /dev/null\n\
                    @@ -1 +0,0 @@\n\
                    -fn main() {}\n";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].new_path.is_none());
    }

    #[test]
    fn test_raw_chunk_captured_per_file() {
        let diff = "diff --git a/a.rs b/a.rs\n\
                    --- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-x\n+y\n\
                    diff --git a/b.rs b/b.rs\n\
                    --- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files[0].raw_chunk.contains("a/a.rs"));
        assert!(!files[0].raw_chunk.contains("a/b.rs"));
        assert!(files[1].raw_chunk.contains("a/b.rs"));
        assert!(!files[1].raw_chunk.contains("a/a.rs"));
    }

    #[test]
    fn test_multi_file_diff() {
        let diff = "diff --git a/a.rs b/a.rs\n\
                    --- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-x\n+y\n\
                    diff --git a/b.rs b/b.rs\n\
                    --- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path.as_ref().unwrap().to_str().unwrap(), "a.rs");
        assert_eq!(files[1].new_path.as_ref().unwrap().to_str().unwrap(), "b.rs");
    }

    #[test]
    fn test_pure_rename_has_no_changes() {
        // Rename-only: paths differ but no hunk, so has_changes() is false.
        let diff = "diff --git a/old.rs b/new.rs\n\
                    similarity index 100%\n\
                    rename from old.rs\n\
                    rename to new.rs\n\
                    --- a/old.rs\n\
                    +++ b/new.rs\n";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        assert!(!files[0].has_changes());
    }

    #[test]
    fn test_no_newline_marker_is_ignored() {
        // Lines starting with `\` must not affect hunk line counts.
        let diff = "diff --git a/x.rs b/x.rs\n\
                    --- a/x.rs\n+++ b/x.rs\n\
                    @@ -1 +1 @@\n\
                    -old\n\
                    \\ No newline at end of file\n\
                    +new\n\
                    \\ No newline at end of file\n";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].has_changes());
    }

    #[test]
    fn test_hunk_range_added_only() {
        // Simple hunk: one added line at the start (no context before it).
        // @@ -1 +1,2 @@ → new file cursor starts at 1.
        // `+line_a` and `+line_b` are both added → range [1, 2].
        let diff = concat!(
            "diff --git a/f.rs b/f.rs\n",
            "--- a/f.rs\n",
            "+++ b/f.rs\n",
            "@@ -1 +1,2 @@\n",
            "+line_a\n",
            "+line_b\n",
        );
        let files = parse_diff(diff).unwrap();
        let h = &files[0].hunks[0];
        assert_eq!(h.start_line, 1);
        assert_eq!(h.end_line, 2);
    }

    #[test]
    fn test_parse_path_strips_prefix() {
        assert_eq!(parse_path("a/src/lib.rs"), Some(PathBuf::from("src/lib.rs")));
        assert_eq!(parse_path("b/src/lib.rs"), Some(PathBuf::from("src/lib.rs")));
        assert_eq!(parse_path("/dev/null"), None);
    }
}
