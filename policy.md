# Code Review Policy — ai-reviewer (Rust CLI)

## Project Context
This is a production Rust CLI tool that pipes unified diffs through a 4-LLM dual-pair review engine.
It calls external AI APIs (MiniMax, DeepSeek, Anthropic, Gemini, OpenAI) and writes Markdown reports.
Correctness, reliability, and clean error handling are the primary concerns.

---

## Security Rules

- **SEC-001**: No API keys, secrets, or credentials hardcoded or leaked in logs.
  - *Exceptions*: Public default base URLs (e.g. `https://api.example.com`) are not secrets. Placeholder strings like `"your-api-key-here"` in example config files are not secrets. Test fixtures with obviously fake credentials are not secrets.
- **SEC-002**: External API responses must be treated as untrusted input — validate before deserializing.
  - *Exceptions*: Using `serde` with a typed schema is itself validation. You do not need manual pre-checks before `serde_json::from_str` if the target type is strict and errors are handled.
- **SEC-003**: File paths from CLI args must not allow directory traversal outside intended roots.
  - *Exceptions*: Paths that are only used for reading (not writing or executing) under a user-supplied `--source-root` are low risk. Flag only if the path is used in a write, delete, or exec context.
- **SEC-004**: Process exit codes must correctly reflect failure states (0=pass, 1=fail, 2=fatal).
  - *Exceptions*: Intermediate helper functions that return `bool` or `Result` and do not call `process::exit` are not in scope for this rule.

## Correctness Rules

- **NULL-001**: `unwrap()` / `expect()` only allowed in tests or where the invariant is proven; use `?` or explicit error handling in production paths.
  - *Exceptions*: `unwrap()` is acceptable when immediately preceded by an explicit `is_some()` / `is_ok()` guard, when used on a value constructed in the same expression that cannot fail (e.g. `Regex::new("literal").unwrap()`), or when accompanied by a `// invariant:` or `// safety:` comment explaining why the value is always `Some`. Do not flag `expect()` in `main()` or top-level setup code where a panic is the correct failure mode.
- **LOGIC-001**: Consensus gate logic must be correct — PASS requires *both* pairs passing with confidence >= threshold.
  - *Exceptions*: Utility functions that compute intermediate boolean values are not gate logic; only flag code that directly determines the final `gate_passed` field.
- **LOGIC-002**: `tokio::join!` arms must not share mutable state without synchronization.
  - *Exceptions*: Immutable shared references (`&T`) and `Arc<T>` reads are safe. Flag only when two arms hold `&mut` to the same allocation or use unsynchronized interior mutability (`Cell`, `RefCell`) across arm boundaries.
- **LOGIC-003**: Retry loops must terminate — max_retries must be respected and not silently exceeded.
  - *Exceptions*: Loops with a clearly bounded counter variable that is incremented every iteration and compared against a finite limit are compliant, even if the loop body uses `continue`.
- **TYPE-001**: `serde` deserialization of LLM output must fail gracefully and trigger the self-correction loop, not panic.
  - *Exceptions*: Deserialization of static, compile-time-known data (e.g. embedded JSON in `const` strings) may use `unwrap()` if the data is under developer control and cannot vary at runtime.

## API & Async Rules

- **ASYNC-001**: Async functions must not block the executor (no `std::thread::sleep`, no blocking I/O in async context).
  - *Exceptions*: `tokio::time::sleep` and `tokio::fs::*` are async-safe and are NOT violations. Only flag `std::thread::sleep`, `std::fs::*` inside `async fn`, and similar stdlib blocking calls.
- **ASYNC-002**: All 4 reviewer futures must be truly concurrent — sequential `.await` chains inside `tokio::join!` arms defeat the purpose.
  - *Exceptions*: Sequential `.await` within a single arm is fine as long as the arm itself is spawned concurrently with the other arms via `tokio::join!`. The rule only prohibits running arms one-by-one outside of `join!`.
- **API-001**: HTTP errors from providers (4xx, 5xx) must be converted to `ReviewError::Completion`, not silently swallowed.
  - *Exceptions*: 404 responses that are explicitly handled as a "not found" sentinel (e.g. checking `status == 404` and returning an empty result) are acceptable if the empty result is propagated correctly.

## Code Quality Rules

- **STYLE-001**: Public types and functions must have doc comments explaining purpose and invariants.
  - *Exceptions*: Simple getter/setter methods, trait implementations where the trait doc is sufficient, and private helpers that are only called once. Do not flag missing doc comments on `main()`.
- **STYLE-002**: Dead code, unused imports, and unused variables must be removed.
  - *Exceptions*: Items annotated with `#[allow(dead_code)]` with a comment explaining the reason (e.g. future use, FFI, serialization). Variables prefixed with `_` by convention. Do not flag if the compiler would not warn.
- **STYLE-003**: Error messages must include enough context for diagnosis (which reviewer, which attempt, what failed).
  - *Exceptions*: Internal panic messages in unreachable branches (e.g. `unreachable!("BUG: ...")`) do not need reviewer/attempt context.
- **NAMING-001**: Function and variable names must accurately reflect their current behavior (no misleading names after refactoring).
  - *Exceptions*: Names that are slightly imprecise but not actively misleading (e.g. `parse_path` that also strips a prefix) should be rated INFO at most, not flagged as a defect.

## Verdict
- **PASS**: Changes are correct, safe, concurrent-safe, and meet quality standards above.
- **FAIL**: Any security issue, logic bug in gate/consensus, panic path in production code, or API error mishandling.
