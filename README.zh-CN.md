# ai-coding-reviewer

面向 GitHub PR 的 AI-to-AI 代码评审门禁。两个 LLM 模型独立评审每一份 diff；必须都投 PASS，并且在评审意见出现分歧或报告了 MEDIUM 及以上级别的发现时，由置信度阈值（安全/正确性 0.90，性能/可维护性 0.80）来把守合并。投票规则本身可按变更声明的 `low | medium | high` 风险等级来选择 —— 详见[风险等级](#风险等级)。

> English version: [README.md](README.md)

---

## 快速开始（3 步）

### 1. 添加 workflow

把 `.github/workflows/ai-review.yml` 复制到你的仓库：

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

    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Set image tag
        run: |
          IMAGE=$(echo "ai-reviewer-pr:${{ github.event.pull_request.head.sha || github.sha }}" | tr '[:upper:]' '[:lower:]')
          echo "IMAGE=${IMAGE}" >> "$GITHUB_ENV"

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3

      - name: Build reviewer image from PR source
        uses: docker/build-push-action@v6
        with:
          context: .
          load: true
          push: false
          tags: ${{ env.IMAGE }}
          cache-from: type=gha
          cache-to: type=gha,mode=max

      - name: Generate diff
        run: git diff origin/${{ github.base_ref }}...HEAD > pr.diff

      - name: Run AI review
        env:
          REVIEWER_1_API_KEY: ${{ secrets.REVIEWER_1_API_KEY }}
          REVIEWER_2_API_KEY: ${{ secrets.REVIEWER_2_API_KEY }}
          # 可选的 provider 覆盖 —— 见下文"切换 provider"。
          # 留空则回退到内置默认值（minimax + deepseek）。
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

### 2. 添加策略文件

在仓库根目录创建 `policy.md`。它会被原样注入到 LLM 的 prompt 中：

```markdown
## Security
- SEC-001: Never log credentials, tokens, or PII
- SEC-002: All SQL queries must use parameterized statements

## Correctness
- LOGIC-001: Every error must be handled or explicitly propagated
```

完整模板见 [`review-policy.example.md`](review-policy.example.md)。

### 3. 添加 secrets、variables，并启用分支保护

两者都在仓库 **Settings → Secrets and variables → Actions** 下，但分别在不同的 tab 中。

**Secrets**（必填）—— *Secrets* tab：

| Secret | 说明 |
|---|---|
| `REVIEWER_1_API_KEY` | 第一个评审模型的 API key |
| `REVIEWER_2_API_KEY` | 第二个评审模型的 API key |

**Variables**（可选）—— *Variables* tab。不填则使用内置默认值（MiniMax + DeepSeek）：

| Variable | 说明 | 示例 |
|---|---|---|
| `REVIEWER_1_MODEL`    | reviewer 1 的模型 ID                   | `MiniMax-M2.7` |
| `REVIEWER_1_BASE_URL` | reviewer 1 的 OpenAI 兼容 endpoint      | `https://api.minimax.chat/v1` |
| `REVIEWER_2_MODEL`    | reviewer 2 的模型 ID                   | `deepseek-chat` |
| `REVIEWER_2_BASE_URL` | reviewer 2 的 OpenAI 兼容 endpoint      | `https://api.deepseek.com/v1` |

> 不要把 model ID / base URL 放进 *Secrets* tab —— Secrets 在 UI 中只能写、不能读，而这些值本身并不敏感。Variables 可以编辑、可以查看。

**分支保护** —— Settings → Branches → Add rule：
- 分支：`main`
- 启用 **Require status checks to pass before merging**
- 添加状态检查：**`AI Review Gate`**

---

## 切换 provider

默认是 MiniMax（reviewer 1）+ DeepSeek（reviewer 2）。通过下列环境变量覆盖任意一侧：

| 变量 | 用途 |
|---|---|
| `REVIEWER_1_BASE_URL` | reviewer 1 的 OpenAI 兼容 base URL |
| `REVIEWER_2_BASE_URL` | reviewer 2 的 OpenAI 兼容 base URL |
| `REVIEWER_1_MODEL` | reviewer 1 的模型 ID |
| `REVIEWER_2_MODEL` | reviewer 2 的模型 ID |

**设置位置：**

- **GitHub Actions** —— 仓库 Settings → Secrets and variables → Actions → **Variables** tab。把 `REVIEWER_1_MODEL` 等作为仓库 Variables 添加。上面的 Quick start workflow 已经把它们接好；留空则回退到默认值。
- **本地 CLI** —— 在 shell 里 `export`，再运行 `cargo run`（或在命令行通过 `--reviewer-1-model <id>` 传入）。
- **本地 Docker** —— `docker run -e REVIEWER_1_MODEL=... -e REVIEWER_1_BASE_URL=... ...`。

任何提供 OpenAI 兼容 `/v1/chat/completions` 接口的 provider 都能用（MiniMax、DeepSeek、Anthropic via proxy、Gemini、OpenAI、本地 Ollama 等）。

---

## 工作原理

```
PR diff
  └─ 解析 → AST 上下文抽取（13 种语言）
       └─ prompt 装配（system：policy + schema | user：diff + 符号）
            └─ 模型 A ──┐
                          ├─ 共识：两侧都 PASS + （没有 ≥MEDIUM 发现 或 各 focus 置信度过线）？
            └─ 模型 B ──┘
                 └─ review-report.md  +  exit 0（PASS）/ 1（FAIL）/ 2（错误）
```

- 两个模型**并发**运行，独立评审同一份 diff。
- 只有当**两个模型对同一 file + line + rule 达成一致**时，才会作为正式发现报告。
- 任意一方投 FAIL，门禁立刻 fail。
- 双方都投 PASS 时，**各 focus 的置信度阈值** —— 安全/正确性 0.90、性能/可维护性 0.80 —— 作为决胜规则被强制要求；但仅在合并后的发现里出现 MEDIUM 及以上严重程度时才生效。如果只有 LOW/INFO 级别的发现且整体 PASS，则无视置信度直接放行 —— "对风格的不确定判断"不该被卡得和 RCE 一样死。
- 上述投票规则是 **`medium` 风险等级**，即默认。门禁同时支持按变更声明的 `low` 与 `high` 等级。详见下方[风险等级](#风险等级)。
- 瞬时错误（timeout、5xx、429）会**指数退避重试**；鉴权错误（401、403、404）立即失败。
- AST 上下文（完整符号定义 + 同文件内的 callee 名称提示）覆盖 13 种语言：Rust、Python、Go、JS/TS、Java、C、C++、Ruby、C#、Bash、Scala。其他语言只送 diff。"callee 名称提示"具体是什么意思，详见[局限与路线图](#局限与路线图) —— 它**不是**一份解析过的跨文件 call graph。

---

## 风险等级

文档错别字和密码学模块的重写不应该共用同一条评审基线。开发者（或做这次变更的 AI agent）**为这次变更声明一个风险等级**，门禁据此选择投票规则。各 focus 的置信度阈值在所有等级下保持不变。

| 风险 | 通过条件 | 适用场景 |
|---|---|---|
| `low`    | **任一**评审者投 PASS 即可 —— 忽略置信度 | 文档、测试、纯注释改动、孤立的错别字修复 |
| `medium` | 都 PASS，并且在两侧分歧或出现 `MEDIUM+` 发现时强制各 focus 置信度阈值（历史默认） | 普通功能开发、重构、bug 修复 |
| `high`   | 都 PASS **且** 都无条件地通过各 focus 的置信度阈值 —— 即使零发现 | 鉴权、密码学、支付、schema 迁移、公开 API 契约 |

**这个声明被原样信任。** 评审器不会去质疑一个 `low` 声明，哪怕 diff 触到了敏感代码；明显滥用应由带外的评审者（人类或另一个工具）来管。声明等级会被显著地渲染在 `review-report.md` 中，也会打印到 stdout，方便发现滥用。

### 在 PR 上声明风险

最简单的方式是用 PR label 或 body 行。CLI 读取 `--risk-level`（或环境变量 `REVIEWER_RISK_LEVEL`）；CI 步骤在调用二进制前先把值提取出来：

```yaml
- name: Resolve risk level from PR label
  id: risk
  env:
    LABELS: ${{ toJSON(github.event.pull_request.labels.*.name) }}
  run: |
    level=$(echo "$LABELS" | jq -r '.[]' | grep -E '^risk:(low|medium|high)$' | head -n1 | cut -d: -f2)
    echo "level=${level:-medium}" >> "$GITHUB_OUTPUT"

- name: Run AI Review
  env:
    REVIEWER_1_API_KEY: ${{ secrets.REVIEWER_1_API_KEY }}
    REVIEWER_2_API_KEY: ${{ secrets.REVIEWER_2_API_KEY }}
    REVIEWER_RISK_LEVEL: ${{ steps.risk.outputs.level }}
  run: |
    docker run --rm -i ${{ env.IMAGE }} --diff - --policy policy.md < pr.diff
```

打上 PR label `risk:low`、`risk:medium` 或 `risk:high`；没有 label 则默认 `medium`（与历史门禁行为完全一致，零差异）。

本地：

```bash
git diff HEAD~1 | cargo run -- --diff - --policy policy.md --risk-level high
```

---

## 推荐的 PR 大小

8 个并发的 LLM 调用每一个都会收到完整的 diff 加上所有变更文件的 AST 上下文 —— 单次调用的 prompt 大小大致随 PR 线性增长。长上下文（一般在 32k+ tokens 左右）下评审质量会在远早于硬窗口上限之前就开始下降，所以把 PR 切小是保持评审锐度最便宜的办法。

| 区间 | 文件数 | 净 diff | 预期 |
|---|---|---|---|
| 甜区 | ≤ 10 | ≤ ~500 行 | 快速、噪声低 |
| 可接受上限 | ≤ 30 | ≤ ~1500 行 | 能用，但更慢、更吵 |
| 必须拆 | > 30 文件 或 > 1500 行 | | 评审前先拆分 |

**怎么拆**：优先按模块或 feature 做原子提交，再按逻辑单元各开一个 PR。目标是"一次变更，一个 PR" —— 而不是"一个 commit，一个 PR"。

**天然例外** —— 下面这些场景天生就大，应该走专门的评审流程而不是这个门禁：
- 跨整个代码库的批量重命名 / 签名变更
- 依赖升级以及其机械式的调用点适配
- 生成代码：lock 文件、protobuf stub、OpenAPI client、迁移脚本

---

## 本地用法

```bash
# 需要在 env 里有 REVIEWER_1_API_KEY 和 REVIEWER_2_API_KEY
cargo run -- --diff path/to/file.diff --policy policy.md --source-root .

# 从 stdin 读
git diff HEAD~1 | cargo run -- --diff - --policy policy.md

# 详细日志
cargo run -- --diff - --policy policy.md -v
```

退出码：`0` = PASS，`1` = FAIL，`2` = 致命错误。

---

## 局限与路线图

### 今天的 "AST 上下文" 实际上包含什么

为了让任何接入这套工具的人都对它有正确的预期：送给每个模型的 AST 上下文包含**完整的符号定义**（变更涉及的 function/type）加上**单文件内的 callee 名称提示** —— 不是一份解析过的 call graph。

具体来说，对每个变更的符号，工具会输出一组光秃秃的 identifier 字符串，它们是该符号体内调用点上出现过的名字，作用域限定在同一个文件：

- ✅ 变更涉及的每个 function / class / struct / impl / trait 的完整源代码
- ✅ 变更符号内调用点出现的 callee 名（如 `verify_token`、`push`、`new`）
- ✅ 如果某个 callee 名恰好匹配**同一文件**中另一个符号，会内联那段定义
- ❌ 没有符号解析 —— `auth::verify_token`、`self.verify_token` 和本地的 `verify_token` 都坍缩成字符串 `"verify_token"`
- ❌ 没有类型推导 —— `vec.push()` 和 `string.push()` 都产生 `… → "push"` 这条边
- ❌ 没有跨文件边 —— 其他文件中的 caller 永远不会被链接，定义在其他文件中的 callee 只会作为光秃名字出现、没有函数体
- ❌ 没有反向边 —— 工具永远不回答"谁在调用这个变更过的函数？"

所以 callee 数据应该被读作 *"这些名字曾在变更代码内部被引用过"*，而不是 *"这些具体的函数被调用了"*。这个内部类型出于历史原因仍然叫 `CallEdge`，但 prompt 中现在把这一节标注为 "Callee Name Hints"，免得评审模型对它过度信任。

### 路线图

一份真正的跨文件 call graph（解析后的全限定名、类型感知的方法分派、反向边、blast-radius 查询）已经在计划中。可行的路线是把解析委托给一个真正的 indexer，而不是在 tree-sitter 之上重新实现一份 —— 正在评估的候选：

- 评审时消费的 **rust-analyzer / SCIP / LSIF** 索引
- **stack-graphs** 做多语言的名字解析
- 针对一个常驻 language server 的 **LSP** "find references"

如果你对哪条路线有强烈的看法，欢迎开 issue。

---

## License

MIT
