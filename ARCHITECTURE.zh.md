# ai-reviewer — 架构蓝图

> **AI 交叉代码审查引擎** · 生产级无状态 Rust CLI
> 专为"纯 AI 生成代码"的 CI/CD 流水线设计，核心目标：绝对的系统安全性与逻辑严谨性。

---

## 0. 整体流水线

```
git diff（标准输入 / 文件）
        │
        ▼
┌─────────────────────────────────────────────────────────────────┐
│  1. DIFF 解析器  (diffy)                                        │
│     解析 unified diff → 每文件变更块（hunk）+ 行范围元数据      │
└─────────────────────────────┬───────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  2. AST 提取器  (tree-sitter)                                   │
│     • 解析完整源文件（而非仅 diff 片段）                        │
│     • 运行 Tree-sitter Query 提取所有                           │
│       function_item / impl_item / trait_item / struct_item 节点 │
│     • 将变更行映射到对应的 AST 节点                             │
│     • BFS/DFS 遍历子节点，构建轻量调用图                        │
│       （caller → callee 符号名）                                │
│     ► 输出：AstContext { changed_nodes, call_graph }            │
└─────────────────────────────┬───────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  3. Prompt 组装器                                               │
│     • SYSTEM 块（缓存）：                                       │
│         - 公司安全规范（注入一次后缓存命中）                    │
│         - 全局编码标准                                          │
│     • USER 块（动态，每次审查不同）：                           │
│         - 序列化后的 AstContext JSON                            │
│         - 变更函数的原始 diff                                   │
│         - 结构化响应的 JSON Schema                              │
└─────────────────────────────┬───────────────────────────────────┘
                              │
              ┌───────────────┴────────────────┐
              │   tokio::join!（真正的并发）    │
              ▼                                ▼
  ┌───────────────────────┐      ┌───────────────────────┐
  │  MiniMax 审查器       │      │  DeepSeek 审查器      │
  │  rig-core Anthropic   │      │  rig-core OpenAI      │
  │  兼容层 +             │      │  兼容层 +             │
  │  自定义 base_url      │      │  自定义 base_url      │
  │  .with_prompt_caching │      │  前缀缓存参数         │
  │  自愈重试环（≤3次）   │      │  自愈重试环（≤3次）   │
  └──────────┬────────────┘      └────────────┬──────────┘
             │                                │
             └───────────────┬────────────────┘
                             ▼
┌─────────────────────────────────────────────────────────────────┐
│  5. 共识引擎                                                    │
│     • 反序列化两个 ReviewResult 结构体                         │
│     • 检查：两个模型置信度均 >= CONFIDENCE_THRESHOLD (0.90)    │
│     • 检查：两个模型裁决一致（同为 PASS 或同为 FAIL）          │
│     • 合并发现列表（按代码位置去重）                           │
│     ► 输出：ConsensusResult { verdict, merged_findings }       │
└─────────────────────────────┬───────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  6. 报告生成器 + 退出码                                         │
│     通过  → 标准输出摘要，退出码 0                              │
│     拦截  → Markdown 交叉对比报告，退出码 1                     │
└─────────────────────────────────────────────────────────────────┘
```

---

## 1. 模块布局

```
src/
├── main.rs            — CLI 入口（clap），串联各流水线阶段
├── diff.rs            — unified diff 解析（diffy），提取 HunkRange
├── ast.rs             — tree-sitter AST 提取 + 调用图构建
├── prompt.rs          — Prompt 组装，缓存块构造
├── models/
│   ├── mod.rs         — ReviewRequest / ReviewResult / ConsensusResult 类型定义
│   ├── minimax.rs     — MiniMax 客户端（rig-core Anthropic 兼容），重试环
│   └── deepseek.rs    — DeepSeek 客户端（rig-core OpenAI 兼容），重试环
├── consensus.rs       — 置信度门控、裁决合并、去重
└── report.rs          — Markdown 报告生成
```

---

## 2. 模型接入方案

两个模型均通过 `rig-core` 现有的 Provider 抽象接入，仅替换 `base_url`，
无需额外 HTTP 代码。

### 2.1 MiniMax — Anthropic API 兼容

MiniMax 暴露了与 Anthropic Messages API 完全兼容的接口。
我们使用 `rig-core` 的 Anthropic Provider 并覆盖 `base_url`：

```rust
// src/models/minimax.rs
use rig::providers::anthropic;

let client = anthropic::ClientBuilder::new(
    &std::env::var("MINIMAX_API_KEY").expect("MINIMAX_API_KEY 未设置"),
)
.base_url("https://api.minimax.chat/v1")   // MiniMax Anthropic 兼容端点
.build();

let model = client
    .completion_model("MiniMax-Text-01")
    .with_prompt_caching();                // 在 system 块末尾注入 cache_control: ephemeral
```

> **为何选择 Anthropic 兼容？** MiniMax 的实现完整镜像了 Anthropic 的
> `system` / `messages` 结构，包括 `cache_control` 字段，因此
> `rig-core` 的 `apply_cache_control()` 无需任何修改即可生效。

### 2.2 DeepSeek — OpenAI API 兼容

```rust
// src/models/deepseek.rs
use rig::providers::openai;

let client = openai::ClientBuilder::new(
    &std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY 未设置"),
)
.base_url("https://api.deepseek.com/v1")   // DeepSeek OpenAI 兼容端点
.build();

let model = client.completion_model("deepseek-reasoner");
```

DeepSeek 的上下文缓存通过前缀匹配激活——只要系统提示保持稳定，
DeepSeek 服务端即可自动命中缓存。通过 `additional_params` 传递缓存提示：

```rust
let extra = serde_json::json!({
    "cache_control": { "type": "prefix" }
});
builder.additional_params(extra)
```

---

## 3. AST 提取原理

### 3.1 使用 tree-sitter 解析

```rust
// src/ast.rs
let mut parser = Parser::new();
parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
let tree = parser.parse(source_bytes, None).unwrap();
```

### 3.2 Rust 符号的 Tree-sitter Query

```scheme
; 捕获所有顶层可调用项
(function_item name: (identifier) @fn.name) @fn.def
(impl_item      type: (_)          @impl.type) @impl.def
```

对每个匹配的 `fn.def` 节点记录：
- `name`、`start_byte..end_byte`、`start_point..end_point`
- 该节点函数体的完整源码文本

### 3.3 diff 块 → AST 节点映射

```
对每个 hunk 中的变更行：
    找到字节范围包含该行的最深 AST 节点
    → 此节点为"受影响节点"
```

### 3.4 调用图构建（轻量级）

在每个受影响函数体内：
- 查询所有 `call_expression` 节点
- 提取被调用方标识符
- 构建 `HashMap<Symbol, Vec<Symbol>>`（调用方 → 被调用方）

发给 LLM 的 Prompt 包含：*"函数 X（已变更）调用了 Y 和 Z，以下是它们的完整定义。"*

---

## 4. Prompt Caching 实现方案

### 问题背景
公司安全规范可达 50–100 KB 的 Markdown。每次 CI 运行都重新发送，
既浪费 Token 费用，又增加约 2 秒的请求延迟。

### 解决方案

**MiniMax**（Anthropic 兼容）— 使用 `cache_control: ephemeral` 约定。
`rig-core` 的 `.with_prompt_caching()` 自动在 system 块末尾和最后一条
user 消息处注入缓存断点。

```
┌─────────────────────────────────────────────────────┐
│ SYSTEM  （缓存，约 50 KB）                           │
│   [安全规范]                                         │
│   [编码标准]                                         │
│   ← cache_control: ephemeral 断点插入此处           │
├─────────────────────────────────────────────────────┤
│ USER  （动态，每次审查约 2–5 KB）                    │
│   [AstContext JSON]                                  │
│   [变更函数 diff]                                    │
│   [JSON Schema 响应约束]                             │
└─────────────────────────────────────────────────────┘
```

**缓存有效期**：服务端 5 分钟。同一流水线窗口内多次调用共享缓存，
System Prompt 的 Token 费用接近于零。

**DeepSeek**（OpenAI 兼容）— 使用前缀缓存：系统提示内容稳定时，
DeepSeek 服务端自动对相同前缀命中缓存，无需客户端额外操作。

---

## 5. Serde 结构体 — 类型合约层

所有 LLM 输出均被强制反序列化为严格定义的 Rust Struct。
`serde` 是硬性门控：反序列化失败即触发自愈重试环，
**严禁** 使用 `serde_json::Value` 作为逃生舱。

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
    pub rule_id:     String,       // 例：「SEC-001」「LOGIC-042」
    pub description: String,
    pub suggestion:  String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReviewResult {
    pub model_id:   String,        // "minimax-text-01" | "deepseek-reasoner"
    pub verdict:    Verdict,
    pub confidence: f64,           // 0.0 – 1.0，必须 >= 阈值才能通过门控
    pub findings:   Vec<Finding>,
    pub reasoning:  String,        // 思维链摘要（不参与门控判断）
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

## 6. 自愈重试环（Self-Correction Loop）

```
第 1 次：
  POST Prompt → 原始文本
  │
  ├─ serde_json::from_str::<ReviewResult>() 成功 → 返回 ✓
  │
  └─ Err(e)：
       构造纠错 Prompt：
         「你上一次的响应未通过 JSON 校验。
          错误信息：{e}
          请只输出符合以下 Schema 的纯 JSON 对象，
          禁止添加 Markdown 代码块或任何说明文字。
          Schema：{SCHEMA_JSON}」
       → 第 2 次

第 2 次：
  POST 纠错 Prompt → 重试解析
  ├─ 成功 → 返回 ✓
  └─ Err(e) → 第 3 次（同上）

第 3 次（最终尝试）：
  POST → 重试解析
  ├─ 成功 → 返回 ✓
  └─ Err(_) → 返回 Err(ReviewError::MaxRetriesExceeded { model, raw_response })
                    ↓
             共识引擎：自动判定为 Fail
             报告：包含原始响应内容以供调试
```

---

## 7. 共识门控逻辑

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

**失败场景一览（均返回退出码 1）：**

| 触发条件 | 裁决 |
|---|---|
| 任一模型置信度 < 0.90 | Fail（不确定） |
| 两个模型裁决不一致 | Fail（冲突） |
| 任一模型触发最大重试 | Fail（解析错误） |
| 两模型均自信地判定 Fail | Fail（确认缺陷） |

只有**两模型均 Pass 且均 ≥ 0.90** → 退出码 0，CI 放行。

---

## 8. CLI 接口（clap）

```
用法：
  ai-reviewer [选项] --diff <路径>

选项：
  -d, --diff <路径>              unified diff 文件路径，或 "-" 表示从标准输入读取
  -s, --source-root <路径>       仓库根目录，用于完整 AST 上下文解析 [默认值：.]
  -p, --policy <路径>            安全/编码规范 Markdown 文件路径 [必填]
  -t, --threshold <浮点数>       置信度门控阈值 [默认值：0.90]
  -o, --output <路径>            报告输出路径 [默认值：./review-report.md]
      --max-retries <N>          每个模型的最大自愈重试次数 [默认值：3]
      --model-minimax <ID>       MiniMax 模型 ID [默认值：MiniMax-Text-01]
      --model-deepseek <ID>      DeepSeek 模型 ID [默认值：deepseek-reasoner]
  -v, --verbose                  启用 tracing 输出（等同 RUST_LOG=info）

环境变量（存放密钥）：
  MINIMAX_API_KEY     MiniMax API 密钥（Anthropic 兼容端点）
  DEEPSEEK_API_KEY    DeepSeek API 密钥（OpenAI 兼容端点）
```

---

## 9. 关键设计决策与权衡

| 决策 | 理由 |
|---|---|
| **MiniMax 走 Anthropic 兼容层** | 复用 `rig-core` Anthropic Provider 及其原生 `cache_control` 支持，零额外代码 |
| **DeepSeek 走 OpenAI 兼容层** | 同上；`rig-core` OpenAI Provider 支持自定义 `base_url` |
| **不引入 RAG / 本地向量库** | Prompt Caching 在模型层实现策略文档检索，彻底消除一个基础设施依赖 |
| **解析完整文件的 AST** | 调用图构建需要整个模块的可见性；仅 diff 的上下文在逻辑上是盲区 |
| **严格 Serde，禁用 `Value` 逃生舱** | 强制 LLM 输出类型安全的数据；解析失败成为一等公民的可重试事件，而非静默的数据丢失 |
| **退出码作为 CI 门控** | 通用的 UNIX 约定；与任何 CI 系统（GitHub Actions、GitLab CI、Jenkins）零集成开销 |

---

## 10. 未来扩展点

- **多语言支持** — 添加 `tree-sitter-python`、`tree-sitter-typescript` 语法包；`ast.rs` 在语法包层之上是语言无关的。
- **第三审查方（裁决者）** — 若 MiniMax 与 DeepSeek 裁决冲突，可选择调用第三个模型作为仲裁者，避免直接阻塞流水线。
- **持久化缓存预热** — 将 Prompt Cache 预热作为独立 CI Job，在审查运行之前执行。
- **语义级 Finding 去重** — 使用嵌入相似度合并两个模型的近似重复发现，替代当前的精确匹配去重。
