# Code Review Policy
#
# Copy this file to .github/review-policy.md and edit to match your project.
# Models must cite rule IDs in findings (e.g. SEC-001).
#
# ── PR size note ─────────────────────────────────────────────────────────────
# This file is injected into every LLM call alongside the diff and AST context.
# Keep PRs small so the models have enough attention budget for the rules below:
#   • Sweet spot:  ≤ 10 files, ≤ ~500 lines of net diff
#   • Acceptable:  ≤ 30 files, ≤ ~1500 lines
#   • Split when larger — one logical change per PR (see README "Recommended PR size")
# This is human guidance for PR authors, not a rule the reviewer enforces.

## Security

- SEC-001: Never log credentials, tokens, secrets, or PII — not even partially
- SEC-002: All database queries must use parameterized statements; no string interpolation
- SEC-003: File paths derived from user input must be validated and confined to an allowed root
- SEC-004: Cryptographic operations must use vetted library primitives — no hand-rolled implementations
- SEC-005: Sensitive values must not be stored in environment variables accessible to child processes unnecessarily
- SEC-006: All external HTTP responses must be validated before use; do not trust status 200 alone

## Correctness

- LOGIC-001: Every error must be handled or explicitly propagated — silent ignores are not allowed
- LOGIC-002: Async tasks must not hold locks (Mutex, RwLock) across await points
- LOGIC-003: Integer arithmetic on untrusted or unbounded input must guard against overflow
- LOGIC-004: Null/nil/None values from external sources must be checked before use
- LOGIC-005: Resource handles (files, connections, sockets) must be closed or returned to pool on all exit paths

## Performance

- PERF-001: Avoid O(n²) or worse algorithms on inputs that are unbounded or user-controlled
- PERF-002: Database or network calls inside loops must be replaced with batch operations
- PERF-003: Unbounded allocations (collecting entire result sets, loading whole files) must be justified
- PERF-004: Synchronous blocking calls must not be made inside async contexts

## Maintainability

- MAINT-001: Public API changes (added, removed, or renamed symbols) must include updated documentation
- MAINT-002: Functions exceeding 80 lines must be broken into smaller, named units
- MAINT-003: Magic numbers and magic strings must be named constants
- MAINT-004: Code duplication spanning more than 5 lines must be extracted into a shared helper
- MAINT-005: TODO/FIXME comments must reference a tracking issue; bare TODOs are not allowed
