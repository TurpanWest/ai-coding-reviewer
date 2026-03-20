# Review Policy

Copy this file to `.github/review-policy.md` in your own project and customise the rules.

## Security Rules

- SEC-001: Never log credentials, tokens, secrets, or PII
- SEC-002: All SQL queries must use parameterized statements — no string interpolation
- SEC-003: File paths from user input must be validated and sandboxed before use
- SEC-004: Cryptographic operations must use standard library primitives, not hand-rolled implementations

## Correctness Rules

- LOGIC-001: Every error must be handled or explicitly propagated — no silent ignores
- LOGIC-002: Async tasks must not hold locks across await points
- LOGIC-003: Integer arithmetic on untrusted input must guard against overflow

## Performance Rules

- PERF-001: Avoid O(n²) or worse algorithms on unbounded inputs
- PERF-002: Database queries inside loops must be replaced with batch operations
- PERF-003: Allocations in hot paths must be justified with a comment

## Maintainability Rules

- MAINT-001: Public API changes must include updated documentation
- MAINT-002: Functions longer than 100 lines must be broken up
- MAINT-003: Magic numbers must be named constants
