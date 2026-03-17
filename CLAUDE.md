# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build
cargo build --release

# Run tests
cargo test
cargo test --lib             # unit tests only
cargo test diff::tests       # single test module
cargo test test_parse_diff   # single test by name

# Lint
cargo clippy -- -D warnings

# Run (requires MINIMAX_API_KEY and DEEPSEEK_API_KEY env vars)
cargo run -- --diff <path-or-> --policy <policy.md> [--source-root <repo-root>]

# Verbose mode
cargo run -- --diff - --policy policy.md -v
```

## Architecture

The tool is a stateless CLI that pipe-lines a unified diff through six stages to produce a PASS/FAIL verdict and a Markdown report:

```
git diff → diff::parse_diff → ast::extract_context → prompt assembly
        → tokio::join!(minimax.review, deepseek.review)
        → consensus::evaluate → report::render_*  → exit code 0/1
```

**Stage details:**

1. **`src/diff.rs`** — Hand-written unified-diff parser (not using diffy's API directly). Produces `FileDiff` structs with `HunkRange` lists (1-indexed new-file line numbers).

2. **`src/ast.rs`** — tree-sitter parser for Rust (`.rs`) and Python (`.py`). Collects all top-level symbols (`function_item`, `impl_item`, `struct_item`, `enum_item`, `trait_item` for Rust; `function_definition`, `class_definition` for Python), maps changed hunk lines to overlapping symbols, then builds a call graph within those symbols. Returns `FileAstContext`.

3. **`src/prompt.rs`** — Assembles the two-part prompt: a stable system prompt (security policy + JSON schema + reviewer instructions) intended for provider-side caching, and a dynamic user prompt (per-file diff + full symbol definitions + call graph). Also builds the self-correction prompt when JSON parsing fails.

4. **`src/models/minimax.rs` and `src/models/deepseek.rs`** — Both use the OpenAI-compat `POST /v1/chat/completions` wire format via raw `reqwest`. Both implement the same self-correction loop: up to `max_retries+1` attempts, appending bad responses and correction messages to the conversation history on parse failure. Both strip `<think>...</think>` blocks (reasoning models) and markdown code fences before `serde_json` deserialization. Note: despite the ARCHITECTURE.md mentioning Anthropic-compat for MiniMax, the actual implementation uses OpenAI-compat for both.

5. **`src/consensus.rs`** — Gate logic: PASS only when both models return `Verdict::Pass` AND both `confidence >= CONFIDENCE_THRESHOLD` (0.90). Any `ReviewError` is converted to a synthetic `Verdict::Fail` with `confidence=1.0`. Finding dedup key is `(file, line_start, rule_id)`.

6. **`src/report.rs`** — Renders a Markdown cross-comparison report. Always written to disk (default: `review-report.md`). The summary line goes to stdout; gate-fail message goes to stderr.

**Key data types** (`src/models/mod.rs`):
- `ReviewResult` — what each model must produce (strict serde, deserialization failure = retry)
- `ConsensusResult` — final output combining both model results + merged findings
- `REVIEW_JSON_SCHEMA` — the JSON Schema string embedded verbatim in the system prompt

## Environment Variables

| Variable | Purpose |
|---|---|
| `MINIMAX_API_KEY` | MiniMax API key (required) |
| `DEEPSEEK_API_KEY` | DeepSeek API key (required) |
| `MINIMAX_MODEL` | Override MiniMax model ID (default: `MiniMax-M2.5`) |
| `DEEPSEEK_MODEL` | Override DeepSeek model ID (default: `deepseek-chat`) |
| `MINIMAX_BASE_URL` | Override MiniMax base URL |
| `DEEPSEEK_BASE_URL` | Override DeepSeek base URL |
| `RUST_LOG` | Tracing filter (e.g. `ai_reviewer=debug`) |

## Exit Codes

- `0` — consensus PASS (both models passed with confidence ≥ 0.90)
- `1` — consensus FAIL (any failure condition)
- `2` — fatal runtime error (bad diff path, missing policy file, etc.)
