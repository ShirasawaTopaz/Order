# TODO

本文档用于定义 code agent 的功能演进方向。每项均包含功能目标、技术方案与 MVP 验收标准，便于直接进入开发阶段。

## P0（高优先）

### 1. 能力协商层（Provider Capability Negotiation）
功能目标：
- 在请求前识别 provider 是否支持 `tools`、`system prompt`、`responses API`、`stream`。
- 避免“先失败再降级”的体验，减少 4xx/协议不兼容错误。

技术方案：
- 在 `crates/core/src/model` 增加能力模型 `ProviderCapabilities`。
- 增加 `CapabilityResolver`，按优先级聚合能力来源：
  1. 本地静态默认表（内置已知 provider 能力）
  2. 配置覆盖（`.order/model.json` 可显式声明）
  3. 启动探测缓存（首次探测后写入 `.order/capabilities.json`）
- 在 `Connection::builder` 与请求入口中根据能力动态选择：
  - 是否注入 tools
  - 是否发送 system preamble
  - 使用 responses API 还是 chat/completions API
- 约定“能力失效回写”：若运行时出现协议错误，降级并更新缓存，避免重复踩坑。

MVP 验收：
- 同一 provider 连续 10 次请求不再出现重复协议类错误。
- 能在日志中看到最终协商结果（tools/system/endpoint）。

### 2. 结构化可观测性（Observability）
功能目标：
- 将“失败不可解释”变为“失败可定位”。
- 每次请求可追踪：请求链路、耗时、重试、工具调用、token 用量。

技术方案：
- 在 `crates/core` 增加统一结构化日志事件 `AgentEvent`（建议 JSON Line）。
- 建立 `trace_id` 贯穿链路：TUI 输入 -> model 请求 -> tools 调用 -> 响应输出。
- 关键事件分层：
  - `request_start/request_end`
  - `retry_scheduled/retry_exhausted`
  - `tool_call_start/tool_call_end`
  - `fallback_applied`
- 在 `.order/logs/` 按天滚动输出，如 `agent-YYYYMMDD.log`。
- 在 TUI 状态栏提供“最近一次失败摘要（trace_id + 原因）”。

MVP 验收：
- 任意一次失败都能通过 `trace_id` 在 1 分钟内定位到错误点。
- 能统计 24 小时成功率、平均耗时、重试率。

### 3. 安全执行闸门（Safety Gate）
功能目标：
- 对有副作用操作（写文件、执行命令）建立可确认、可回滚机制。
- 降低误改、误执行风险。

技术方案：
- 引入 `ExecutionGuard`：
  - 风险分级：`read-only`、`low-risk`、`high-risk`
  - 针对 `high-risk` 强制二次确认
- 写文件前生成预览 diff（统一展示增删行数与关键文件列表）。
- 为每次写入创建快照（如 `.order/snapshots/<trace_id>/`）。
- 提供回滚命令：按 `trace_id` 或最近一次操作恢复。
- 命令执行增加 allowlist 与前缀规则，默认拒绝破坏性命令。

MVP 验收：
- 高风险写入必须经过确认，未确认时不落盘。
- 能在 30 秒内完成“最近一次操作”一键回滚。

### 4. 自动验证闭环（Auto-Validation Loop）
功能目标：
- 代码改动后自动完成最小验证与结果归档，减少人工来回。
- 验证失败时自动给出下一步修复建议。

技术方案：
- 在执行计划中增加 `ValidationPipeline`：
  1. 基于改动文件选择最小测试（如 `cargo test -p <crate>`）
  2. 通过后扩大为相关模块/workspace 检查
  3. 汇总结果并写入执行报告
- 规则来源：
  - 项目预设（Rust 默认 `cargo check/test`）
  - 本地配置覆盖（`.order/validation.toml`）
- 失败时记录：失败命令、关键报错、建议行动（重试/降级/回退）。

MVP 验收：
- 常规改动可自动执行最小验证并给出明确通过/失败结论。
- 失败结果包含可直接复制执行的复现命令。

## P1（中优先）

### 5. 上下文管理（Context Management）
功能目标：
- 在长会话中保持任务连续性，避免模型遗忘关键约束与决策。

技术方案：
- 引入三层上下文：
  - 短期上下文：最近 N 轮完整消息
  - 中期摘要：阶段性总结（目标、已完成、阻塞点）
  - 长期记忆：项目规则、偏好、关键决策（持久化）
- 新增 `ContextCompressor`，按 token 预算自动压缩历史。
- 将关键决策写入 `.order/context/memory.json`，按任务 ID 归档。

MVP 验收：
- 连续多轮任务中，关键约束不丢失率达到可接受水平（人工评估）。
- 上下文超长时响应质量无明显断崖下降。

### 6. 流式与中断控制（Streaming & Cancellation）
功能目标：
- 提供更实时的交互反馈，并可在长响应中安全中断。

技术方案：
- 统一流式事件协议：`delta`、`tool_progress`、`done`、`error`。
- 在 TUI 中支持：
  - 流式增量渲染
  - 用户取消（Ctrl+C 或命令级 cancel）
  - 超时中止与状态提示
- 重试策略采用指数退避 + 抖动，区分可重试错误与不可重试错误。

MVP 验收：
- 用户可在流式响应期间随时取消，且不会污染后续会话状态。
- 网络抖动场景下请求成功率有可观提升。

## P2（增强）

### 7. 编辑器能力补齐（LSP Workflow Completion）
功能目标：
- 补齐高频编辑闭环，减少“跳出编辑器操作”的次数。

技术方案：
- 基于现有 `crates/lsp` 客户端补充请求与事件映射：
  - `textDocument/rename`
  - `textDocument/codeAction`
  - `textDocument/formatting`
  - `workspace/applyEdit` 与诊断修复闭环
- 在 `crates/rander/src/editor` 新增对应快捷命令与 UI 提示。
- 对不同语言能力做特性探测，避免不支持时误调用。

MVP 验收：
- 在 Rust/TS 两类常见项目中可完成 rename + format + quick fix 基本链路。

### 8. Windows 编码健壮性（Encoding Robustness）
功能目标：
- 在 Windows 环境中稳定显示中文，避免乱码与输入异常。

技术方案：
- 启动时检测并设置控制台编码为 UTF-8（输入/输出双向）。
- 对文件读写统一 UTF-8（无 BOM）+ LF，写入前做编码校验。
- 在日志和历史文件中增加编码自检与错误提示（而不是静默失败）。
- 针对 PowerShell/Windows Terminal 做兼容提示（字体、code page、环境变量）。

MVP 验收：
- 常见中文输入/输出场景无乱码。
- 历史记录与配置文件在跨终端读取时保持一致可读。
