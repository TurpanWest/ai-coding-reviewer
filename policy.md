# Code Review Policy — ai-reviewer (Rust CLI)

## Project Context
This is a production Rust CLI tool that pipes unified diffs through a 4-LLM dual-pair review engine.
It calls external AI APIs (MiniMax, DeepSeek, Anthropic, Gemini, OpenAI) and writes Markdown reports.
Correctness, reliability, and clean error handling are the primary concerns.

---

## Security Rules

- **SEC-001**: No API keys, secrets, or credentials hardcoded or leaked in logs.
- **SEC-002**: External API responses must be treated as untrusted input — validate before deserializing.
- **SEC-003**: File paths from CLI args must not allow directory traversal outside intended roots.
- **SEC-004**: Process exit codes must correctly reflect failure states (0=pass, 1=fail, 2=fatal).

## Correctness Rules

- **NULL-001**: `unwrap()` / `expect()` only allowed in tests or where the invariant is proven; use `?` or explicit error handling in production paths.
- **LOGIC-001**: Consensus gate logic must be correct — PASS requires *both* pairs passing with confidence >= threshold.
- **LOGIC-002**: `tokio::join!` arms must not share mutable state without synchronization.
- **LOGIC-003**: Retry loops must terminate — max_retries must be respected and not silently exceeded.
- **TYPE-001**: `serde` deserialization of LLM output must fail gracefully and trigger the self-correction loop, not panic.

## API & Async Rules

- **ASYNC-001**: Async functions must not block the executor (no `std::thread::sleep`, no blocking I/O in async context).
- **ASYNC-002**: All 4 reviewer futures must be truly concurrent — sequential `.await` chains inside `tokio::join!` arms defeat the purpose.
- **API-001**: HTTP errors from providers (4xx, 5xx) must be converted to `ReviewError::Completion`, not silently swallowed.

## Code Quality Rules

- **STYLE-001**: Public types and functions must have doc comments explaining purpose and invariants.
- **STYLE-002**: Dead code, unused imports, and unused variables must be removed.
- **STYLE-003**: Error messages must include enough context for diagnosis (which reviewer, which attempt, what failed).
- **NAMING-001**: Function and variable names must accurately reflect their current behavior (no misleading names after refactoring).

## Verdict
- **PASS**: Changes are correct, safe, concurrent-safe, and meet quality standards above.
- **FAIL**: Any security issue, logic bug in gate/consensus, panic path in production code, or API error mishandling.
# test
