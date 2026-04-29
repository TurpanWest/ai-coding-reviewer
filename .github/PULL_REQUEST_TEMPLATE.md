## Summary

<!-- One or two sentences: what changed and why. -->

## Risk level

**Pick one and apply the matching label** (`risk:low` / `risk:medium` / `risk:high`) on the right.

- `risk:low` — docs, tests, comments, isolated typos. Gate passes if **either** reviewer votes PASS.
- `risk:medium` — ordinary features, refactors, bug fixes. **Both** must PASS, with severity-aware confidence (default).
- `risk:high` — auth, crypto, payments, schema migrations, public API contracts. **Both** must PASS *and* both must clear the per-focus confidence threshold.

No label → defaults to `risk:medium`. The chosen level is rendered in `review-report.md`.

## Test plan

<!-- How did you verify this change? Bullets are fine. -->
