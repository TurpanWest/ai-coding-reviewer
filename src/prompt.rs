use crate::ast::{CallEdge, FileAstContext, Symbol, SymbolKind};
use crate::models::REVIEW_JSON_SCHEMA;

// ── System prompt (cached portion) ───────────────────────────────────────────

/// Build the **system prompt** that will be submitted with `cache_control:
/// ephemeral` (MiniMax/Anthropic) or as the stable prefix (DeepSeek).
///
/// This is the large, stable part that gets cached between requests.
/// `policy_text` is the raw Markdown content of the security/coding policy file.
pub fn build_system_prompt(policy_text: &str) -> String {
    format!(
        r#"You are a ruthless, expert security and correctness code reviewer operating inside a fully automated CI/CD pipeline.
There are no human reviewers in this loop. Your findings directly gate production deployments.

## Your Mission
Analyse the provided code diff and its surrounding AST context with extreme rigour.
Detect every security vulnerability, logic error, data race, resource leak, API misuse,
and violation of the policies below. Be brutally honest — false negatives are catastrophic.

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
  - Only report >= 0.9 if you are genuinely certain after thorough analysis
  - If any ambiguity exists, report 0.7–0.89 (which will trigger a pipeline block)
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
"#,
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
    out.push_str("3. The outgoing call graph from each changed symbol\n\n");
    out.push_str("---\n\n");

    for ctx in contexts {
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
                out.push_str(&format_symbol(sym));
                out.push('\n');
            }
        }

        // ── Call graph ────────────────────────────────────────────────────
        if !ctx.call_edges.is_empty() {
            out.push_str("#### Call Graph (from changed symbols)\n\n");
            let grouped = group_edges_by_caller(&ctx.call_edges);
            for (caller, callees) in &grouped {
                out.push_str(&format!("- `{}` calls: {}\n", caller, callees.join(", ")));
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
                    out.push_str(&format_symbol(sym));
                    out.push('\n');
                }
            }
        }

        out.push_str("---\n\n");
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

fn format_symbol(sym: &Symbol) -> String {
    let lang = match sym.kind {
        SymbolKind::Function => "rust",
        SymbolKind::ImplBlock => "rust",
        _ => "rust",
    };
    format!(
        "**`{}` {} (lines {}–{})**\n```{}\n{}\n```\n",
        sym.name, sym.kind, sym.start_line, sym.end_line, lang, sym.source
    )
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
