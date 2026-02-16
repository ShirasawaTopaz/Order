# TODO

本文档用于定义 code agent 的下一阶段优化方向。每项均包含功能目标、技术方案与 MVP 验收标准，便于直接进入开发阶段。

## P0（高优先）

### 2. 能力降级策略稳健化（Capability Fallback Hardening）
功能目标：
- 将“基于错误文案猜测降级”升级为“可解释、可复用、可验证”的降级决策流程。
- 在多 provider / 多网关场景下，降低误判降级、重复失败和无效重试。
- 确保降级结果可以被缓存并带有效期管理，避免长期固化错误状态。

技术方案：
- 在 `crates/core/src/model` 增加 `ErrorClassifier` 与 `CapabilityFallbackPlan`：
  1. `ErrorClassifier` 统一解析 `status code`、`provider error code`、`endpoint`、`request flags(tools/stream/responses)`。
  2. 将错误映射为标准类别：`ToolsUnsupported`、`ResponsesUnsupported`、`StreamUnsupported`、`AuthError`、`RateLimited`、`TransientNetwork`、`Unknown`。
- 在 `Connection` 请求主流程中引入“显式降级状态机”：
  1. 首次请求失败后先分类，而不是直接字符串 contains。
  2. 仅对“可降级类型”执行降级，且限制最大降级步数与最大尝试次数（例如 3 步）。
  3. 对不可降级错误（鉴权、配额、参数错误）立即失败，避免无意义重试。
- 扩展能力缓存模型（`.order/capabilities.json`）：
  1. 增加 `reason`、`first_seen_at`、`last_seen_at`、`ttl`、`confidence` 字段。
  2. 支持 TTL 过期后自动重新探测，避免“永久降级”。
  3. 记录降级来源（配置覆盖 / 运行时回写 / 手工重置）。
- 新增 `capability reset` 命令与诊断入口：
  1. 支持按 provider/model 清除降级缓存。
  2. `/status` 展示当前生效能力与最近一次降级原因。
- 测试补强：
  1. 为 OpenAI/Codex/OpenAIAPI 典型错误体构造 fixture。
  2. 覆盖“可降级”“不可降级”“TTL 过期重探测”“缓存写回失败不影响主流程”等分支。

MVP 验收：
- 已知不兼容网关场景下，首次失败后可在一次自动降级内恢复可用。
- 同一错误在 24 小时内不再重复触发相同无效请求。
- 日志中可直接看到“错误分类 -> 降级动作 -> 最终能力”的完整链路。

### 3. 可观测性补齐 Token/成本维度（Usage & Cost Observability）
功能目标：
- 除“成功率/耗时/重试率”外，提供 token 用量与成本的统一统计。
- 支持按 provider/model/任务维度追踪消耗，便于容量规划和成本控制。
- 让每次请求的产出与消耗可对齐到同一 `trace_id`。

技术方案：
- 扩展 `AgentEvent`（`crates/core/src/observability.rs`）：
  1. 在 `RequestEnd` 增加 `usage` 子结构：`prompt_tokens`、`completion_tokens`、`total_tokens`。
  2. 增加 `cost` 子结构：`currency`、`input_cost`、`output_cost`、`total_cost`（可为空）。
- 在连接层采集 usage：
  1. 封装 `ProviderUsageExtractor`，按 provider 响应结构提取 token 统计。
  2. 提取失败时不中断主流程，仅打 `usage_missing` 标记。
- 引入价格配置文件 `.order/pricing.toml`：
  1. 以 provider + model 维护输入/输出单价。
  2. 支持默认价格与模型级覆盖。
  3. 未配置价格时仅展示 token，不计算成本。
- 升级 `/status` 聚合：
  1. 近 24h 增加总 token、平均每请求 token、估算总成本。
  2. 增加按 model TopN 消耗排行。
  3. 支持输出简版报告到 `.order/reports/usage-YYYYMMDD.json`。
- 测试补强：
  1. usage 提取器的 provider 级单测。
  2. 成本聚合与空价格回退单测。

MVP 验收：
- 任意一次成功请求均可在日志中检索到 token 字段（若 provider 返回）。
- `/status` 可展示近 24h token 与估算成本统计。
- 未配置价格时系统正常运行，仅缺失成本字段且有明确提示。

## P1（中优先）

### 4. 上下文预算精度提升（Tokenizer-aware Budgeting）
功能目标：
- 解决“字符估算 token”偏差导致的上下文截断不稳问题。
- 在中英混排、代码块、长路径等场景下提升预算命中率。
- 减少因为预算误差引发的响应质量抖动。

技术方案：
- 在 `crates/rander/src/history.rs` 引入 `TokenEstimator` 抽象：
  1. `ExactEstimator`：按 provider/model 使用对应 tokenizer（可选特性开关）。
  2. `HeuristicEstimator`：保留现有估算作为兜底。
- 实施“两阶段预算控制”：
  1. 阶段一按当前策略生成候选上下文。
  2. 阶段二按 tokenizer 实测 token 校准，超预算则按优先级继续裁剪。
- 细化裁剪优先级与最小保真规则：
  1. 固定保留最近 N 轮 user/assistant 配对。
  2. 中期摘要按段落级裁剪，不直接截断到半句。
  3. 长期记忆按类别限额（规则/偏好/决策）独立裁剪。
- 在日志中补充上下文构建指标：
  1. `input_budget`、`used_tokens`、`trimmed_items`、`fallback_estimator_used`。
  2. 便于回溯“为什么这轮上下文被裁剪”。
- 测试补强：
  1. 中英文、代码混排、超长会话的预算回归测试。
  2. 历史裁剪后摘要仍可读性与完整性测试。

MVP 验收：
- 相比现状，上下文超预算导致的请求失败率显著下降。
- 多轮长对话中，关键约束保留率提升且波动降低。
- `/status` 或日志可看到每轮上下文预算使用明细。

### 5. 自动验证异步化与可配置白名单（Async Validation Pipeline）
功能目标：
- 避免 `/approve` 后同步阻塞主交互，提升 TUI 可用性。
- 将验证命令安全策略从“仅 cargo”升级为“可配置前缀白名单”。
- 让验证任务可观察、可中断、可复现。

技术方案：
- 在 `core::validation` 引入任务执行器 `ValidationRunner`：
  1. `tui` 发起任务后立即返回 `task_id`，后台异步执行。
  2. 通过事件总线推送状态：`queued/running/success/failed/cancelled`。
- 扩展 `.order/validation.toml`：
  1. `allow_prefixes = [["cargo"], ["pnpm","test"], ["pytest"]]`。
  2. 支持命令级超时、最大并发、失败后是否继续。
  3. 明确区分 `minimal` 与 `extended` 的触发条件。
- 增加验证命令：
  1. `/validation status <task_id>` 查看进度与失败摘要。
  2. `/validation cancel <task_id>` 取消仍在运行的任务。
  3. `/validation rerun <trace_id>` 基于历史改动重跑。
- 报告增强：
  1. 记录执行主机、工作目录、命令版本信息。
  2. 保存可直接复制的“最小复现命令组”。
- 测试补强：
  1. 异步状态流转测试。
  2. 白名单拦截与越权命令拒绝测试。
  3. 超时取消与资源清理测试。

MVP 验收：
- `/approve` 后界面不再阻塞，可继续执行普通操作。
- 非白名单命令被明确拒绝并给出原因。
- 验证失败时可通过报告一键复现失败命令。

### 6. 安全确认升级为可审阅 Patch（Reviewable Write Gate）
功能目标：
- 将“行数摘要确认”升级为“可审阅、可比对、可防并发冲突”的写入确认机制。
- 降低误写风险，提升用户对变更落盘前的可控性。
- 强化回滚的完整性和一致性校验。

技术方案：
- 在 `ExecutionGuard` 的 stage 阶段生成统一 diff：
  1. 为每个待写文件生成 `unified patch`。
  2. 汇总写入 `.order/pending/writes/<trace_id>/preview.patch`。
  3. 在 TUI 展示“文件列表 + 关键 hunks 摘要 + 风险标签”。
- 增加落盘前冲突检测：
  1. stage 时记录文件 `hash/mtime/size`。
  2. approve 时再次校验，若文件已被外部修改则拒绝并要求重新 stage。
- 优化回滚元数据：
  1. `manifest.json` 增加快照 hash 与文件校验值。
  2. rollback 前后做一致性校验并输出校验报告。
- 增强确认交互：
  1. `/approve <trace_id> --files a,b` 支持按文件粒度确认（可选）。
  2. `/approve` 前先显示待确认摘要，避免盲确认。
- 测试补强：
  1. 覆盖覆盖写、追加写、新建文件、删除回滚。
  2. 覆盖“stage 后文件被篡改”的拒绝路径。

MVP 验收：
- 用户可在确认前查看可读 patch，而非仅行数统计。
- 发生并发改写时，系统能拒绝落盘并给出重试指引。
- 回滚后可校验恢复一致性，并输出明确结果。

## P2（增强）

### 7. 文档与实现一致性治理（Doc-Impl Drift Guard）
功能目标：
- 避免 README/帮助文档与实际命令能力漂移。
- 形成“命令元数据单一事实源”，降低手工维护成本。
- 在 CI 阶段自动阻断文档过期。

技术方案：
- 建立命令清单单源 `CommandCatalog`：
  1. 包含命令名、参数、描述、风险级别、是否对外可见。
  2. TUI 自动补全、`/help`、README 命令段统一从该清单生成。
- 增加文档生成脚本（如 `xtask`）：
  1. 自动渲染 README 命令列表区块。
  2. 自动生成 `docs/commands.md` 明细页。
- CI 增加一致性校验：
  1. 执行生成器后检测 git diff 是否为空。
  2. 若不为空则失败并提示运行更新命令。
- 为行为变更引入文档检查清单：
  1. PR 模板中强制勾选“已更新命令文档/帮助文本”。

MVP 验收：
- 新增/删除命令后，README 与 `/help` 自动同步。
- CI 能拦截命令文档未更新的提交。
- 命令说明来源唯一，避免多处不一致。

### 8. 技术债收敛：输入与命令系统重构（Input & Command Refactor）
功能目标：
- 消除关键交互路径中的遗留 TODO 与重复实现。
- 降低 `tui.rs` 超大文件维护成本，提升后续迭代效率。
- 将命令解析、执行与渲染职责解耦，减少回归风险。

技术方案：
- 完成多行输入能力：
  1. 实现 `Shift+Enter` 插入换行。
  2. 明确提交手势（如 `Enter` 提交单行、`Ctrl+Enter` 提交多行）。
  3. 适配补全弹窗与光标行为，避免输入状态错乱。
- 合并命令系统：
  1. 提炼共享 `CommandParser/CommandExecutor` 模块。
  2. 移除未被主流程使用的旧命令分支，避免双轨逻辑漂移。
- 拆分大文件与模块边界：
  1. 将 `tui.rs` 拆分为 `request_flow`、`command_handlers`、`status_view`、`persistence` 等子模块。
  2. 明确每个模块的输入输出与副作用边界。
- 测试补强：
  1. 增加命令解析表驱动测试。
  2. 增加多行输入与提交手势的交互测试。
  3. 增加关键命令（approve/reject/rollback/status）回归测试。

MVP 验收：
- `Shift+Enter` 多行输入可用且不会破坏现有补全/发送逻辑。
- 命令解析入口统一，删除重复实现后功能无回归。
- `tui` 主文件体积明显下降，模块职责清晰可维护。