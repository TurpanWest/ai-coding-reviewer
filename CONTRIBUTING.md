# Contributing to ai-reviewer

Thanks for taking the time to contribute. This document covers how to file
issues, how to send pull requests, and the few project-specific conventions
that make reviews go smoothly.

If you're just getting oriented, the [README](README.md) explains what the
tool does and how it's wired together; [`CLAUDE.md`](CLAUDE.md) is a short
architecture tour aimed at AI coding assistants but is also the fastest read
for humans.

---

## Filing issues

Open an issue at <https://github.com/TurpanWest/ai-reviewer/issues/new>.
Pick the kind of report that fits and include the matching information:

### Bug report

Please include:

- **What you ran** — the exact CLI invocation, or the workflow snippet if it
  was triggered from CI.
- **Diff / repro** — the smallest unified diff that reproduces it. If the
  diff is sensitive, a redacted version that still reproduces is fine; if you
  can't share it, describe its shape (file count, languages, net lines).
- **Expected vs. actual** — what verdict / report you expected, and what you
  got. Paste the stdout summary line and (when relevant) the relevant section
  of `review-report.md`.
- **Environment** — `ai-reviewer --version` (or the commit SHA you built
  from), OS, and which two providers / models you were using.
- **Logs** — re-run with `RUST_LOG=ai_reviewer=debug` and include the trace
  output for the failing call. Redact API keys.

### Feature request

Lead with the problem, not the solution. "I want to gate on X" or "review
quality drops when Y" is more useful than "add flag --foo". Sketch the
alternative shapes you considered if you have a preference.

### Security report

Do **not** open a public issue for vulnerabilities in this tool itself
(API-key leakage, prompt-injection escapes from policy text, etc.). Email
the maintainer directly — see the repository owner's profile for contact.

---

## Sending pull requests

### Before you start

For anything beyond a typo, please open or comment on an issue first so we
can agree on direction. A 30-line discussion saves a 300-line rewrite.

### Development setup

```bash
git clone https://github.com/TurpanWest/ai-reviewer
cd ai-reviewer
cargo build
cargo test
cargo clippy -- -D warnings
```

To run the binary end-to-end you need `REVIEWER_1_API_KEY` and
`REVIEWER_2_API_KEY` exported. For tests that don't hit the network, just
`cargo test` is enough — no keys needed.

### What to check before opening the PR

- `cargo build` succeeds in release and debug.
- `cargo test` passes.
- `cargo clippy -- -D warnings` is clean — clippy warnings are treated as
  errors in CI.
- New behavior has a test. If it touches `diff.rs`, `ast.rs`, or
  `consensus.rs`, prefer a unit test next to the existing ones in that
  module.
- If you changed CLI flags, env vars, exit codes, or the consensus rule,
  update the README and `CLAUDE.md` in the same PR.

### Commit messages

Follow the existing style in `git log` — short conventional-commit-ish
prefixes (`feat:`, `fix:`, `docs:`, `ci:`, `refactor:`, `test:`) and a
present-tense summary. Body is optional but welcome when the *why* isn't
obvious from the diff.

### PR size

This repo runs its own AI review gate on incoming PRs (see below), so
oversized PRs hurt you twice — slower human review and noisier AI review.
Aim for the README's sweet spot: **≤ 10 files and ≤ ~500 net lines**. If a
change genuinely can't fit, split it into stacked PRs by module or feature.

The natural exceptions called out in the README — mass renames, dependency
upgrades, generated code — apply here too. Flag them in the PR description
so reviewers know not to expect a tidy diff.

### Risk label (required)

Every PR needs a `risk:low`, `risk:medium`, or `risk:high` label. The label
selects which voting rule the AI gate applies to your change:

| Label         | Use for                                                      |
|---------------|--------------------------------------------------------------|
| `risk:low`    | docs, tests, comment-only edits, isolated typo fixes         |
| `risk:medium` | ordinary feature work, refactors, bug fixes (default)        |
| `risk:high`   | consensus / gate / prompt / policy logic, auth-style changes |

The hidden reminder in the PR template covers the same ground. No label →
the workflow falls back to `risk:medium`, which is fine for most changes —
but please set it explicitly so the choice is visible in the PR timeline.

### The AI review gate runs on your PR

`.github/workflows/ai-review.yml` will build a reviewer image from your
branch and run it against your own diff. A FAIL verdict is **not** an
automatic block on merging — a maintainer will read the report — but it's
the first signal we look at. If the gate fails:

- Read `review-report.md` from the workflow's "Upload report" artifact.
- If the finding is real, fix it and push again.
- If the finding is a false positive, say so in the PR thread with a brief
  explanation. The maintainer can override.
- If the gate itself misbehaved (crash, timeout, garbled JSON), open a
  separate issue and link it from the PR.

### Review and merge

- A maintainer will review within a few days. Please be patient — this is a
  side project.
- Squash-merge is the default. Keep the squashed message clean; the PR
  title becomes the commit subject.
- We don't require sign-off / DCO, but please make sure you have the right
  to contribute the code under the project's MIT license.

---

## Code style

- Rust 2021, formatted with `cargo fmt` (CI doesn't enforce it yet, but
  diffs are easier to read when it's clean).
- No new dependencies without a note in the PR description explaining what
  the dep buys us and what was considered as alternatives. The binary is
  intentionally lean.
- Public items added to `src/` modules should have a one-line `///` doc
  comment when the name alone doesn't make the contract obvious. Don't
  paraphrase the code; explain invariants and edge cases.
- Tests live next to the code (`#[cfg(test)] mod tests`) — no separate
  `tests/` integration directory unless you have a reason.

---

## License

By contributing, you agree that your contributions will be licensed under
the [MIT License](LICENSE) that covers the project.
