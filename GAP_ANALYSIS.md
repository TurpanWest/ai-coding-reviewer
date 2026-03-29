# 生产级别差距分析

> 目标标准：大厂生产级高可用 / 高可靠 / 高性能 CLI + CI/CD 工具
> 当前状态：功能完整的 Rust CLI，3822 行，13 个单元测试，支持 5 种 LLM provider
> 更新日期：2026-03-30

---

## 总体评分

| 维度 | 当前水平 | 差距 |
|---|---|---|
| 测试与质量保障 | ★★☆☆☆ | 大 |
| 可靠性与容错 | ★★★☆☆ | 中 |
| 性能优化 | ★★★☆☆ | 中 |
| 可观测性 | ★★★★☆ | 小 |
| 安全性 | ★★★☆☆ | 中 |
| 工程规范 | ★★☆☆☆ | 大 |
| 运维与部署 | ★★☆☆☆ | 大 |
| 代码架构 | ★★★★☆ | 小 |

---

## 一、测试与质量保障

### 1.1 测试覆盖率严重不足 🔴 高优先级

**现状**：13 个单元测试，覆盖 `diff.rs`、`ast.rs`、`tools.rs` 的 happy path。
**缺失**：
- `consensus.rs` 零测试（核心 gate 逻辑）
- `prompt.rs` 零测试（prompt 组装错误会导致 LLM 输出垃圾）
- `report.rs` 零测试（输出格式正确性无保证）
- `main.rs` 零测试（CLI 参数解析、provider 初始化逻辑）
- `telemetry.rs` 零测试

**任务**：
- [ ] 为 `consensus.rs` 编写单元测试：覆盖 PASS/FAIL 边界、confidence 阈值、finding 去重逻辑
- [ ] 为 `prompt.rs` 编写快照测试：对固定输入断言 system prompt 和 user prompt 的格式
- [ ] 为 `report.rs` 编写单元测试：验证 Markdown 渲染输出
- [ ] 补充 `diff.rs` 边界测试：二进制文件、空 diff、仅删除文件、rename

### 1.2 缺少集成测试 🔴 高优先级

**现状**：`mock_review_e2e.rs` 是一个手动运行的 binary，不是 `cargo test` 的一部分。
**缺失**：真正的 `#[tokio::test]` 集成测试，用 `wiremock` 或 `httpmock` mock LLM 端点，验证完整 review 流程。

**任务**：
- [ ] 引入 `wiremock` 或 `httpmock` 依赖
- [ ] 将 `mock_review_e2e.rs` 改写为 `tests/integration_test.rs`，使其可 `cargo test` 运行
- [ ] 覆盖场景：API 返回无效 JSON → 触发 retry；API 超时 → 返回 FAIL；两模型分歧 → 返回 FAIL

### 1.3 缺少 Fuzz 测试 🟡 中优先级

**现状**：diff parser 和 AST extractor 处理外部输入，但无 fuzz 覆盖。
**缺失**：对 `diff::parse_diff` 和 `ast::extract_context` 的 fuzz 测试，防止 panic。

**任务**：
- [ ] 引入 `cargo-fuzz`，编写 `fuzz/fuzz_targets/fuzz_parse_diff.rs`
- [ ] 编写 `fuzz/fuzz_targets/fuzz_ast_extract.rs`

### 1.4 缺少 Benchmark 🟡 中优先级

**现状**：无任何性能基准，无法感知优化回归。

**任务**：
- [ ] 引入 `criterion`，为 `diff::parse_diff` 和 `ast::extract_context` 添加 benchmark
- [ ] 在 CI 中加 benchmark 对比，PR 性能退化时输出警告

---

## 二、可靠性与容错

### 2.1 超时后无重试 🔴 高优先级

**现状**：LLM 调用超时后直接转为 `ReviewError::Completion`，进入 consensus 计为 FAIL，整个 review 失败。
**问题**：LLM API 偶发超时是常态（网络抖动、provider 负载），一次超时导致 PR 被 block 不合理。

**任务**：
- [ ] 在 `models/reviewer.rs` 中，将超时单独处理：超时触发重试（最多 2 次），而不是立即转为 FAIL
- [ ] 区分 "超时" 和 "API 返回错误"：超时可重试，4xx 不重试，5xx 有限重试

### 2.2 缺少 Provider 级别 Fallback 🟡 中优先级

**现状**：reviewer-1 和 reviewer-2 各自固定绑定一个 provider。若某 provider API 完全不可用，对应的所有 4 个 focus group 全部 FAIL。
**缺失**：provider 级别的 fallback（如 MiniMax 不可用时自动切换到 OpenAI）。

**任务**：
- [ ] 支持 `--reviewer-1-fallback` 参数，指定备用 provider
- [ ] 在 `LlmReviewer` 中实现 fallback 逻辑：主 provider 连续失败 N 次后切换

### 2.3 缺少 Rate Limit 处理 🟡 中优先级

**现状**：8 个 LLM 调用并发发起，若 provider 有 rate limit（如 DeepSeek 免费层），会收到 429 响应，当前代码将 429 作为普通错误处理，不识别 `Retry-After` 头。

**任务**：
- [ ] 在 HTTP 错误处理中识别 429 状态码
- [ ] 解析 `Retry-After` 响应头，在指定时间后重试
- [ ] 支持 `--concurrent-reviews` 参数，限制同时在途的 LLM 调用数

### 2.4 self-correction 循环的退化问题 🟡 中优先级

**现状**：JSON 解析失败时，将原始响应追加到对话历史并重发。随着对话历史增长，token 消耗翻倍，可能触发模型的上下文长度限制，引发新错误。

**任务**：
- [ ] 在 retry 时不追加完整历史，只发送 "修正提示 + 上一次原始响应摘要"
- [ ] 添加对话历史 token 预算检查，超限时截断

---

## 三、性能优化

### 3.1 缺少结果缓存 🟡 中优先级

**现状**：相同 diff 每次都重新调用 LLM，在 CI 重跑时浪费时间和费用。
**缺失**：基于 diff 内容哈希的本地缓存（或 GitHub Actions cache）。

**任务**：
- [ ] 计算 diff 内容 + policy 内容的 SHA-256 作为 cache key
- [ ] 支持 `--cache-dir` 参数，将 `ReviewResult` 序列化缓存到磁盘
- [ ] 在 GitHub Actions workflow 中使用 `actions/cache` 缓存 review 结果

### 3.2 Token 估算不准确 🟢 低优先级

**现状**：使用 `chars / 4` 估算 token 数，对中文、代码、特殊字符误差大（中文约 1.5 chars/token，代码约 3 chars/token）。

**任务**：
- [ ] 引入 `tiktoken-rs` 或 `tokenizers` 做精确 token 计数
- [ ] 在 `prompt.rs` 中添加 prompt token 预算检查，超过模型上下文限制时自动截断低优先级内容

### 3.3 大 diff 处理策略单一 🟡 中优先级

**现状**：diff 超过 5000 行时直接 `bail!` 退出（exit code 2），CI 失败无任何 review 输出。
**更好的策略**：按文件优先级（安全相关文件优先）分片处理，至少输出部分 review。

**任务**：
- [ ] 当 diff 超限时，按文件重要性排序，只取前 N 行做 review，输出警告说明覆盖范围
- [ ] 支持 `--on-diff-overflow truncate|fail|split` 策略参数

### 3.4 LLM 调用不支持流式输出 🟢 低优先级

**现状**：等待 LLM 返回完整响应后再处理，延迟高（120s 超时）。
**改进**：流式接收响应，可更早检测 JSON 开始/结束，减少有效等待时间。

**任务**：
- [ ] 评估 rig-core 是否支持 streaming API
- [ ] 若支持，改用流式接收，在检测到完整 JSON 对象后立即解析

---

## 四、可观测性

### 4.1 Prometheus metrics 未暴露给外部 🟡 中优先级

**现状**：`telemetry.rs` 中有 Prometheus metrics 定义（counter、histogram），但 CLI 工具没有 HTTP server 来 expose `/metrics` 端点；metrics 只在进程内，无法被 Prometheus scrape。

**任务**：
- [ ] 在 review 结束时，将 Prometheus metrics 以文本格式写入文件（`--metrics-output` 参数）
- [ ] 在 GitHub Actions 中，将 metrics 文件上传为 artifact，或推送到 Pushgateway

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

**现状**：无自动化依赖漏洞检查，Cargo.lock 锁定了具体版本但无审计流程。

**任务**：
- [ ] 在 CI 中添加 `cargo audit` 步骤（`rustsec/audit-check` action）
- [ ] 添加 `cargo deny` 配置，限制 license 白名单和已知漏洞依赖

### 5.2 缺少 SBOM 生成 🟢 低优先级

**缺失**：无软件物料清单（Software Bill of Materials），不满足部分企业合规要求。

**任务**：
- [ ] 在 release CI 中添加 `cargo cyclonedx` 生成 SBOM
- [ ] 将 SBOM 文件作为 GitHub Release artifact 附件发布

### 5.3 API Key 在进程参数中可见 🟡 中优先级

**现状**：`--reviewer-1-api-key` 可以通过 CLI 参数传入，会出现在 `ps aux` 进程列表中（即使有 env var 备选路径）。

**任务**：
- [ ] 将 `--reviewer-1-api-key` 标注为不推荐，在文档中强调只用 env var 传密钥
- [ ] 支持从文件读取 key：`--reviewer-1-api-key-file /run/secrets/minimax_key`

### 5.4 LLM 工具调用输入未做严格大小限制 🟡 中优先级

**现状**：`read_file` 工具有 300 行截断，但 `find_symbol` 工具无大小限制；若 LLM 构造恶意参数（prompt injection），可能读取仓库外信息。

**任务**：
- [ ] 对所有工具调用参数做长度检查（file_path 最大 512 字节，symbol 名最大 256 字节）
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

**现状**：CLAUDE.md 中提到 `cargo clippy -- -D warnings`，但 CI workflow 中没有这一步。

**任务**：
- [ ] 在 CI 中添加 `cargo clippy -- -D warnings` 步骤（格式检查失败则 CI 失败）
- [ ] 在 CI 中添加 `cargo fmt --check` 步骤
- [ ] 添加 `.rustfmt.toml` 配置文件统一代码风格

### 6.3 缺少 rustdoc API 文档 🟢 低优先级

**现状**：公开函数和类型缺少 `///` doc comments，`cargo doc` 输出不完整。

**任务**：
- [ ] 为 `models/mod.rs` 中的所有公开类型添加 doc comments
- [ ] 为 `consensus.rs` 的 gate 逻辑添加注释说明 PASS 条件
- [ ] 在 CI 中添加 `cargo doc --no-deps` 步骤，确保文档可正常生成

### 6.4 缺少架构决策记录 (ADR) 🟢 低优先级

**缺失**：为什么选 rig-core？为什么 confidence 阈值是 0.90？为什么用 4 个 focus group？这些决策没有文档记录。

**任务**：
- [ ] 建立 `docs/adr/` 目录，用 Markdown 格式记录关键架构决策

### 6.5 main.rs 过于庞大 🟡 中优先级

**现状**：`main.rs` 451 行，混合了 CLI 解析、provider 初始化、并发调度、结果汇总逻辑。

**任务**：
- [ ] 将 provider 初始化逻辑提取到 `src/providers.rs`
- [ ] 将并发调度逻辑提取到 `src/orchestrator.rs`
- [ ] `main.rs` 只保留 CLI 解析和入口调用（目标 < 100 行）

---

## 七、运维与部署

### 7.1 缺少 Dockerfile ✅ 已完成

**现状**：用户需要本地安装 Rust 工具链才能使用，CI 每次 `cargo build --release` 耗时长。

**任务**：
- [x] 编写多阶段 `Dockerfile`（`cargo-chef` 缓存依赖层，`rust:alpine` builder，`alpine:3` runner）
- [x] 发布到 GitHub Container Registry（`ghcr.io`）— 见 `.github/workflows/docker-publish.yml`
- [x] 在 CI workflow 中改用 Docker 镜像运行 reviewer，避免编译时间 — 见 `.github/workflows/ai-review.yml`

### 7.2 CI 编译时间过长 ✅ 已完成

**现状**：`cargo build --release` + 13 个 tree-sitter 语言 binding 每次编译约 3-5 分钟，拖慢 PR 反馈。

**解决方案**：
- [x] `docker-publish.yml` 在 push to main（源码变更时）构建镜像并推送 ghcr.io
- [x] `ai-review.yml` 直接 `docker pull` 运行，PR review 无编译；首次 setup 自动 fallback 到本地 build
- [x] Docker 构建使用 `cache-from/cache-to: type=gha` 缓存 cargo-chef 依赖层，Cargo.lock 不变时依赖层命中缓存

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

- ✅ **并发执行**：8 个 LLM 调用真正并发（`tokio::join!`），不是顺序执行
- ✅ **指数退避重试**：JSON 解析失败时 1s→2s→4s→8s→16s 退避
- ✅ **路径穿越防护**：`safe_join()` 阻止 `../../etc/passwd`，有测试覆盖
- ✅ **OpenTelemetry 集成**：可接入 Jaeger / Tempo 做分布式追踪
- ✅ **多 provider 支持**：通过 rig-core trait 抽象，换 provider 不改核心逻辑
- ✅ **Diff 大小防护**：超限 fast-fail 而非静默截断
- ✅ **Finding 去重**：`(file, line_start, rule_id)` 三元组去重，避免重复报告
- ✅ **TLS 强制**：reqwest 使用 rustls，无明文 HTTP
- ✅ **Release 优化**：`lto=true`、`opt-level=3`、`codegen-units=1`、`strip=true`

---

## 快速任务索引（按优先级）

### 立即可做（高优先级，影响大）

1. `cargo audit` 加入 CI
2. 为 `consensus.rs` 补单元测试
3. 集成测试改为 `cargo test` 可运行
4. 超时触发重试而非直接 FAIL
5. `cargo clippy` 和 `cargo fmt --check` 加入 CI

### 下一阶段（中优先级，需设计）

6. Provider fallback 机制
7. Rate limit 429 识别与重试
8. 结果缓存（基于 diff hash）
9. `main.rs` 拆分（providers + orchestrator）
10. 添加 `Dependabot`
11. 编写 `Dockerfile`

### 有空再做（低优先级，锦上添花）

12. Fuzz 测试（`diff::parse_diff`、`ast::extract_context`）
13. Benchmark（`criterion`）
14. rustdoc 文档
15. SBOM 生成
16. ADR 文档
17. SLO 定义与 Prometheus alerting rules
