# ai-reviewer

> **Work in Progress** — under active development.

AI-to-AI code review gate for GitHub PRs. Two LLM models independently review every diff across Security, Correctness, Performance, and Maintainability. All four checks must pass with high confidence, or the merge is blocked.

---

## Add to your repo in 3 steps

### 1. Create the workflow file

Add `.github/workflows/ai-review.yml` to your repo:

```yaml
name: AI Code Review

on:
  pull_request:
    branches: [main]

jobs:
  ai-review:
    name: AI Review Gate
    runs-on: ubuntu-latest

    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2

      - name: Build ai-reviewer
        run: |
          git clone https://github.com/TurpanWest/ai-coding-reviewer.git /tmp/ai-reviewer
          cd /tmp/ai-reviewer && cargo build --release
          cp /tmp/ai-reviewer/target/release/ai-reviewer /usr/local/bin/ai-reviewer

      - name: Generate diff
        run: git diff origin/${{ github.base_ref }}...HEAD > /tmp/pr.diff

      - name: Run AI review
        env:
          MINIMAX_API_KEY: ${{ secrets.MINIMAX_API_KEY }}
          DEEPSEEK_API_KEY: ${{ secrets.DEEPSEEK_API_KEY }}
        run: |
          ai-reviewer \
            --diff /tmp/pr.diff \
            --policy .github/review-policy.md \
            --output review-report.md

      - name: Print report
        if: always()
        run: cat review-report.md

      - name: Upload report
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: ai-review-report
          path: review-report.md
```

### 2. Add a policy file

Create `.github/review-policy.md` — this is your ruleset, injected into the LLM prompt:

```markdown
## Security
- SEC-001: Never log credentials, tokens, or PII
- SEC-002: All SQL queries must use parameterized statements

## Correctness
- LOGIC-001: Every error must be handled or explicitly propagated
- LOGIC-002: Async tasks must not hold locks across await points
```

See [`review-policy.example.md`](review-policy.example.md) for a fuller starting point.

### 3. Add API keys and enable branch protection

**Add secrets** — go to your repo → Settings → Secrets and variables → Actions:

| Secret | Where to get it |
|---|---|
| `MINIMAX_API_KEY` | [minimax.chat](https://www.minimax.chat) |
| `DEEPSEEK_API_KEY` | [platform.deepseek.com](https://platform.deepseek.com) |

**Enable branch protection** — Settings → Branches → Add classic branch protection rule:
- Branch name pattern: `main`
- Check **Require status checks to pass before merging**
- Search for and add: **`AI Review Gate`**

That's it. Any PR targeting `main` will now be blocked until both models pass.

---

## Using different providers

The default is MiniMax + DeepSeek. You can swap either slot to any supported provider:

| Provider | Secret name | `--reviewer-N` value | Default model |
|---|---|---|---|
| MiniMax | `MINIMAX_API_KEY` | `minimax` | `MiniMax-M2.7` |
| DeepSeek | `DEEPSEEK_API_KEY` | `deepseek` | `deepseek-chat` |
| Anthropic | `ANTHROPIC_API_KEY` | `anthropic` | `claude-sonnet-4-6` |
| Google Gemini | `GEMINI_API_KEY` | `gemini` | `gemini-3.1-pro-preview` |
| OpenAI | `OPENAI_API_KEY` | `openai` | `gpt-5.4` |

Example — swap reviewer 2 to Anthropic:

```yaml
      - name: Run AI review
        env:
          MINIMAX_API_KEY: ${{ secrets.MINIMAX_API_KEY }}
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
        run: |
          ai-reviewer \
            --diff /tmp/pr.diff \
            --policy .github/review-policy.md \
            --reviewer-2 anthropic \
            --reviewer-2-model claude-opus-4-6
```

---

## How it works

Each PR diff is distributed across up to 4 focus groups (one per changed file, round-robin). Every group runs two models in parallel:

```
PR diff → 4 focus groups (Security · Correctness · Performance · Maintainability)
              each group: Model A + Model B run concurrently
                              ↓
                     both must return PASS with confidence ≥ 0.90
                              ↓
                     review-report.md  +  exit 0 / 1
```

- Exit `0` — all groups passed → merge allowed
- Exit `1` — any group failed → merge blocked
- Exit `2` — fatal error (missing keys, diff too large, etc.)

Supports 13 languages with AST context (Rust, Python, Go, JS/TS, Java, C, C++, Ruby, C#, Bash, Scala). Other languages are reviewed diff-only.

---

## License

MIT
