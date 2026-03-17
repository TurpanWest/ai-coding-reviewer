# ai-reviewer — Architecture Blueprint

> **AI-to-AI Code Review Engine** · Production-grade, stateless Rust CLI
> Designed for pure-AI CI pipelines where correctness > developer ergonomics.

---

## 0. High-Level Pipeline

```
git diff (stdin / file)
        │
        ▼
┌─────────────────────────────────────────────────────────────────┐
│  1. DIFF PARSER  (diffy)                                        │
│     Parse unified diff → per-file hunks + line-range metadata  │
└─────────────────────────────┬───────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  2. AST EXTRACTOR  (tree-sitter)                                │
│     • Parse *full* source file (not just the diff chunk)       │
│     • Run Tree-sitter Query to collect all function_item,      │
│       impl_item, trait_item, struct_item nodes                 │
│     • Map changed lines → enclosing AST nodes                  │
│     • BFS/DFS over children to build lightweight Call Graph     │
│       (caller → callee symbol names)                           │
│     ► Emit: AstContext { changed_nodes, call_graph }           │
└─────────────────────────────┬───────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  3. PROMPT ASSEMBLER                                            │
│     • SYSTEM block (cached):                                   │
│         - Company security policy (injected once, cached)      │
│         - Global coding standards                              │
│     • USER block (dynamic, per-review):                        │
│         - Serialised AstContext JSON                           │
│         - Raw diff of changed functions                        │
│         - Structured review schema (JSON Schema)               │
└─────────────────────────────┬───────────────────────────────────┘
                              │
              ┌───────────────┴────────────────┐
              │   tokio::join! (true parallel)  │
              ▼                                ▼
  ┌───────────────────────┐      ┌───────────────────────┐
  │  MINIMAX REVIEWER     │      │  DEEPSEEK REVIEWER    │
  │  rig-core Anthropic   │      │  rig-core OpenAI      │
  │  provider +           │      │  provider +           │
  │  custom base_url      │      │  custom base_url      │
  │  .with_prompt_caching │      │  prefix cache param   │
  │  Self-Correction Loop │      │  Self-Correction Loop │
  │  max 3 retries        │      │  max 3 retries        │
  └──────────┬────────────┘      └────────────┬──────────┘
             │                                │
             └───────────────┬────────────────┘
                             ▼
┌─────────────────────────────────────────────────────────────────┐
│  5. CONSENSUS ENGINE                                            │
│     • Deserialise both ReviewResult structs                    │
│     • Check: both confidence >= CONFIDENCE_THRESHOLD (0.90)    │
│     • Check: verdict agreement (both PASS or both FAIL)        │
│     • Merge finding lists (deduplicate by location)            │
│     ► Emit: ConsensusResult { verdict, merged_findings }       │
└─────────────────────────────┬───────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  6. REPORTER + EXIT CODE                                        │
│     PASS  → stdout summary, exit code 0                        │
│     FAIL  → Markdown cross-comparison report, exit code 1      │
└─────────────────────────────────────────────────────────────────┘
```

---

## 1. Module Layout

```
src/
├── main.rs            — CLI entry point (clap), wires pipeline stages
├── diff.rs            — Unified diff parsing (diffy), HunkRange extraction
├── ast.rs             — tree-sitter AST extraction + Call Graph builder
├── prompt.rs          — Prompt assembly, cache block construction
├── models/
│   ├── mod.rs         — ReviewRequest / ReviewResult / ConsensusResult types
│   ├── minimax.rs     — MiniMax client (rig-core Anthropic-compat), retry loop
│   └── deepseek.rs    — DeepSeek client (rig-core OpenAI-compat), retry loop
├── consensus.rs       — Confidence gate, verdict merge, dedup
└── report.rs          — Markdown report generation
```

---

## 2. Model Provider Configuration

Both models are accessed via `rig-core`'s existing provider abstractions,
re-pointed to third-party base URLs. No extra HTTP code is required.

### 2.1 MiniMax — Anthropic-Compatible Endpoint

MiniMax exposes an API fully compatible with the Anthropic Messages API.
We use `rig-core`'s Anthropic provider and override `base_url`:

```rust
// src/models/minimax.rs
use rig::providers::anthropic;

let client = anthropic::ClientBuilder::new(
    &std::env::var("MINIMAX_API_KEY").expect("MINIMAX_API_KEY not set"),
)
.base_url("https://api.minimax.chat/v1")   // MiniMax Anthropic-compat endpoint
.build();

let model = client
    .completion_model("MiniMax-Text-01")
    .with_prompt_caching();                // cache_control: ephemeral on system block
```

> **Why Anthropic compat?** MiniMax's implementation mirrors the Anthropic
> `system` / `messages` schema including `cache_control` blocks, so
> `rig-core`'s `apply_cache_control()` works without modification.

### 2.2 DeepSeek — OpenAI-Compatible Endpoint

```rust
// src/models/deepseek.rs
use rig::providers::openai;

let client = openai::ClientBuilder::new(
    &std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY not set"),
)
.base_url("https://api.deepseek.com/v1")   // DeepSeek OpenAI-compat endpoint
.build();

let model = client.completion_model("deepseek-reasoner");
```

DeepSeek's context cache is activated by prefixing the system prompt as the
first message with a stable hash. We pass the cache hint via `additional_params`:

```rust
let extra = serde_json::json!({
    "cache_control": { "type": "prefix" }
});
builder.additional_params(extra)
```

---

## 3. AST Extraction — How It Works

### 3.1 Parsing with tree-sitter

```rust
// src/ast.rs
let mut parser = Parser::new();
parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
let tree = parser.parse(source_bytes, None).unwrap();
```

### 3.2 Tree-sitter Query for Rust symbols

```scheme
; Capture all top-level callable items
(function_item name: (identifier) @fn.name) @fn.def
(impl_item      type: (_)          @impl.type) @impl.def
```

For each matched `fn.def` node we record:
- `name`, `start_byte..end_byte`, `start_point..end_point`
- Full source text of that node's body

### 3.3 Mapping diff hunks → AST nodes

```
for each changed_line in hunk:
    find the deepest AST node whose byte range contains that line
    → this is the "impacted node"
```

### 3.4 Call Graph (lightweight)

Within each impacted function body:
- Query all `call_expression` nodes
- Extract callee identifier
- Build `HashMap<Symbol, Vec<Symbol>>` (caller → callees)

The LLM prompt includes: *"Function X (changed) calls Y and Z; their full definitions follow."*

---

## 4. Prompt Caching — Concrete Implementation

### Problem
The company security ruleset can be 50–100 KB of Markdown. Re-sending it
on every CI run wastes tokens and adds ~2 s latency per request.

### Solution

**MiniMax** — uses the Anthropic `cache_control: ephemeral` convention.
`rig-core`'s `.with_prompt_caching()` inserts the breakpoint automatically
at the end of the system block and the last user message.

```
┌─────────────────────────────────────────────────────┐
│ SYSTEM  (cached, ~50 KB)                            │
│   [Security Policy]                                 │
│   [Coding Standards]                                │
│   ← cache_control: ephemeral breakpoint HERE       │
├─────────────────────────────────────────────────────┤
│ USER  (dynamic, ~2–5 KB per review)                 │
│   [AstContext JSON]                                 │
│   [Diff of changed functions]                       │
│   [JSON Schema for response]                        │
└─────────────────────────────────────────────────────┘
```

**Cache lifetime**: 5 minutes server-side. Multiple CI jobs in the same
pipeline window share the cached system prompt with near-zero token cost.

**DeepSeek** — uses prefix caching: the stable system-prompt prefix is
hashed and cached on DeepSeek's side between requests with identical prefixes.

---

## 5. Serde Structs — The Contract Layer

All LLM output is forced into a strict JSON schema declared in the system
prompt. `serde` is the hard gate: deserialisation failure triggers the
self-correction loop.

```rust
// src/models/mod.rs

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Severity { Critical, High, Medium, Low, Info }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict { Pass, Fail }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CodeLocation {
    pub file:       String,
    pub line_start: u32,
    pub line_end:   u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Finding {
    pub severity:    Severity,
    pub location:    CodeLocation,
    pub rule_id:     String,       // e.g. "SEC-001", "LOGIC-042"
    pub description: String,
    pub suggestion:  String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReviewResult {
    pub model_id:   String,        // "minimax-text-01" | "deepseek-reasoner"
    pub verdict:    Verdict,
    pub confidence: f64,           // 0.0 – 1.0, must be >= THRESHOLD to pass gate
    pub findings:   Vec<Finding>,
    pub reasoning:  String,        // chain-of-thought summary (not gated)
}

#[derive(Debug, Serialize)]
pub struct ConsensusResult {
    pub verdict:          Verdict,
    pub minimax_result:   ReviewResult,
    pub deepseek_result:  ReviewResult,
    pub merged_findings:  Vec<Finding>,
    pub gate_passed:      bool,
}
```

---

## 6. Self-Correction Retry Loop

```
attempt 1:
  POST prompt → raw text
  │
  ├─ serde_json::from_str::<ReviewResult>() OK → return ✓
  │
  └─ Err(e):
       build correction prompt:
         "Your previous response failed JSON validation.
          Error: {e}
          You MUST return ONLY a raw JSON object — no markdown fences,
          no commentary. Schema: {SCHEMA_JSON}"
       → attempt 2

attempt 2:
  POST correction prompt → retry parse
  │
  ├─ OK → return ✓
  └─ Err(e) → attempt 3 (same pattern)

attempt 3:
  POST → retry parse
  ├─ OK → return ✓
  └─ Err(_) → return Err(ReviewError::MaxRetriesExceeded { model, raw_response })
                  ↓
           Consensus engine: automatic Fail verdict
           Report: includes raw response for debugging
```

---

## 7. Consensus Gate Logic

```rust
// src/consensus.rs

pub const CONFIDENCE_THRESHOLD: f64 = 0.90;

pub fn evaluate(
    minimax:  &ReviewResult,
    deepseek: &ReviewResult,
) -> ConsensusResult {
    let both_confident = minimax.confidence  >= CONFIDENCE_THRESHOLD
                      && deepseek.confidence >= CONFIDENCE_THRESHOLD;

    let verdicts_agree = matches!(
        (&minimax.verdict, &deepseek.verdict),
        (Verdict::Pass, Verdict::Pass) | (Verdict::Fail, Verdict::Fail)
    );

    let gate_passed = both_confident
        && verdicts_agree
        && matches!(minimax.verdict, Verdict::Pass);

    ConsensusResult {
        verdict: if gate_passed { Verdict::Pass } else { Verdict::Fail },
        merged_findings: merge_and_dedup(
            &minimax.findings,
            &deepseek.findings,
        ),
        gate_passed,
        minimax_result:  minimax.clone(),
        deepseek_result: deepseek.clone(),
    }
}
```

**Failure modes → always exit code 1:**

| Condition | Verdict |
|---|---|
| Either model < 0.90 confidence | Fail (uncertain) |
| Models disagree (Pass vs Fail) | Fail (conflict) |
| Either model max-retried | Fail (parse error) |
| Both Fail, both confident | Fail (confirmed defect) |

Only `Both Pass + Both ≥ 0.90` → exit code 0.

---

## 8. CLI Interface (clap)

```
USAGE:
  ai-reviewer [OPTIONS] --diff <PATH>

OPTIONS:
  -d, --diff <PATH>              Unified diff file path, or "-" for stdin
  -s, --source-root <PATH>       Repository root for full-file AST context [default: .]
  -p, --policy <PATH>            Security/coding policy Markdown file [required]
  -t, --threshold <FLOAT>        Confidence gate threshold [default: 0.90]
  -o, --output <PATH>            Report output path [default: ./review-report.md]
      --max-retries <N>          LLM self-correction retries per model [default: 3]
      --model-minimax <ID>       MiniMax model ID [default: MiniMax-Text-01]
      --model-deepseek <ID>      DeepSeek model ID [default: deepseek-reasoner]
  -v, --verbose                  Enable tracing output (RUST_LOG=info)

ENVIRONMENT:
  MINIMAX_API_KEY     MiniMax API key (Anthropic-compat endpoint)
  DEEPSEEK_API_KEY    DeepSeek API key (OpenAI-compat endpoint)
```

---

## 9. Key Design Decisions & Trade-offs

| Decision | Rationale |
|---|---|
| **MiniMax via Anthropic-compat** | Reuses `rig-core`'s Anthropic provider + native `cache_control` support with zero extra code |
| **DeepSeek via OpenAI-compat** | Same rationale; `rig-core` OpenAI provider accepts custom `base_url` |
| **No RAG / local vector DB** | Prompt caching delivers policy context at model-layer speed, eliminating an entire infra dependency |
| **Full-file AST parse** | Call graph construction requires whole-module visibility; diff-only context is logically blind |
| **Strict Serde, no `Value` fallback** | Forces well-typed output; parse failures become first-class retryable events, not silent data loss |
| **Exit code as CI gate** | Universal UNIX convention; zero integration overhead for any CI system |

---

## 10. Future Extension Points

- **Multi-language support** — Add `tree-sitter-python`, `tree-sitter-typescript` grammars; `ast.rs` is language-agnostic above the grammar layer.
- **Third reviewer (tie-breaker)** — If MiniMax and DeepSeek disagree, invoke a third model as arbiter before blocking the pipeline.
- **Persistent cache warming** — Pre-warm prompt cache as a dedicated CI job step before review runs.
- **Semantic finding dedup** — Use embedding similarity to merge near-duplicate findings across models rather than exact-match dedup.
