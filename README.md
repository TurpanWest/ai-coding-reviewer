# ai-reviewer

AI-to-AI code review gate for GitHub PRs. Two LLM models independently review every diff; both must pass with confidence ≥ 0.90 or the merge is blocked.

---

## Quick start (3 steps)

### 1. Add the workflow

Copy `.github/workflows/ai-review.yml` to your repo:

```yaml
name: AI Code Review

on:
  pull_request:
    branches: [main]

jobs:
  ai-review:
    name: AI Review Gate
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: read

    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Log in to GHCR
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Compute lowercase image ref
        run: |
          IMAGE=$(echo "ghcr.io/${{ github.repository }}:latest" | tr '[:upper:]' '[:lower:]')
          echo "IMAGE=${IMAGE}" >> "$GITHUB_ENV"

      - name: Pull reviewer image
        run: |
          docker pull "$IMAGE" || docker build -t "$IMAGE" .

      - name: Generate diff
        run: git diff origin/${{ github.base_ref }}...HEAD > pr.diff

      - name: Run AI review
        env:
          REVIEWER_1_API_KEY: ${{ secrets.REVIEWER_1_API_KEY }}
          REVIEWER_2_API_KEY: ${{ secrets.REVIEWER_2_API_KEY }}
          # Optional provider overrides — see "Changing providers" below.
          # Empty values fall back to the built-in defaults (minimax + deepseek).
          REVIEWER_1_MODEL:    ${{ vars.REVIEWER_1_MODEL }}
          REVIEWER_2_MODEL:    ${{ vars.REVIEWER_2_MODEL }}
          REVIEWER_1_BASE_URL: ${{ vars.REVIEWER_1_BASE_URL }}
          REVIEWER_2_BASE_URL: ${{ vars.REVIEWER_2_BASE_URL }}
        run: |
          docker run --rm \
            -v "${{ github.workspace }}:/repo" \
            -e REVIEWER_1_API_KEY \
            -e REVIEWER_2_API_KEY \
            -e REVIEWER_1_MODEL \
            -e REVIEWER_2_MODEL \
            -e REVIEWER_1_BASE_URL \
            -e REVIEWER_2_BASE_URL \
            "$IMAGE" \
            --diff /repo/pr.diff \
            --policy /repo/policy.md \
            --source-root /repo \
            --output /repo/review-report.md

      - name: Upload report
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: ai-review-report
          path: review-report.md
```

### 2. Add a policy file

Create `policy.md` in your repo root. This is injected verbatim into the LLM prompt:

```markdown
## Security
- SEC-001: Never log credentials, tokens, or PII
- SEC-002: All SQL queries must use parameterized statements

## Correctness
- LOGIC-001: Every error must be handled or explicitly propagated
```

See [`review-policy.example.md`](review-policy.example.md) for a full template.

### 3. Add secrets and enable branch protection

**Secrets** — repo Settings → Secrets and variables → Actions:

| Secret | Description |
|---|---|
| `REVIEWER_1_API_KEY` | API key for the first reviewer model |
| `REVIEWER_2_API_KEY` | API key for the second reviewer model |

**Branch protection** — Settings → Branches → Add rule:
- Branch: `main`
- Enable **Require status checks to pass before merging**
- Add status check: **`AI Review Gate`**

---

## Changing providers

Defaults are MiniMax (reviewer 1) + DeepSeek (reviewer 2). Override either slot via these env vars:

| Variable | Purpose |
|---|---|
| `REVIEWER_1_BASE_URL` | OpenAI-compat base URL for reviewer 1 |
| `REVIEWER_2_BASE_URL` | OpenAI-compat base URL for reviewer 2 |
| `REVIEWER_1_MODEL` | Model ID for reviewer 1 |
| `REVIEWER_2_MODEL` | Model ID for reviewer 2 |

**Where to set them:**

- **GitHub Actions** — repo Settings → Secrets and variables → Actions → **Variables** tab. Add `REVIEWER_1_MODEL` etc. as repo Variables. The Quick start workflow above already wires them through; leave them unset to fall back to the defaults.
- **Local CLI** — export them in your shell before `cargo run` (or pass `--reviewer-1-model <id>` on the command line).
- **Docker locally** — `docker run -e REVIEWER_1_MODEL=... -e REVIEWER_1_BASE_URL=... ...`.

Any provider with an OpenAI-compatible `/v1/chat/completions` endpoint works (MiniMax, DeepSeek, Anthropic via proxy, Gemini, OpenAI, local Ollama, etc.).

---

## How it works

```
PR diff
  └─ parse → AST context extraction (13 languages)
       └─ prompt assembly (system: policy + schema | user: diff + symbols)
            └─ Model A ──┐
                          ├─ consensus: both PASS + confidence ≥ 0.90?
            └─ Model B ──┘
                 └─ review-report.md  +  exit 0 (PASS) / 1 (FAIL) / 2 (error)
```

- Both models run **concurrently** and review the same diff independently.
- A finding is reported only when **both models agree** on the same file + line + rule.
- Transient errors (timeout, 5xx, 429) are **retried with exponential backoff**; auth errors (401, 403, 404) fail fast.
- AST context (full symbol definitions + call graph) is attached for 13 languages: Rust, Python, Go, JS/TS, Java, C, C++, Ruby, C#, Bash, Scala. Other languages are diff-only.

---

## Recommended PR size

Every one of the 8 concurrent LLM calls receives the full diff plus AST context for all changed files — per-call prompt size grows roughly linearly with the PR. Review quality degrades in long contexts (typically around 32k+ tokens) well before any hard window limit, so keeping PRs small is the cheapest way to keep reviews sharp.

| Tier | Files | Net diff | Expectation |
|---|---|---|---|
| Sweet spot | ≤ 10 | ≤ ~500 lines | Fast, low-noise reviews |
| Acceptable ceiling | ≤ 30 | ≤ ~1500 lines | Works, but slower and noisier |
| Split required | > 30 or > 1500 lines | | Partition before review |

**How to split**: prefer atomic commits organized by module or feature, then open one PR per logical unit. "One change, one PR" is the goal — not "one commit, one PR".

**Natural exceptions** — these are legitimately large and should be routed through a dedicated review process rather than this gate:
- Mass renames / signature changes that ripple across the codebase
- Dependency upgrades and their mechanical call-site adaptations
- Generated code: lock files, protobuf stubs, OpenAPI clients, migrations

---

## Local usage

```bash
# Requires REVIEWER_1_API_KEY and REVIEWER_2_API_KEY in env
cargo run -- --diff path/to/file.diff --policy policy.md --source-root .

# From stdin
git diff HEAD~1 | cargo run -- --diff - --policy policy.md

# Verbose
cargo run -- --diff - --policy policy.md -v
```

Exit codes: `0` = PASS, `1` = FAIL, `2` = fatal error.

---

## For AI tools

<!-- ai-reviewer:meta
This section is machine-readable context for AI coding assistants (Claude, Copilot, Cursor, etc.)
reading this repository. Human readers can skip it.
-->

```yaml
tool: ai-reviewer
type: CLI binary / Docker image
language: Rust
purpose: >
  Dual-LLM code review gate. Parses a unified diff, extracts AST context,
  sends to two LLM models in parallel, and produces PASS/FAIL + Markdown report.

interface:
  binary: ai-reviewer
  docker: ghcr.io/<owner>/<repo>:latest
  args:
    --diff:        path to unified diff file, or "-" to read from stdin (required)
    --policy:      path to Markdown policy file injected into the system prompt (required)
    --source-root: repo root for AST context extraction and tool-call file access (optional)
    --output:      path for the Markdown report (default: review-report.md)
    -v/--verbose:  enable verbose tracing output

env_vars:
  required:
    REVIEWER_1_API_KEY: API key for the first LLM reviewer
    REVIEWER_2_API_KEY: API key for the second LLM reviewer
  optional:
    REVIEWER_1_BASE_URL: OpenAI-compat base URL (default: provider-specific)
    REVIEWER_2_BASE_URL: OpenAI-compat base URL (default: provider-specific)
    REVIEWER_1_MODEL:    model ID override for reviewer 1
    REVIEWER_2_MODEL:    model ID override for reviewer 2
    RUST_LOG:            tracing filter, e.g. "ai_reviewer=debug"

exit_codes:
  0: consensus PASS — both models passed with confidence >= 0.90
  1: consensus FAIL — any model failed or confidence below threshold
  2: fatal error — bad diff path, missing policy file, diff too large, etc.

outputs:
  stdout: single summary line (PASS/FAIL + finding count)
  stderr: gate-fail message when exit code is 1
  file:   full Markdown cross-comparison report (default: review-report.md)

key_source_files:
  src/main.rs:          CLI entrypoint, provider wiring, concurrent orchestration
  src/diff.rs:          unified diff parser → FileDiff / HunkRange
  src/ast.rs:           tree-sitter AST extraction + call graph (13 languages)
  src/prompt.rs:        system prompt + user prompt assembly
  src/models/reviewer.rs: shared retry loop, JSON self-correction, strip helpers
  src/models/mod.rs:    ReviewResult / ConsensusResult types + JSON schema
  src/consensus.rs:     PASS/FAIL gate logic, finding dedup
  src/report.rs:        Markdown report renderer
  src/tools.rs:         LLM tool implementations (read_file, find_symbol)
  src/telemetry.rs:     Prometheus metrics export

consensus_rule: >
  PASS only when BOTH models return Verdict::Pass AND both confidence >= 0.90.
  Any ReviewError becomes a synthetic Verdict::Fail with confidence=1.0.
  Finding dedup key: (file, line_start, rule_id).

retry_policy: >
  Up to max_retries+1 attempts per LLM call.
  Retryable: timeout, 5xx, 429, network reset — exponential backoff 1s→2s→4s→8s→16s.
  Non-retryable: 401, 403, 404, invalid API key — fail immediately.
  JSON parse failure also retries with a self-correction prompt (self-contained, no history accumulation).

pr_size_guidance:
  rationale: >
    All 8 concurrent LLM calls receive the full diff plus AST context for every changed file.
    Per-call prompt size grows roughly linearly with the PR; review quality degrades in long
    contexts (≈ 32k+ tokens) well before the model's hard window limit.
  sweet_spot:        "<= 10 files, <= ~500 lines net diff"
  acceptable_ceiling: "<= 30 files, <= ~1500 lines net diff"
  split_required:    "> 30 files OR > 1500 lines — partition before review"
  split_strategy:    "atomic commits per module/feature; one logical change per PR"
  natural_exceptions: >
    Mass renames, signature ripples, dependency upgrades, and generated code (lock files,
    protobuf stubs, OpenAPI clients, DB migrations) are legitimately large — route through
    a dedicated review process rather than this gate.
```

---

## License

MIT
