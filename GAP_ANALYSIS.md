# 生产级别差距分析

> 目标标准：大厂生产级高可用 / 高可靠 / 高性能 CLI + CI/CD 工具
> 当前状态：功能完整的 Rust CLI，~4700 行，72 个单元测试，5 种 LLM provider，工具调用增强审查
> 更新日期：2026-04-10

---

## 总体评分

| 维度 | 当前水平 | 差距 |
|---|---|---|
| 测试与质量保障 | ★★★☆☆ | 中 |
| 可靠性与容错 | ★★★☆☆ | 中 |
| 性能优化 | ★★★☆☆ | 中 |
| 可观测性 | ★★★★☆ | 小 |
| 安全性 | ★★★☆☆ | 中 |
| 工程规范 | ★★☆☆☆ | 大 |
| 运维与部署 | ★★★☆☆ | 中 |
| 代码架构 | ★★★★☆ | 小 |

---

## 一、测试与质量保障

### 1.1 核心模块测试已补全 ✅ 已完成

**现状（已改善）**：51 个单元测试，覆盖所有核心模块。

| 模块 | 状态 | 覆盖内容 |
|---|---|---|
| `consensus.rs` | ✅ 12 个测试 | PASS/FAIL 边界、confidence 阈值、finding 去重、gate_failure_reason |
| `prompt.rs` | ✅ 11 个测试 | ignore 注解解析、user prompt 构造、correction prompt、ReviewFocus |
| `report.rs` | ✅ 7 个测试 | Markdown 渲染、badge、finding 表格、PASS/FAIL 输出 |
| `models/reviewer.rs` | ✅ 22 个测试 | strip_think_block（正常/截断/无）、strip_json_fences（各格式）、is_retryable_api_error（401/403/404/key/5xx/429/timeout/network）、is_retryable_review_error |
| `tools.rs` | ✅ 12 个测试 | safe_join 路径穿越、read_file 全文/分页/截断/遍历、find_symbol |
| `diff.rs` | ✅ 有测试 | diff 解析 happy path |
| `ast.rs` | ✅ 有测试 | AST 提取 |

**仍缺失**：
- `main.rs` 零测试（CLI 参数解析、provider 初始化逻辑）
- `telemetry.rs` 零测试（Prometheus metrics 注册/编码逻辑）
- `diff.rs` 边界测试不足：二进制文件、空 diff、仅删除文件、rename

**任务**：
- [ ] 补充 `diff.rs` 边界测试：二进制文件、空 diff、仅删除、rename
- [ ] 为 `telemetry.rs` 的 `record_review` 函数添加单元测试

### 1.2 缺少集成测试 🔴 高优先级

**现状**：`mock_review_e2e.rs` 是手动运行的 binary，不是 `cargo test` 的一部分。  
**缺失**：真正的 `#[tokio::test]` 集成测试，用 `wiremock` 或 `httpmock` mock LLM 端点，验证完整 review 流程。

**任务**：
- [ ] 引入 `wiremock` 或 `httpmock` 依赖
- [ ] 将 `mock_review_e2e.rs` 改写为 `tests/integration_test.rs`，使其可 `cargo test` 运行
- [ ] 覆盖场景：API 返回无效 JSON → 触发 retry；API 超时 → 返回 FAIL；两模型分歧 → 返回 FAIL

### 1.3 缺少 Fuzz 测试 🟡 中优先级

**现状**：diff parser 和 AST extractor 处理外部输入，但无 fuzz 覆盖。

**任务**：
- [ ] 引入 `cargo-fuzz`，编写 `fuzz/fuzz_targets/fuzz_parse_diff.rs`
- [ ] 编写 `fuzz/fuzz_targets/fuzz_ast_extract.rs`

### 1.4 缺少 Benchmark 🟡 中优先级

**现状**：无任何性能基准，无法感知优化回归。

**任务**：
- [ ] 引入 `criterion`，为 `diff::parse_diff` 和 `ast::extract_context` 添加 benchmark
- [ ] 在 CI 中加 benchmark 对比，PR 性能退化时输出警告

### 1.5 format_symbol 硬编码语言标签 🟡 中优先级

**现状**：`src/prompt.rs:355–364` 的 `format_symbol` 函数对所有符号类型一律返回 `"rust"` 语言标签（`match` 三个分支全部返回 `"rust"`）。当 AST 提取到 Python / Go / Java 等文件的符号时，code fence 会被标注为 ` ```rust `，导致语法高亮错误，影响 LLM 对代码的上下文理解。

**任务**：
- [ ] 在 `Symbol` 结构体中携带原始文件扩展名或语言标识
- [ ] 修复 `format_symbol` 根据实际语言返回正确的 fence 标签（`python`、`go`、`java` 等）

---

## 二、可靠性与容错

### 2.1 超时与瞬态错误重试 ✅ 已完成

**已改善**：在 `reviewer.rs` 中引入 `is_retryable_api_error` / `is_retryable_review_error` 两个辅助函数，将错误分为两类：
- **可重试**：超时、5xx 服务端错误、429 限流、网络重置 — `continue` 进入下一次重试
- **不可重试**：401 / 403 / 404 / 无效 API Key — 立即 `return Err` 快速失败

超时不再直接返回 `ReviewError::Completion`，而是更新 `last_error` 后继续重试循环，与 JSON 解析失败的退避重试统一处理。两个函数均有 13 个单元测试覆盖所有分支。

### 2.2 缺少 Provider 级别 Fallback 🟡 中优先级

**现状**：reviewer-1 和 reviewer-2 各自固定绑定一个 provider。若某 provider API 完全不可用，对应的所有 4 个 focus group 全部 FAIL。

**任务**：
- [ ] 支持 `--reviewer-1-fallback` 参数，指定备用 provider
- [ ] 在 `LlmReviewer` 中实现 fallback 逻辑：主 provider 连续失败 N 次后切换

### 2.3 Rate Limit 处理（部分完成）🟡 中优先级

**已改善**：429 响应现在被 `is_retryable_api_error` 识别为可重试错误，会进入指数退避重试循环，不再直接 FAIL。

**仍缺失**：
- [ ] 解析 `Retry-After` 响应头，按 header 指定时间等待而非固定退避
- [ ] 支持 `--concurrent-reviews` 参数，限制同时在途的 LLM 调用数

### 2.4 self-correction 循环已修复 ✅ 已完成

**已改善**：self-correction 不再追加对话历史。`build_correction_prompt` 将原始 user prompt、bad response、parse error 和 schema 全部内联到每次重试的提示中，每次重试完全自包含（`reviewer.rs:96–113`），消除了对话历史膨胀和 token 超限风险。

---

## 三、性能优化

### 3.1 缺少结果缓存 🟡 中优先级

**现状**：相同 diff 每次都重新调用 LLM，在 CI 重跑时浪费时间和费用。

**任务**：
- [ ] 计算 diff 内容 + policy 内容的 SHA-256 作为 cache key
- [ ] 支持 `--cache-dir` 参数，将 `ReviewResult` 序列化缓存到磁盘
- [ ] 在 GitHub Actions workflow 中使用 `actions/cache` 缓存 review 结果

### 3.2 Token 估算不准确 🟢 低优先级

**现状**：使用 `chars / 4` 估算 token 数（`reviewer.rs:124–125`），对中文、代码、特殊字符误差大。

**任务**：
- [ ] 引入 `tiktoken-rs` 或 `tokenizers` 做精确 token 计数
- [ ] 在 `prompt.rs` 中添加 prompt token 预算检查，超过模型上下文限制时自动截断低优先级内容

### 3.3 大 diff 处理策略单一 🟡 中优先级

**现状**：diff 超过 5000 行时直接 `bail!` 退出（exit code 2），CI 失败无任何 review 输出。

**任务**：
- [ ] 当 diff 超限时，按文件重要性排序，只取前 N 行做 review，输出警告说明覆盖范围
- [ ] 支持 `--on-diff-overflow truncate|fail|split` 策略参数

### 3.4 LLM 调用不支持流式输出 🟢 低优先级

**现状**：等待 LLM 返回完整响应后再处理，延迟高（120s 超时上限）。

**任务**：
- [ ] 评估 rig-core 是否支持 streaming API
- [ ] 若支持，改用流式接收，在检测到完整 JSON 对象后立即解析

---

## 四、可观测性

### 4.1 Prometheus metrics 导出 ✅ 已完成

**已完成**：`telemetry.rs:150–193` 中 `Metrics::export()` 支持两个独立的导出 sink：
- `METRICS_FILE_PATH` — 写入 Prometheus 文本格式文件（适配 node_exporter textfile collector）
- `PROMETHEUS_PUSHGATEWAY_URL` — 推送到 Prometheus Pushgateway

两个 sink 均可选、独立配置，导出失败不影响 gate 结果。

### 4.2 缺少结构化日志输出 🟡 中优先级

**现状**：日志格式为 tracing 默认的人类可读格式，CI 日志难以被 Datadog / Splunk 等解析。

**任务**：
- [ ] 添加 `--log-format json` 参数，启用 `tracing_subscriber::fmt::json()` 格式
- [ ] 在 GitHub Actions workflow 中设置 `--log-format json` 并将日志归档

### 4.3 缺少 SLO 定义与告警 🟢 低优先级

**缺失**：没有定义 SLO（如"review 成功率 > 95%"、"P99 耗时 < 60s"），没有 alerting 规则。

**任务**：
- [ ] 在 README 或独立文档中定义 SLO 目标
- [ ] 提供 Prometheus alerting rule 示例（YAML）

---

## 五、安全性

### 5.1 缺少依赖漏洞扫描 🔴 高优先级

**现状**：CI 中无 `cargo audit` 或 `cargo deny`，Cargo.lock 锁定了具体版本但无审计流程。

**任务**：
- [ ] 在 CI 中添加 `cargo audit` 步骤（`rustsec/audit-check` action）
- [ ] 添加 `cargo deny` 配置，限制 license 白名单和已知漏洞依赖

### 5.2 缺少 SBOM 生成 🟢 低优先级

**缺失**：无软件物料清单（SBOM），不满足部分企业合规要求。

**任务**：
- [ ] 在 release CI 中添加 `cargo cyclonedx` 生成 SBOM
- [ ] 将 SBOM 文件作为 GitHub Release artifact 附件发布

### 5.3 API Key 在进程参数中可见 🟡 中优先级

**现状**：`--reviewer-1-api-key` / `--reviewer-2-api-key` CLI 参数通过 clap `env =` 读取 env var，但也允许从命令行直接传值，此时 key 会出现在 `ps aux` 进程列表中。

**任务**：
- [ ] 将 `--reviewer-N-api-key` 标注为不推荐，文档强调只用 env var 传密钥
- [ ] 支持从文件读取 key：`--reviewer-1-api-key-file /run/secrets/key`

### 5.4 LLM 工具调用输入未做严格大小限制 🟡 中优先级

**现状**：`tools.rs` 中 `read_file` 工具有 `MAX_LINES=300` 的输出截断，但对 `path` 参数本身的字节长度无限制；`find_symbol` 工具对 `name` 参数也无大小限制。若 LLM 被 prompt injection 操纵，可能构造超长路径或符号名导致异常。

**任务**：
- [ ] 对 `path` 参数添加长度检查（如最大 512 字节）
- [ ] 对 `name` 参数添加长度检查（如最大 256 字节）
- [ ] 添加工具调用日志，记录每次调用的 path + 返回行数（用于审计）

---

## 六、工程规范

### 6.1 缺少语义化版本与自动发布 🔴 高优先级

**现状**：`Cargo.toml` 版本是 `0.1.0`，无 GitHub Release，无 Changelog，无版本标签。

**任务**：
- [ ] 建立语义化版本策略（SemVer）
- [ ] 编写 `CHANGELOG.md`，记录版本变更
- [ ] 添加 release CI workflow：打 git tag 时自动构建多平台二进制（x86_64-linux、aarch64-linux、x86_64-darwin、aarch64-darwin）并发布到 GitHub Releases

### 6.2 缺少 Clippy 和 rustfmt 的 CI 强制检查 🟡 中优先级

**现状**：CI 只有 `docker-publish.yml` 和 `ai-review.yml` 两个 workflow，无 `cargo clippy` 或 `cargo fmt --check` 步骤。CLAUDE.md 提到 `cargo clippy -- -D warnings` 但未在 CI 中执行。

**任务**：
- [ ] 新增 `ci.yml` workflow：在每个 PR 上运行 `cargo clippy -- -D warnings` 和 `cargo fmt --check`
- [ ] 添加 `.rustfmt.toml` 配置文件统一代码风格

### 6.3 缺少 rustdoc API 文档 🟢 低优先级

**现状**：`telemetry.rs` 和 `tools.rs` 有模块级 doc comments，但 `models/mod.rs` 的公开类型、`consensus.rs` 的 gate 逻辑等核心接口文档不完整。

**任务**：
- [ ] 为 `models/mod.rs` 中的所有公开类型补充 doc comments
- [ ] 为 `consensus.rs` 的 `evaluate`/`evaluate_pair` 函数添加详细注释（PASS 条件）
- [ ] 在 CI 中添加 `cargo doc --no-deps` 步骤，确保文档可正常生成

### 6.4 缺少架构决策记录 (ADR) 🟢 低优先级

**缺失**：为什么选 rig-core？为什么 confidence 阈值是 0.90？为什么用 4 个 focus group？为什么每次重试使用自包含 prompt 而不是追加历史？这些决策没有文档记录。

**任务**：
- [ ] 建立 `docs/adr/` 目录，用 Markdown 格式记录关键架构决策

### 6.5 main.rs 过于庞大 🟡 中优先级

**现状**：`main.rs` 440 行，混合了 CLI 解析、provider 初始化、并发调度、结果汇总逻辑。

**任务**：
- [ ] 将 provider 初始化逻辑（`build_reviewer`）提取到 `src/providers.rs`
- [ ] 将并发调度逻辑（group 构建、`join_all`、pair results 汇总）提取到 `src/orchestrator.rs`
- [ ] `main.rs` 只保留 CLI 解析和入口调用（目标 < 100 行）

---

## 七、运维与部署

### 7.1 Dockerfile ✅ 已完成

- [x] 多阶段 `Dockerfile`（`cargo-chef` 缓存依赖层，debian-slim runner）
- [x] 发布到 GitHub Container Registry（`ghcr.io`）— 见 `.github/workflows/docker-publish.yml`
- [x] CI workflow 直接 `docker pull` 运行，PR review 无编译 — 见 `.github/workflows/ai-review.yml`

### 7.2 CI 编译时间 ✅ 已完成

- [x] `docker-publish.yml` 在 push to main 时构建镜像并推送 ghcr.io
- [x] `ai-review.yml` 直接 `docker pull` 运行，PR review 无编译；首次 setup 自动 fallback 到本地 build
- [x] Docker 构建使用 `cache-from/cache-to: type=gha` 缓存 cargo-chef 依赖层

### 7.3 缺少 Dependabot 配置 🟡 中优先级

**现状**：无自动依赖更新机制，安全漏洞 fix 版本发布后需手动更新。

**任务**：
- [ ] 添加 `.github/dependabot.yml`，配置 Cargo 和 GitHub Actions 依赖的自动 PR

### 7.4 缺少本地开发环境文档 🟢 低优先级

**现状**：README 有快速上手，但没有说明如何在本地 mock LLM 接口做开发。

**任务**：
- [ ] 在 README 或 `docs/development.md` 中记录本地开发流程：如何用 `mock_review_e2e` 测试、如何设置 `RUST_LOG`、如何生成测试 diff

---

## 八、已达到的生产标准（勿退化）

以下已做到位，未来修改时注意不要破坏：

- ✅ **并发执行**：8 个 LLM 调用真正并发（`futures::join_all`），不是顺序执行
- ✅ **指数退避重试**：JSON 解析失败时 1s→2s→4s→8s→16s 退避
- ✅ **路径穿越防护**：`safe_join()` 阻止 `../../etc/passwd`，有测试覆盖
- ✅ **OpenTelemetry 集成**：可接入 Jaeger / Tempo 做分布式追踪
- ✅ **多 provider 支持**：通过 rig-core trait 抽象支持 Minimax / DeepSeek / Anthropic / Gemini / OpenAI，换 provider 不改核心逻辑
- ✅ **Anthropic prompt caching**：系统 prompt 带 `cache_control: ephemeral`，CI 重复运行时显著降低成本和延迟
- ✅ **工具调用增强**：LLM 可通过 `read_file` / `find_symbol` 工具在审查时主动获取额外上下文（最多 8 轮工具调用）
- ✅ **多语言 AST**：支持 12 种语言（Rust、Python、Go、JavaScript、TypeScript、Java、C、C++、Ruby、C#、Bash、Scala）
- ✅ **self-correction 自包含**：每次重试使用独立的自包含 prompt，不累积对话历史，避免 token 膨胀
- ✅ **智能重试分类**：`is_retryable_api_error` 区分瞬态错误（5xx/429/超时/网络）与永久错误（401/403/404），瞬态错误退避重试，永久错误立即快速失败
- ✅ **Prometheus metrics 导出**：支持 textfile sink（node_exporter）和 Pushgateway 两个导出方式
- ✅ **Diff 大小防护**：超限 fast-fail 而非静默截断
- ✅ **Finding 去重**：`(file, line_start, rule_id)` 三元组去重，避免重复报告
- ✅ **TLS 强制**：reqwest 使用 rustls，无明文 HTTP
- ✅ **Release 优化**：`lto=true`、`opt-level=3`、`codegen-units=1`、`strip=true`
- ✅ **Ignore 注解**：支持 `// ai-reviewer: ignore[RULE-ID]` 抑制指定规则的 finding

---

## 快速任务索引（按优先级）

### 立即可做（高优先级，影响大）

1. `cargo audit` + `cargo deny` 加入 CI
2. 新增 `ci.yml` workflow：`cargo clippy -- -D warnings` + `cargo fmt --check`
3. 集成测试改为 `cargo test` 可运行（引入 `wiremock`）
4. ~~超时触发重试而非直接 FAIL~~ ✅ 已完成（`is_retryable_api_error` + `is_retryable_review_error`）

### 下一阶段（中优先级，需设计）

5. `format_symbol` 修复语言标签硬编码（`prompt.rs:355–364`）
6. Provider fallback 机制
7. Rate limit 429 识别与重试
8. 结果缓存（基于 diff hash）
9. `main.rs` 拆分（`providers.rs` + `orchestrator.rs`）
10. 添加 Dependabot 配置
11. 工具调用输入长度校验（`tools.rs` path/name 参数）
12. 结构化 JSON 日志（`--log-format json`）

### 有空再做（低优先级，锦上添花）

13. Fuzz 测试（`diff::parse_diff`、`ast::extract_context`）
14. Benchmark（`criterion`）
15. rustdoc 文档补全
16. SBOM 生成（`cargo cyclonedx`）
17. ADR 文档（`docs/adr/`）
18. SLO 定义与 Prometheus alerting rules
19. 语义化版本 + GitHub Release workflow
