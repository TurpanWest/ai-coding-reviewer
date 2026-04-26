use crate::ast::{CallEdge, FileAstContext, Symbol};
use crate::models::REVIEW_JSON_SCHEMA;

// ── Ignore annotations ────────────────────────────────────────────────────────

/// A single `// ai-reviewer: ignore[RULE-ID]` annotation found in the diff.
struct IgnoreAnnotation {
    file: String,
    rule_id: String,
    reason: String,
}

/// Scan every added/context line in the diff for `ai-reviewer: ignore[RULE-ID]`
/// comments (supports `//`, `#`, and `--` comment styles).
fn extract_ignore_annotations(contexts: &[FileAstContext]) -> Vec<IgnoreAnnotation> {
    let mut result = Vec::new();
    for ctx in contexts {
        for line in ctx.raw_diff.lines() {
            // Only inspect lines that exist in the new file (added or context).
            let is_added = line.starts_with('+') && !line.starts_with("+++");
            let is_context = line.starts_with(' ');
            if !(is_added || is_context) {
                continue;
            }
            let content = &line[1..];
            if let Some(ann) = parse_ignore_comment(content, &ctx.file) {
                result.push(ann);
            }
        }
    }
    result
}

/// Parse a single line for the `ai-reviewer: ignore[RULE-ID]` marker.
/// Supports optional reason text after the closing `]`, separated by `—`, `--`, or `-`.
fn parse_ignore_comment(line: &str, file: &str) -> Option<IgnoreAnnotation> {
    const MARKER: &str = "ai-reviewer: ignore[";
    let pos = line.find(MARKER)?;
    let rest = &line[pos + MARKER.len()..];
    let close = rest.find(']')?;
    let rule_id = rest[..close].trim().to_string();
    if rule_id.is_empty() {
        return None;
    }
    let after = rest[close + 1..].trim();
    // Strip leading punctuation used as separator (—, --, -)
    let reason = after
        .trim_start_matches('—')
        .trim_start_matches("--")
        .trim_start_matches('-')
        .trim()
        .to_string();
    Some(IgnoreAnnotation {
        file: file.to_string(),
        rule_id,
        reason,
    })
}

// ── Review focus ──────────────────────────────────────────────────────────────

/// Each of the four review groups is assigned one exclusive focus dimension.
/// This keeps every LLM's attention narrow so it catches more within its lane.
#[derive(Debug, Clone, Copy)]
pub enum ReviewFocus {
    /// Injection, auth bypass, crypto misuse, secrets, input validation, SSRF.
    Security,
    /// Logic defects, type errors, null dereferences, races, error handling.
    Correctness,
    /// Algorithmic complexity, allocations, blocking calls, N+1, memory leaks.
    Performance,
    /// Naming, dead code, duplication, SRP violations, magic values, docs.
    Maintainability,
}

impl ReviewFocus {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewFocus::Security       => "security",
            ReviewFocus::Correctness    => "correctness",
            ReviewFocus::Performance    => "performance",
            ReviewFocus::Maintainability => "maintainability",
        }
    }
}

// ── System prompt (cached portion) ───────────────────────────────────────────

/// Build the **system prompt** that will be submitted with `cache_control:
/// ephemeral` (Anthropic) or as the stable prefix (OpenAI-compat providers).
///
/// This is the large, stable part that gets cached between requests.  It also
/// tells the model about the `read_file` / `find_symbol` tools it can call for
/// additional context (the reviewer always registers them).
/// `policy_text` is the raw Markdown content of the security/coding policy file.
pub fn build_system_prompt(policy_text: &str, focus: ReviewFocus) -> String {
    let focus_section = match focus {
        ReviewFocus::Security => {
            "## Your Assigned Focus: SECURITY\n\
             Review ONLY for: SQL/command/LDAP injection, XSS, authentication & authorisation bypass,\n\
             broken access control, sensitive data exposure, insecure or deprecated cryptography,\n\
             SSRF, path traversal, deserialization flaws, hardcoded secrets/API keys,\n\
             missing or bypassable input validation, unsafe use of eval/exec/shell.\n\
             Do NOT report correctness, performance, or maintainability issues — those are handled by dedicated reviewers.\n\n"
        }
        ReviewFocus::Correctness => {
            "## Your Assigned Focus: CORRECTNESS\n\
             Review ONLY for: logic defects, type errors, integer overflow/underflow,\n\
             null/nil dereferences, use-after-free, off-by-one errors, incorrect error handling,\n\
             unchecked return values, data races & concurrency bugs, broken invariants,\n\
             silent failure paths, API misuse that causes wrong behaviour, regressions.\n\
             Do NOT report security, performance, or maintainability issues — those are handled by dedicated reviewers.\n\n"
        }
        ReviewFocus::Performance => {
            "## Your Assigned Focus: PERFORMANCE\n\
             Review ONLY for: algorithmic complexity (e.g. O(n²) where O(n) is achievable),\n\
             unnecessary heap allocations or redundant clones in hot paths,\n\
             blocking / synchronous calls inside async executors,\n\
             N+1 query patterns, unbounded memory growth, cache-unfriendly data access,\n\
             holding locks or large allocations across await points.\n\
             Do NOT report security, correctness, or maintainability issues — those are handled by dedicated reviewers.\n\n"
        }
        ReviewFocus::Maintainability => {
            "## Your Assigned Focus: MAINTAINABILITY\n\
             Review ONLY for: unclear naming, dead or unreachable code, duplicated logic,\n\
             overly complex control flow, missing or misleading comments/documentation,\n\
             single-responsibility violations, hardcoded magic values, test coverage gaps\n\
             for new branches, use of deprecated APIs, tight coupling & missing abstractions.\n\
             Do NOT report security, correctness, or performance issues — those are handled by dedicated reviewers.\n\n"
        }
    };

    format!(
        r#"{focus_section}You are a ruthless, expert security and correctness code reviewer operating inside a fully automated CI/CD pipeline.
There are no human reviewers in this loop. Your findings directly gate production deployments.

## Your Mission
Analyse the provided code diff and its surrounding AST context with rigour.
Detect every **genuine** issue within your assigned focus area above.

Both false negatives AND false positives carry real cost:
- A missed real bug ships to production.
- A false positive blocks a valid change, wastes developer time, and erodes trust in the tool.

When you are uncertain whether a pattern is a real defect versus intentional design:
- Prefer `verdict: "pass"` with `confidence` 0.80–0.89 and describe your uncertainty in `reasoning`.
- Do NOT default to `verdict: "fail"` simply because you cannot prove the code is safe.
- Reserve high-confidence `fail` for clear, unambiguous defects with no plausible legitimate interpretation.

## Company Security & Coding Policy
{policy_text}

---

## Output Contract
You MUST respond with a single, raw JSON object — no markdown fences, no commentary, no explanation outside the JSON.
The object must strictly conform to this JSON Schema:

```json
{REVIEW_JSON_SCHEMA}
```

Field semantics:
- `model_id`  : your model identifier string (e.g. "minimax-text-01")
- `verdict`   : "pass" if code is safe to merge, "fail" otherwise
- `confidence`: float 0.0–1.0 reflecting your certainty in the verdict
  - Set >= 0.90 only when you are genuinely certain the verdict is correct after thorough analysis
  - If you suspect an issue but cannot rule out intentional design, set confidence 0.80–0.89 with `verdict: "pass"` and explain in `reasoning`
  - Do NOT lower confidence on a `pass` verdict just because ambiguity exists — only lower it when you genuinely doubt your own verdict
- `findings`  : every defect found, even INFO-level observations
- `reasoning` : concise chain-of-thought (2–5 sentences) explaining your verdict

## Severity Guide
- CRITICAL : Remote code execution, privilege escalation, SQL/command injection, auth bypass
- HIGH     : Data exposure, insecure crypto, SSRF, path traversal, panics in production paths
- MEDIUM   : Logic errors, resource leaks, incorrect error handling, unsafe unwrap
- LOW      : Style violations that could mask bugs, dead code, minor API misuse
- INFO     : Suggestions, refactoring notes, non-blocking observations

## Rules
1. Do NOT produce markdown outside the JSON object.
2. Do NOT truncate the JSON — emit the complete object.
3. If you find NO issues, still produce the JSON with `findings: []` and an honest confidence score.
4. `line_start` and `line_end` must refer to line numbers in the NEW version of the file.

## Tool Usage
You have access to tools to read additional source code from the repository.
Use `read_file` or `find_symbol` when:
- The diff calls a function whose full definition is NOT shown in the context above
- You need to verify a callee's implementation to judge correctness or security
- You need to see how a changed interface is used elsewhere in the codebase

Call tools as needed before producing your verdict. When you have sufficient context,
output the final JSON review result as your last message.
Do NOT call tools to re-read content already visible in the diff or symbol context above.
"#,
        focus_section = focus_section,
        policy_text = policy_text,
        REVIEW_JSON_SCHEMA = REVIEW_JSON_SCHEMA,
    )
}

// ── User prompt (dynamic portion) ────────────────────────────────────────────

/// Build the **user prompt** for the first (non-retry) attempt.
/// `contexts` is one entry per changed file.
pub fn build_user_prompt(contexts: &[FileAstContext]) -> String {
    let mut out = String::from("## Code Review Request\n\n");
    out.push_str("Review the following changes. For each file you will receive:\n");
    out.push_str("1. The unified diff\n");
    out.push_str("2. The full AST context of every changed function/type\n");
    out.push_str("3. Bare callee names referenced from inside each changed symbol — single-file, unresolved (see caveat below)\n\n");
    out.push_str("---\n\n");

    for ctx in contexts {
        let lang = lang_from_file(&ctx.file);

        out.push_str(&format!("### File: `{}`\n\n", ctx.file));

        // ── Raw diff ──────────────────────────────────────────────────────
        out.push_str("#### Unified Diff\n\n```diff\n");
        out.push_str(&ctx.raw_diff);
        out.push_str("\n```\n\n");

        // ── Changed symbols ───────────────────────────────────────────────
        if ctx.changed_symbols.is_empty() {
            out.push_str("*No named symbols overlap with this diff (binary or unsupported language).*\n\n");
        } else {
            out.push_str("#### Changed Symbols (full definition)\n\n");
            for sym in &ctx.changed_symbols {
                out.push_str(&format_symbol(sym, lang));
                out.push('\n');
            }
        }

        // ── Callee name hints (single-file, unresolved) ───────────────────
        if !ctx.call_edges.is_empty() {
            out.push_str("#### Callee Name Hints (from changed symbols)\n\n");
            out.push_str(
                "> ⚠️ These are **bare identifier strings** extracted from call sites inside each changed symbol — \
                 not resolved symbols. There is no name resolution, no type inference, and no cross-file linking. \
                 `auth::foo`, `self.foo`, and a local `foo` all appear as `\"foo\"`; `vec.push` and `string.push` both \
                 appear as `\"push\"`. Use as a hint about what was referenced, not as a fact about which function was called.\n\n",
            );
            let grouped = group_edges_by_caller(&ctx.call_edges);
            for (caller, callees) in &grouped {
                out.push_str(&format!("- `{}` references: {}\n", caller, callees.join(", ")));
            }

            // Inline callee definitions that exist in the same file
            let callee_names: std::collections::HashSet<String> = ctx
                .call_edges
                .iter()
                .map(|e| e.callee.clone())
                .collect();
            let callee_syms: Vec<&Symbol> = ctx
                .all_symbols
                .iter()
                .filter(|s| callee_names.contains(&s.name))
                .collect();

            if !callee_syms.is_empty() {
                out.push_str("\n#### Callee Definitions (same file, for context)\n\n");
                for sym in callee_syms {
                    out.push_str(&format_symbol(sym, lang));
                    out.push('\n');
                }
            }
        }

        out.push_str("---\n\n");
    }

    // ── Acknowledged exceptions ───────────────────────────────────────────
    let annotations = extract_ignore_annotations(contexts);
    if !annotations.is_empty() {
        out.push_str("## Acknowledged Exceptions\n\n");
        out.push_str(
            "The developer has explicitly annotated the following patterns as intentional.\n\
             Do NOT raise findings for these specific rule IDs at these locations.\n\
             You may still note them in `reasoning` if you disagree, but they must NOT\n\
             appear in `findings` and must NOT cause a `fail` verdict on their own.\n\n",
        );
        for ann in &annotations {
            if ann.reason.is_empty() {
                out.push_str(&format!("- `{}` — rule `{}`\n", ann.file, ann.rule_id));
            } else {
                out.push_str(&format!(
                    "- `{}` — rule `{}`: {}\n",
                    ann.file, ann.rule_id, ann.reason
                ));
            }
        }
        out.push('\n');
    }

    out.push_str(
        "Now produce the JSON review result. Remember: output ONLY the raw JSON object.\n",
    );
    out
}

// ── Self-correction prompt ────────────────────────────────────────────────────

/// Build the correction prompt when a model's previous response failed JSON
/// parsing.  Includes the exact `serde` error and the expected schema.
pub fn build_correction_prompt(
    original_user_prompt: &str,
    bad_response: &str,
    parse_error: &str,
    attempt: u32,
    max_attempts: u32,
) -> String {
    format!(
        r#"Your previous response (attempt {attempt}/{max_attempts}) failed JSON validation.

**Parse error**: `{parse_error}`

**Your invalid response was**:
```
{bad_response}
```

You MUST fix the JSON and re-emit a single, complete, valid JSON object.
Rules:
- No markdown code fences
- No text before or after the JSON object
- All required fields must be present
- `verdict` must be exactly `"pass"` or `"fail"` (lowercase)
- `confidence` must be a number between 0.0 and 1.0
- `findings` must be a JSON array (use `[]` if empty)

Target schema:
```json
{REVIEW_JSON_SCHEMA}
```

The original review request for context:
---
{original_user_prompt}
"#,
        attempt = attempt,
        max_attempts = max_attempts,
        parse_error = parse_error,
        bad_response = bad_response,
        REVIEW_JSON_SCHEMA = REVIEW_JSON_SCHEMA,
        original_user_prompt = original_user_prompt,
    )
}

// ── Formatting helpers ────────────────────────────────────────────────────────

fn format_symbol(sym: &Symbol, lang: &str) -> String {
    format!(
        "**`{}` {} (lines {}–{})**\n```{}\n{}\n```\n",
        sym.name, sym.kind, sym.start_line, sym.end_line, lang, sym.source
    )
}

/// Map a file path's extension to the corresponding Markdown code-fence language tag.
/// Must stay in sync with `ast::detect_language`.
fn lang_from_file(file: &str) -> &'static str {
    match std::path::Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
    {
        Some("rs")                                                  => "rust",
        Some("py")                                                  => "python",
        Some("go")                                                  => "go",
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs")       => "javascript",
        Some("ts") | Some("tsx")                                    => "typescript",
        Some("java")                                                => "java",
        Some("c") | Some("h")                                       => "c",
        Some("cpp") | Some("cc") | Some("cxx") | Some("hpp") | Some("hxx") => "cpp",
        Some("rb")                                                  => "ruby",
        Some("cs")                                                  => "csharp",
        Some("sh") | Some("bash")                                   => "bash",
        Some("scala") | Some("sc")                                  => "scala",
        _                                                           => "text",
    }
}

fn group_edges_by_caller(
    edges: &[CallEdge],
) -> std::collections::BTreeMap<&str, Vec<String>> {
    let mut map: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    for edge in edges {
        map.entry(edge.caller.as_str())
            .or_default()
            .push(format!("`{}`", edge.callee));
    }
    map
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::FileAstContext;

    fn ctx(file: &str, raw_diff: &str) -> FileAstContext {
        FileAstContext {
            file: file.into(),
            changed_symbols: vec![],
            all_symbols: vec![],
            call_edges: vec![],
            raw_diff: raw_diff.into(),
        }
    }

    // ── parse_ignore_comment ──────────────────────────────────────────────────

    #[test]
    fn test_ignore_double_slash() {
        let ann = parse_ignore_comment("// ai-reviewer: ignore[SEC-001]", "f.rs").unwrap();
        assert_eq!(ann.rule_id, "SEC-001");
        assert!(ann.reason.is_empty());
    }

    #[test]
    fn test_ignore_hash_style() {
        let ann = parse_ignore_comment("# ai-reviewer: ignore[PERF-042]", "f.py").unwrap();
        assert_eq!(ann.rule_id, "PERF-042");
    }

    #[test]
    fn test_ignore_with_dash_reason() {
        let ann = parse_ignore_comment(
            "// ai-reviewer: ignore[SEC-001] - intentional fallthrough",
            "f.rs",
        ).unwrap();
        assert_eq!(ann.rule_id, "SEC-001");
        assert_eq!(ann.reason, "intentional fallthrough");
    }

    #[test]
    fn test_ignore_with_emdash_reason() {
        let ann = parse_ignore_comment(
            "// ai-reviewer: ignore[SEC-001] — safe here",
            "f.rs",
        ).unwrap();
        assert_eq!(ann.reason, "safe here");
    }

    #[test]
    fn test_ignore_empty_rule_id_rejected() {
        assert!(parse_ignore_comment("// ai-reviewer: ignore[]", "f.rs").is_none());
    }

    #[test]
    fn test_ignore_no_marker_returns_none() {
        assert!(parse_ignore_comment("// just a comment", "f.rs").is_none());
    }

    // ── extract_ignore_annotations ────────────────────────────────────────────

    #[test]
    fn test_extract_from_added_line() {
        let raw = "+// ai-reviewer: ignore[R1]\n";
        let anns = extract_ignore_annotations(&[ctx("f.rs", raw)]);
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].rule_id, "R1");
    }

    #[test]
    fn test_extract_from_context_line() {
        let raw = " // ai-reviewer: ignore[R2]\n";
        let anns = extract_ignore_annotations(&[ctx("f.rs", raw)]);
        assert_eq!(anns.len(), 1);
    }

    #[test]
    fn test_removed_lines_not_extracted() {
        // Lines starting with `-` are deleted — ignore annotations on them
        // should NOT suppress findings in the new file.
        let raw = "-// ai-reviewer: ignore[R1]\n";
        let anns = extract_ignore_annotations(&[ctx("f.rs", raw)]);
        assert!(anns.is_empty());
    }

    // ── build_user_prompt ─────────────────────────────────────────────────────

    #[test]
    fn test_build_user_prompt_contains_filename() {
        let prompt = build_user_prompt(&[ctx("src/auth.rs", "+fn login() {}")]);
        assert!(prompt.contains("src/auth.rs"));
    }

    #[test]
    fn test_build_user_prompt_annotations_section_present() {
        let raw = "+// ai-reviewer: ignore[SEC-001] - test\n";
        let prompt = build_user_prompt(&[ctx("f.rs", raw)]);
        assert!(prompt.contains("SEC-001"));
        assert!(prompt.contains("Acknowledged Exceptions"));
    }

    // ── build_correction_prompt ───────────────────────────────────────────────

    #[test]
    fn test_build_correction_prompt_contains_attempt_info() {
        let p = build_correction_prompt("original", "{bad json}", "unexpected token", 2, 4);
        assert!(p.contains("2/4"));
        assert!(p.contains("unexpected token"));
    }

    // ── lang_from_file ────────────────────────────────────────────────────────

    #[test]
    fn test_lang_from_file_known_extensions() {
        assert_eq!(lang_from_file("src/main.rs"), "rust");
        assert_eq!(lang_from_file("app.py"), "python");
        assert_eq!(lang_from_file("main.go"), "go");
        assert_eq!(lang_from_file("index.js"), "javascript");
        assert_eq!(lang_from_file("app.jsx"), "javascript");
        assert_eq!(lang_from_file("mod.ts"), "typescript");
        assert_eq!(lang_from_file("comp.tsx"), "typescript");
        assert_eq!(lang_from_file("Foo.java"), "java");
        assert_eq!(lang_from_file("main.c"), "c");
        assert_eq!(lang_from_file("main.cpp"), "cpp");
        assert_eq!(lang_from_file("lib.rb"), "ruby");
        assert_eq!(lang_from_file("Program.cs"), "csharp");
        assert_eq!(lang_from_file("script.sh"), "bash");
        assert_eq!(lang_from_file("Main.scala"), "scala");
    }

    #[test]
    fn test_lang_from_file_unknown_extension_is_text() {
        assert_eq!(lang_from_file("data.json"), "text");
        assert_eq!(lang_from_file("Makefile"), "text");
        assert_eq!(lang_from_file("no_ext"), "text");
    }

    #[test]
    fn test_format_symbol_uses_correct_lang_fence() {
        use crate::ast::{Symbol, SymbolKind};
        let sym = Symbol {
            name: "authenticate".into(),
            kind: SymbolKind::Function,
            start_line: 10,
            end_line: 15,
            source: "def authenticate(token):\n    pass".into(),
        };
        let out = format_symbol(&sym, "python");
        assert!(out.contains("```python"), "expected python fence, got: {out}");
        assert!(!out.contains("```rust"), "should not have rust fence: {out}");
    }

    // ── ReviewFocus::as_str ───────────────────────────────────────────────────

    #[test]
    fn test_review_focus_as_str() {
        assert_eq!(ReviewFocus::Security.as_str(), "security");
        assert_eq!(ReviewFocus::Correctness.as_str(), "correctness");
        assert_eq!(ReviewFocus::Performance.as_str(), "performance");
        assert_eq!(ReviewFocus::Maintainability.as_str(), "maintainability");
    }
}
