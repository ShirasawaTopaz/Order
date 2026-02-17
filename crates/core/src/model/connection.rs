use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use futures::StreamExt;
use rig::{
    agent::{Agent, MultiTurnStreamItem},
    client::CompletionClient,
    completion::{Chat, Message, Prompt},
    message::ToolResultContent,
    providers::{
        anthropic::{self, completion::CompletionModel as AnthropicCompletionModel},
        gemini::{self, CompletionModel as GeminiCompletionModel},
        openai::{
            self, completion::CompletionModel as OpenAICompletionModel,
            responses_api::ResponsesCompletionModel as OpenAIResponsesCompletionModel,
        },
    },
    streaming::{StreamedAssistantContent, StreamedUserContent, StreamingChat},
};

use super::{
    capabilities::{
        CapabilityResolver, CapabilityWritebackContext, ModelEndpoint, NegotiatedCapabilities,
        ProviderCapabilities, ProviderCapabilitiesOverride,
    },
    fallback::{CapabilityFallbackPlan, ClassifiedError, ErrorClassifier, RequestFeatureFlags},
};
use crate::observability::{
    AgentEvent, log_event_best_effort, ts, with_trace_id, workspace_root_best_effort,
};
use crate::tool::{
    command::CommandTool, read::ReadTool, search_file::SearchFileTool, write::WriteTool,
};

/// 默认系统提示。
///
/// 这里给出“先定位源码、再读取文件”的最小工作流约束，减少模型在编码场景下反复向用户追问路径。
pub const PREMABLE: &str = r#"你是仓库内的代码助手。
当用户没有明确给出文件路径时，请先调用 SearchFileTool 在当前工作区搜索，再调用 ReadTool 读取命中文件。
请遵循以下规则：
1. SearchFileTool 的 `path` 优先使用 `.`。
2. SearchFileTool 返回格式是 `<relative_path>:<line>:<line_content>`；后续调用 ReadTool/WriteTool 时只传 `relative_path`。
3. 读写路径只能使用工作区相对路径，不要要求用户提供绝对路径或当前目录。
4. 若首次关键词无结果，请自行更换 1-2 组同义关键词后再决定是否向用户追问。
5. 在完成至少一次“搜索或读取”之前，不要先向用户追问实现细节；优先基于现有代码自行推进。
6. 仅当存在真实阻塞（如需求冲突、权限限制、上下文缺失且无法通过工具补齐）时，才进行最小化追问。
7. 当用户请求“修改代码/修复问题/实现功能”时，默认立即动手执行：必须调用工具完成改动，而不是只回复计划、确认或承诺“下一步会做”。
8. 禁止只输出类似“我会继续”“我现在开始提交补丁”的过程性话术；若可以执行，就直接执行并产出结果。
9. 完成改动后，给出最小必要的结果说明：改了哪些文件、为什么这么改、如何验证。
10. 当需要执行构建、测试、格式化等终端命令时，直接调用 CommandTool，不要只口头描述“准备执行”。"#;

/// agent 多轮上限默认值。
///
/// rig 0.30 的 `default_max_turns` 默认为 `None`，在请求阶段会回退为 `0`，
/// 当模型需要连续多轮工具调用时会触发 `MaxTurnError(reached max turn limit: 0)`。
/// 这里显式设置一个保守上限，避免 Codex/工具链场景在首轮就被硬性终止。
const DEFAULT_AGENT_MAX_TURNS: usize = 12;
/// 可接受的最大轮次上限，避免配置异常导致超长循环。
const MAX_AGENT_MAX_TURNS: usize = 64;
/// 能力降级最多允许的步数，防止同一请求在错误分类不稳定时陷入长链路重试。
const MAX_CAPABILITY_FALLBACK_STEPS: usize = 3;
/// 每次请求允许的最大尝试次数（首轮 + 降级重试）。
const MAX_CAPABILITY_ATTEMPTS: u32 = MAX_CAPABILITY_FALLBACK_STEPS as u32 + 1;
/// 运行时降级缓存默认有效期：24 小时。
const RUNTIME_FALLBACK_CACHE_TTL_SECONDS: u64 = 24 * 60 * 60;

/// 将协商结果落地到 rig 的 agent builder（宏形式避免引入具体 builder 类型）。
///
/// 为什么用宏而不是函数：
/// - rig 的 builder 类型在不同 client/feature 下可能包含泛型或生命周期参数；
/// - 使用宏可以直接在调用点做链式调用，减少类型推断/签名耦合带来的编译脆弱性。
macro_rules! build_agent_with_options {
    ($builder:expr, $negotiated:expr, $max_turns:expr) => {{
        let builder = $builder;
        let negotiated = $negotiated;
        let max_turns = $max_turns;

        if negotiated.tools_enabled {
            if negotiated.system_preamble_enabled && !PREMABLE.trim().is_empty() {
                builder
                    .preamble(PREMABLE)
                    .tool(ReadTool)
                    .tool(WriteTool)
                    .tool(SearchFileTool)
                    // 在启用工具的会话里同步开放命令执行能力，避免模型只能“口头承诺会运行测试”。
                    .tool(CommandTool)
                    .default_max_turns(max_turns)
                    .build()
            } else {
                builder
                    .tool(ReadTool)
                    .tool(WriteTool)
                    .tool(SearchFileTool)
                    // 与 preamble 开关保持一致，确保工具集合在不同构建路径下行为一致。
                    .tool(CommandTool)
                    .default_max_turns(max_turns)
                    .build()
            }
        } else if negotiated.system_preamble_enabled && !PREMABLE.trim().is_empty() {
            builder
                .preamble(PREMABLE)
                .default_max_turns(max_turns)
                .build()
        } else {
            builder.default_max_turns(max_turns).build()
        }
    }};
}

/// 大模型提供商类型。
#[derive(Debug, Clone, Copy)]
pub enum Provider {
    OpenAI,
    /// OpenAI 的 Codex 系列模型。
    ///
    /// 之所以单独拆出枚举：
    /// - 便于在配置层用 `provider = "codex"` 显式表达“编码型”模型意图；
    /// - 允许使用独立环境变量（如 `CODEX_API_KEY`）而不影响通用 OpenAI 配置。
    Codex,
    Claude,
    Gemini,
    OpenAIAPI,
}

/// 统一封装已构建的 Agent。
#[derive(Clone)]
pub enum BuiltClient {
    OpenAI(Agent<OpenAIResponsesCompletionModel>),
    /// OpenAI Chat Completions 端点（用于 responses API 不兼容时的降级）。
    OpenAIChat(Agent<OpenAICompletionModel>),
    Codex(Agent<OpenAICompletionModel>),
    Claude(Agent<AnthropicCompletionModel>),
    Gemini(Agent<GeminiCompletionModel>),
    OpenAIAPI(Agent<OpenAICompletionModel>),
}

/// 统一流式事件协议，供上层 UI 做增量渲染与状态提示。
///
/// 事件语义约定：
/// - `Delta`：模型新增文本片段；
/// - `ToolProgress`：工具调用过程中的状态变化；
/// - `Done`：本次流式请求成功结束；
/// - `Error`：本次流式请求失败或被中断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStreamEvent {
    Delta { content: String },
    ToolProgress { message: String },
    Done,
    Error { message: String },
}

impl BuiltClient {
    /// 使用统一入口发送提示词，并返回模型文本响应。
    pub async fn prompt(&self, prompt: String) -> Result<String> {
        match self {
            BuiltClient::OpenAI(client) => client
                .prompt(prompt)
                .await
                .map_err(|error| anyhow!(error.to_string())),
            BuiltClient::OpenAIChat(client) => client
                .prompt(prompt)
                .await
                .map_err(|error| anyhow!(error.to_string())),
            BuiltClient::Codex(client) => client
                .prompt(prompt)
                .await
                .map_err(|error| anyhow!(error.to_string())),
            BuiltClient::Claude(client) => client
                .prompt(prompt)
                .await
                .map_err(|error| anyhow!(error.to_string())),
            BuiltClient::Gemini(client) => client
                .prompt(prompt)
                .await
                .map_err(|error| anyhow!(error.to_string())),
            BuiltClient::OpenAIAPI(client) => client
                .prompt(prompt)
                .await
                .map_err(|error| anyhow!(error.to_string())),
        }
    }

    /// 使用统一入口发送“带历史”的对话请求。
    ///
    /// 为什么要单独提供该接口：
    /// - `prompt` 是单轮请求，不会自动携带以往消息；
    /// - 当前 TUI 需要显式传入历史，才能避免“每次都像新会话”。
    pub async fn chat(&self, prompt: String, history: Vec<Message>) -> Result<String> {
        match self {
            BuiltClient::OpenAI(client) => client
                .chat(prompt, history)
                .await
                .map_err(|error| anyhow!(error.to_string())),
            BuiltClient::OpenAIChat(client) => client
                .chat(prompt, history)
                .await
                .map_err(|error| anyhow!(error.to_string())),
            BuiltClient::Codex(client) => client
                .chat(prompt, history)
                .await
                .map_err(|error| anyhow!(error.to_string())),
            BuiltClient::Claude(client) => client
                .chat(prompt, history)
                .await
                .map_err(|error| anyhow!(error.to_string())),
            BuiltClient::Gemini(client) => client
                .chat(prompt, history)
                .await
                .map_err(|error| anyhow!(error.to_string())),
            BuiltClient::OpenAIAPI(client) => client
                .chat(prompt, history)
                .await
                .map_err(|error| anyhow!(error.to_string())),
        }
    }

    /// 以统一流式事件协议发送多轮对话请求。
    ///
    /// 设计取舍说明：
    /// - 上层只需要消费统一事件，不需要关心不同 provider 的流式细节；
    /// - 通过 `cancellation` 原子标记轮询中断，保证 UI 可随时取消长响应；
    /// - 工具调用的细粒度 delta 并不直接渲染到主输出区，而是归并为 `ToolProgress`，避免噪声淹没正文。
    pub async fn stream_chat(
        &self,
        prompt: String,
        history: Vec<Message>,
        cancellation: &AtomicBool,
        on_event: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<String> {
        match self {
            BuiltClient::OpenAI(client) => {
                stream_chat_with_agent(client, prompt, history, cancellation, on_event).await
            }
            BuiltClient::OpenAIChat(client) => {
                stream_chat_with_agent(client, prompt, history, cancellation, on_event).await
            }
            BuiltClient::Codex(client) => {
                stream_chat_with_agent(client, prompt, history, cancellation, on_event).await
            }
            BuiltClient::Claude(client) => {
                stream_chat_with_agent(client, prompt, history, cancellation, on_event).await
            }
            BuiltClient::Gemini(client) => {
                stream_chat_with_agent(client, prompt, history, cancellation, on_event).await
            }
            BuiltClient::OpenAIAPI(client) => {
                stream_chat_with_agent(client, prompt, history, cancellation, on_event).await
            }
        }
    }
}

/// 执行单次流式 chat，并将 provider 原始流转换为统一事件协议。
async fn stream_chat_with_agent<M>(
    agent: &Agent<M>,
    prompt: String,
    history: Vec<Message>,
    cancellation: &AtomicBool,
    on_event: &mut dyn FnMut(ModelStreamEvent),
) -> Result<String>
where
    M: rig::completion::CompletionModel + 'static,
    M::StreamingResponse: rig::completion::GetTokenUsage + rig::wasm_compat::WasmCompatSend,
{
    // `stream_chat(...).await` 会直接返回多轮流式结果流，而不是 `Result`。
    let mut stream = agent.stream_chat(prompt, history).await;

    let mut aggregated_text = String::new();
    let mut final_response_text: Option<String> = None;

    loop {
        // 这里采用短轮询间隔，是为了在“流式无新 token”时也能及时响应取消请求。
        if cancellation.load(Ordering::Relaxed) {
            return Err(anyhow!("请求已取消"));
        }

        match tokio::time::timeout(Duration::from_millis(120), stream.next()).await {
            Ok(Some(Ok(item))) => match item {
                MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text)) => {
                    aggregated_text.push_str(&text.text);
                    on_event(ModelStreamEvent::Delta { content: text.text });
                }
                MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                    tool_call,
                    ..
                }) => {
                    on_event(ModelStreamEvent::ToolProgress {
                        message: format!("工具执行中：{}", tool_call.function.name),
                    });
                }
                MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                    tool_result,
                    ..
                }) => {
                    let summary = tool_result
                        .content
                        .iter()
                        .find_map(|item| match item {
                            ToolResultContent::Text(text) => Some(text.text.clone()),
                            ToolResultContent::Image(_) => None,
                        })
                        .map(|text| truncate_stream_tool_progress(&text, 120))
                        .unwrap_or_else(|| "工具执行完成（非文本结果）".to_string());
                    on_event(ModelStreamEvent::ToolProgress {
                        message: format!("工具执行完成：{}", summary),
                    });
                }
                MultiTurnStreamItem::FinalResponse(final_response) => {
                    final_response_text = Some(final_response.response().to_string());
                }
                _ => {}
            },
            Ok(Some(Err(error))) => return Err(anyhow!(error.to_string())),
            Ok(None) => break,
            Err(_) => {}
        }
    }

    Ok(final_response_text.unwrap_or(aggregated_text))
}

/// 截断工具进度文案，避免结果过长时淹没主输出。
fn truncate_stream_tool_progress(text: &str, max_chars: usize) -> String {
    let normalized = text.trim();
    if normalized.chars().count() <= max_chars {
        return normalized.to_string();
    }

    let mut shortened = normalized
        .chars()
        .take(max_chars.saturating_sub(2))
        .collect::<String>();
    shortened.push_str("..");
    shortened
}

/// 连接配置：保存 provider、地址、密钥和模型选择。
#[derive(Clone)]
pub struct Connection {
    provider: Provider,
    api_url: String,
    api_key: String,
    agent_select: String,
    support_tools: bool,
    /// agent 默认多轮上限（可配置）。
    ///
    /// - `None` 或 `Some(0)` 会回退到 `DEFAULT_AGENT_MAX_TURNS`；
    /// - 过大值会被裁剪到 `MAX_AGENT_MAX_TURNS`，防止异常配置造成长循环。
    max_turns: Option<usize>,
    /// provider 能力覆盖（可选）。
    ///
    /// 为什么放在 Connection 上：
    /// - 该字段属于“本次连接应如何协商能力”的输入；
    /// - 由配置层（`.order/model.json`）解析后直接传入，避免连接层重复读配置文件。
    capabilities: Option<ProviderCapabilitiesOverride>,
}

impl Connection {
    /// 创建连接配置。
    pub fn new(
        provider: Provider,
        api_url: String,
        api_key: String,
        agent_select: String,
        support_tools: bool,
        max_turns: Option<usize>,
        capabilities: Option<ProviderCapabilitiesOverride>,
    ) -> Self {
        Self {
            provider,
            api_url,
            api_key,
            agent_select,
            support_tools,
            max_turns,
            capabilities,
        }
    }

    /// 获取当前 provider。
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// 获取 API 基础地址。
    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    /// 获取 API 密钥（原始配置值）。
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// 获取模型选择标识。
    pub fn agent_select(&self) -> &str {
        &self.agent_select
    }

    /// 当前模型是否允许调用工具。
    pub fn support_tools(&self) -> bool {
        self.support_tools
    }

    /// 解析并返回当前连接应使用的多轮上限。
    ///
    /// 这里把“缺省、非法、过大”统一归一化，保证最终传给 rig 的值稳定可控。
    fn effective_max_turns(&self) -> usize {
        match self.max_turns {
            Some(value) if value > 0 => value.min(MAX_AGENT_MAX_TURNS),
            _ => DEFAULT_AGENT_MAX_TURNS,
        }
    }

    /// 解析可用 API Key：优先使用连接配置，其次读取环境变量。
    ///
    /// 这里允许传入多个环境变量名，按顺序尝试读取：
    /// - 兼容用户在不同工具链下的习惯命名（例如 Codex 可能使用 `CODEX_API_KEY`）；
    /// - 避免为了“别名 provider”复制一套几乎相同的连接逻辑。
    fn resolve_api_key(&self, env_vars: &[&str]) -> Result<String> {
        if !self.api_key.trim().is_empty() {
            return Ok(self.api_key.trim().to_string());
        }

        for env_var in env_vars {
            if let Ok(value) = std::env::var(env_var)
                && !value.trim().is_empty()
            {
                return Ok(value.trim().to_string());
            }
        }

        // 统一输出更明确的错误信息，方便用户一次性补齐环境变量。
        Err(anyhow!(
            "API Key 未配置。请在配置文件中设置 token 字段，或设置环境变量 {}",
            env_vars.join(" 或 ")
        ))
    }

    /// 规范化用户给定的 API 地址。
    ///
    /// 统一去除尾部斜杠，避免路径拼接出现 `//`。
    fn normalized_api_url(&self) -> Option<String> {
        let trimmed = self.api_url.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.trim_end_matches('/').to_string())
        }
    }

    /// 构建并返回统一客户端枚举（基于协商结果）。
    ///
    /// 之所以把“协商结果”作为入参：
    /// - 能力协商是高层策略，构建 client 只是落地手段；
    /// - 这样可以在运行时降级后重建 client，而不必改写 Connection 本身。
    fn build_client(&self, negotiated: &NegotiatedCapabilities) -> Result<BuiltClient> {
        let custom_base_url = self.normalized_api_url();
        let max_turns = self.effective_max_turns();

        match self.provider {
            Provider::OpenAI => {
                let api_key = self.resolve_api_key(&["OPENAI_API_KEY"])?;
                match negotiated.endpoint {
                    ModelEndpoint::ResponsesApi => {
                        let mut builder = openai::Client::builder().api_key(api_key);
                        if let Some(base_url) = custom_base_url.as_ref() {
                            builder = builder.base_url(base_url);
                        }

                        let client = builder.build()?;
                        let agent = build_agent_with_options!(
                            client.agent(&self.agent_select),
                            negotiated,
                            max_turns
                        );
                        Ok(BuiltClient::OpenAI(agent))
                    }
                    ModelEndpoint::ChatCompletions => {
                        let mut builder = openai::CompletionsClient::builder().api_key(api_key);
                        if let Some(base_url) = custom_base_url.as_ref() {
                            builder = builder.base_url(base_url);
                        }

                        let client = builder.build()?;
                        let agent = build_agent_with_options!(
                            client.agent(&self.agent_select),
                            negotiated,
                            max_turns
                        );
                        Ok(BuiltClient::OpenAIChat(agent))
                    }
                }
            }
            Provider::Codex => {
                // Codex 默认优先读取 `CODEX_API_KEY`，以便与通用 `OPENAI_API_KEY` 并存。
                // 若未设置，则回退到 `OPENAI_API_KEY`，降低迁移成本。
                // 使用 Chat Completions API，兼容性更好。
                let api_key = self.resolve_api_key(&["CODEX_API_KEY", "OPENAI_API_KEY"])?;
                let mut builder = openai::CompletionsClient::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;
                // Codex 的主要价值在于"带工具的编码工作流"，
                // 但仍需尊重能力协商结果，避免网关不兼容导致整次请求失败。
                let agent = build_agent_with_options!(
                    client.agent(&self.agent_select),
                    negotiated,
                    max_turns
                );
                Ok(BuiltClient::Codex(agent))
            }
            Provider::Claude => {
                let api_key = self.resolve_api_key(&["ANTHROPIC_API_KEY"])?;
                let mut builder = anthropic::Client::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;
                let agent = build_agent_with_options!(
                    client.agent(&self.agent_select),
                    negotiated,
                    max_turns
                );
                Ok(BuiltClient::Claude(agent))
            }
            Provider::Gemini => {
                let api_key = self.resolve_api_key(&["GEMINI_API_KEY"])?;
                let mut builder = gemini::Client::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;
                let agent = build_agent_with_options!(
                    client.agent(&self.agent_select),
                    negotiated,
                    max_turns
                );
                Ok(BuiltClient::Gemini(agent))
            }
            Provider::OpenAIAPI => {
                let api_key = self.resolve_api_key(&["OPENAI_API_KEY"])?;
                let mut builder = openai::CompletionsClient::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;
                let agent = build_agent_with_options!(
                    client.agent(&self.agent_select),
                    negotiated,
                    max_turns
                );
                Ok(BuiltClient::OpenAIAPI(agent))
            }
        }
    }

    /// 将运行时错误映射为标准分类。
    ///
    /// 分类时显式传入 endpoint 与请求能力标记，避免只看文案导致误判。
    fn classify_error(
        error: &anyhow::Error,
        negotiated: &NegotiatedCapabilities,
    ) -> ClassifiedError {
        ErrorClassifier::default().classify(
            error,
            negotiated.endpoint,
            RequestFeatureFlags::from_negotiated(negotiated),
        )
    }

    /// 为运行时降级写回构建缓存上下文。
    ///
    /// 这里将分类器输出的置信度带入缓存，便于后续诊断判断该降级是否可靠。
    fn build_runtime_writeback_context(
        classified: &ClassifiedError,
        reason: &str,
    ) -> CapabilityWritebackContext {
        CapabilityWritebackContext::runtime(reason)
            .with_ttl_seconds(RUNTIME_FALLBACK_CACHE_TTL_SECONDS)
            .with_confidence(classified.confidence_hint())
    }

    /// 以“尽力而为”方式写回降级缓存。
    ///
    /// 写回失败时只记录日志，不中断当前请求主流程。
    fn writeback_fallback_cache_best_effort(
        resolver: &CapabilityResolver,
        workspace_root: &Path,
        trace_id: &str,
        provider: Provider,
        api_url: Option<&str>,
        model: &str,
        capabilities: ProviderCapabilities,
        context: &CapabilityWritebackContext,
    ) {
        if let Err(writeback_error) = resolver.writeback_cache_with_context(
            workspace_root,
            provider,
            api_url,
            model,
            capabilities,
            context,
        ) {
            log_event_best_effort(
                workspace_root,
                AgentEvent::ToolCallEnd {
                    ts: ts(),
                    trace_id: trace_id.to_string(),
                    tool: "capability_cache_writeback".to_string(),
                    ok: false,
                    duration_ms: 0,
                    error: Some(writeback_error.to_string()),
                },
            );
        }
    }

    /// 兼容历史单测入口：判断是否应降级关闭 tools。
    #[cfg(test)]
    fn should_retry_without_tools(error: &anyhow::Error) -> bool {
        let classified = ErrorClassifier::default().classify(
            error,
            ModelEndpoint::ChatCompletions,
            RequestFeatureFlags {
                tools_enabled: true,
                stream_enabled: false,
                responses_enabled: false,
            },
        );
        classified.category == super::fallback::ErrorCategory::ToolsUnsupported
    }

    /// 兼容历史单测入口：判断是否应降级关闭 responses API。
    #[cfg(test)]
    fn should_retry_without_responses_api(error: &anyhow::Error) -> bool {
        let classified = ErrorClassifier::default().classify(
            error,
            ModelEndpoint::ResponsesApi,
            RequestFeatureFlags {
                tools_enabled: false,
                stream_enabled: false,
                responses_enabled: true,
            },
        );
        classified.category == super::fallback::ErrorCategory::ResponsesUnsupported
    }

    /// 兼容历史单测入口：判断是否应降级关闭 streaming。
    #[cfg(test)]
    fn should_retry_without_stream(error: &anyhow::Error) -> bool {
        let classified = ErrorClassifier::default().classify(
            error,
            ModelEndpoint::ChatCompletions,
            RequestFeatureFlags {
                tools_enabled: false,
                stream_enabled: true,
                responses_enabled: false,
            },
        );
        classified.category == super::fallback::ErrorCategory::StreamUnsupported
    }

    /// 对外响应接口：发送单轮请求（默认生成 trace_id）。
    pub async fn response(&mut self, prompt: String) -> Result<String> {
        let trace_id = crate::observability::new_trace_id();
        self.response_traced(trace_id, prompt)
            .await
            .map(|value| value.content)
            .map_err(|error| anyhow!(error.to_string()))
    }

    /// 对外响应接口：发送带历史的多轮请求（默认生成 trace_id）。
    pub async fn response_with_history(
        &mut self,
        prompt: String,
        history: Vec<Message>,
    ) -> Result<String> {
        let trace_id = crate::observability::new_trace_id();
        self.response_with_history_traced(trace_id, prompt, history)
            .await
            .map(|value| value.content)
            .map_err(|error| anyhow!(error.to_string()))
    }

    /// 发送单轮请求，并显式指定 trace_id（用于全链路观测）。
    pub async fn response_traced(
        &mut self,
        trace_id: String,
        prompt: String,
    ) -> std::result::Result<TracedModelResponse, TracedModelError> {
        self.response_inner(trace_id, RequestMode::Prompt { prompt })
            .await
    }

    /// 发送带历史的多轮请求，并显式指定 trace_id（用于全链路观测）。
    pub async fn response_with_history_traced(
        &mut self,
        trace_id: String,
        prompt: String,
        history: Vec<Message>,
    ) -> std::result::Result<TracedModelResponse, TracedModelError> {
        self.response_inner(trace_id, RequestMode::Chat { prompt, history })
            .await
    }

    /// 发送带历史的流式请求（显式 trace_id），并通过统一事件协议回调增量内容。
    ///
    /// 兼容策略：
    /// - 若协商结果不启用 streaming，则自动回退到非流式并以单个 `delta + done` 回放；
    /// - 若 streaming 在运行时失败且判断为协议不兼容，则同样回退到非流式；
    /// - 取消请求由上层通过 `cancellation` 原子标记控制，本方法在流式轮询中会及时响应。
    pub async fn response_with_history_streamed_traced<F>(
        &mut self,
        trace_id: String,
        prompt: String,
        history: Vec<Message>,
        cancellation: &AtomicBool,
        mut on_event: F,
    ) -> std::result::Result<TracedModelResponse, TracedModelError>
    where
        F: FnMut(ModelStreamEvent),
    {
        let workspace_root = workspace_root_best_effort();
        let resolver = CapabilityResolver::default();
        let custom_base_url = self.normalized_api_url();
        let model = self.agent_select.clone();

        let negotiated = resolver
            .resolve(
                &workspace_root,
                self.provider,
                custom_base_url.as_deref(),
                &model,
                self.support_tools(),
                self.capabilities.as_ref(),
            )
            .map_err(|error| TracedModelError::new(trace_id.clone(), error))?;

        if !negotiated.stream_enabled {
            let fallback = self
                .response_with_history_as_single_event(trace_id, prompt, history, &mut on_event)
                .await;
            // 非流式回退失败时也要补发统一 `error` 事件，
            // 避免上层在“回退分支”丢失错误状态提示。
            if let Err(error) = &fallback {
                on_event(ModelStreamEvent::Error {
                    message: error.to_string(),
                });
            }
            return fallback;
        }

        let stream_result: Result<String> = with_trace_id(trace_id.clone(), async {
            let client = self.build_client(&negotiated)?;
            client
                .stream_chat(prompt.clone(), history.clone(), cancellation, &mut on_event)
                .await
        })
        .await;

        match stream_result {
            Ok(content) => {
                on_event(ModelStreamEvent::Done);
                Ok(TracedModelResponse { trace_id, content })
            }
            Err(error) => {
                let classified = Self::classify_error(&error, &negotiated);
                log_event_best_effort(
                    &workspace_root,
                    AgentEvent::ErrorClassified {
                        ts: ts(),
                        trace_id: trace_id.clone(),
                        category: classified.category.as_str().to_string(),
                        status_code: classified.status_code,
                        provider_error_code: classified.provider_error_code.clone(),
                        endpoint: negotiated.endpoint.as_str().to_string(),
                        tools: negotiated.tools_enabled,
                        stream: negotiated.stream_enabled,
                        responses: negotiated.endpoint == ModelEndpoint::ResponsesApi,
                        degradable: classified.is_degradable(),
                        summary: classified.summary.clone(),
                    },
                );

                let mut plan = CapabilityFallbackPlan::new(1, 2);
                if let Some(step) = plan.next_step(&negotiated, &classified) {
                    let downgraded = step.apply_to(&negotiated);
                    log_event_best_effort(
                        &workspace_root,
                        AgentEvent::RetryScheduled {
                            ts: ts(),
                            trace_id: trace_id.clone(),
                            attempt: 2,
                            reason: format!("{}:{}", classified.category.as_str(), step.reason),
                        },
                    );

                    Self::writeback_fallback_cache_best_effort(
                        &resolver,
                        &workspace_root,
                        &trace_id,
                        self.provider,
                        custom_base_url.as_deref(),
                        &model,
                        downgraded.provider_capabilities,
                        &Self::build_runtime_writeback_context(&classified, step.reason),
                    );

                    log_event_best_effort(
                        &workspace_root,
                        AgentEvent::FallbackApplied {
                            ts: ts(),
                            trace_id: trace_id.clone(),
                            reason: format!("{}:{}", classified.category.as_str(), step.reason),
                            from_endpoint: negotiated.endpoint.as_str().to_string(),
                            to_endpoint: downgraded.endpoint.as_str().to_string(),
                            tools_from: negotiated.tools_enabled,
                            tools_to: downgraded.tools_enabled,
                            system_from: negotiated.system_preamble_enabled,
                            system_to: downgraded.system_preamble_enabled,
                        },
                    );

                    let fallback = self
                        .response_with_history_as_single_event(
                            trace_id,
                            prompt,
                            history,
                            &mut on_event,
                        )
                        .await;
                    // streaming 不兼容后的降级调用若再次失败，同样需要发出 `error` 事件。
                    if let Err(fallback_error) = &fallback {
                        on_event(ModelStreamEvent::Error {
                            message: fallback_error.to_string(),
                        });
                    }
                    return fallback;
                }

                on_event(ModelStreamEvent::Error {
                    message: error.to_string(),
                });
                Err(TracedModelError::new(trace_id, error))
            }
        }
    }

    /// 非流式回退路径：复用既有请求链路，并按统一事件协议回放一次完整输出。
    async fn response_with_history_as_single_event(
        &mut self,
        trace_id: String,
        prompt: String,
        history: Vec<Message>,
        on_event: &mut dyn FnMut(ModelStreamEvent),
    ) -> std::result::Result<TracedModelResponse, TracedModelError> {
        let response = self
            .response_with_history_traced(trace_id.clone(), prompt, history)
            .await?;

        if !response.content.is_empty() {
            on_event(ModelStreamEvent::Delta {
                content: response.content.clone(),
            });
        }
        on_event(ModelStreamEvent::Done);
        Ok(response)
    }

    async fn response_inner(
        &mut self,
        trace_id: String,
        mode: RequestMode,
    ) -> std::result::Result<TracedModelResponse, TracedModelError> {
        // 这里按请求重建 client，而不是跨请求缓存。
        //
        // 原因：当启用 tools 时，rig 内部 ToolServer 通过 `tokio::spawn` 绑定在当前 runtime；
        // 如果在“每次请求新建 runtime”的上层调用模式下复用旧 client，第二轮很容易出现
        // `Failed to get tool definitions`（旧 runtime 已结束，tool handle 失效）。
        let workspace_root = workspace_root_best_effort();
        let start_at = Instant::now();
        let resolver = CapabilityResolver::default();
        let custom_base_url = self.normalized_api_url();
        let model = self.agent_select.clone();

        let negotiated = resolver
            .resolve(
                &workspace_root,
                self.provider,
                custom_base_url.as_deref(),
                &model,
                self.support_tools(),
                self.capabilities.as_ref(),
            )
            .map_err(|error| TracedModelError::new(trace_id.clone(), error))?;

        log_event_best_effort(
            &workspace_root,
            AgentEvent::RequestStart {
                ts: ts(),
                trace_id: trace_id.clone(),
                provider: format!("{:?}", self.provider),
                model: model.clone(),
                endpoint: negotiated.endpoint.as_str().to_string(),
                tools: negotiated.tools_enabled,
                system_preamble: negotiated.system_preamble_enabled,
                capability_sources: negotiated.sources.clone(),
            },
        );

        // 尝试执行请求，并在必要时由“显式降级状态机”驱动后续重试。
        let mut attempts: u32 = 1;
        let mut current = negotiated;
        let mut last_error: Option<anyhow::Error> = None;
        let mut plan =
            CapabilityFallbackPlan::new(MAX_CAPABILITY_FALLBACK_STEPS, MAX_CAPABILITY_ATTEMPTS);

        for attempt in 1..=plan.max_attempts() {
            attempts = attempt;

            let call_result: Result<String> = with_trace_id(trace_id.clone(), async {
                let client = self.build_client(&current)?;
                match &mode {
                    RequestMode::Prompt { prompt } => client.prompt(prompt.clone()).await,
                    RequestMode::Chat { prompt, history } => {
                        client.chat(prompt.clone(), history.clone()).await
                    }
                }
            })
            .await;

            match call_result {
                Ok(content) => {
                    let duration_ms = start_at.elapsed().as_millis();
                    log_event_best_effort(
                        &workspace_root,
                        AgentEvent::RequestEnd {
                            ts: ts(),
                            trace_id: trace_id.clone(),
                            ok: true,
                            duration_ms,
                            attempts,
                            endpoint: current.endpoint.as_str().to_string(),
                            tools: current.tools_enabled,
                            system_preamble: current.system_preamble_enabled,
                            error: None,
                        },
                    );
                    return Ok(TracedModelResponse { trace_id, content });
                }
                Err(error) => {
                    let classified = Self::classify_error(&error, &current);
                    log_event_best_effort(
                        &workspace_root,
                        AgentEvent::ErrorClassified {
                            ts: ts(),
                            trace_id: trace_id.clone(),
                            category: classified.category.as_str().to_string(),
                            status_code: classified.status_code,
                            provider_error_code: classified.provider_error_code.clone(),
                            endpoint: current.endpoint.as_str().to_string(),
                            tools: current.tools_enabled,
                            stream: current.stream_enabled,
                            responses: current.endpoint == ModelEndpoint::ResponsesApi,
                            degradable: classified.is_degradable(),
                            summary: classified.summary.clone(),
                        },
                    );
                    last_error = Some(error);

                    // 不可降级错误（鉴权、限流、参数错误等）直接失败，避免无意义重试。
                    if attempt >= plan.max_attempts() {
                        break;
                    }

                    let Some(step) = plan.next_step(&current, &classified) else {
                        break;
                    };

                    log_event_best_effort(
                        &workspace_root,
                        AgentEvent::RetryScheduled {
                            ts: ts(),
                            trace_id: trace_id.clone(),
                            attempt: attempt + 1,
                            reason: format!("{}:{}", classified.category.as_str(), step.reason),
                        },
                    );

                    let from = current.clone();
                    current = step.apply_to(&from);

                    // 写回缓存，确保同一 provider 后续请求不再重复踩坑。
                    Self::writeback_fallback_cache_best_effort(
                        &resolver,
                        &workspace_root,
                        &trace_id,
                        self.provider,
                        custom_base_url.as_deref(),
                        &model,
                        current.provider_capabilities,
                        &Self::build_runtime_writeback_context(&classified, step.reason),
                    );

                    log_event_best_effort(
                        &workspace_root,
                        AgentEvent::FallbackApplied {
                            ts: ts(),
                            trace_id: trace_id.clone(),
                            reason: format!("{}:{}", classified.category.as_str(), step.reason),
                            from_endpoint: from.endpoint.as_str().to_string(),
                            to_endpoint: current.endpoint.as_str().to_string(),
                            tools_from: from.tools_enabled,
                            tools_to: current.tools_enabled,
                            system_from: from.system_preamble_enabled,
                            system_to: current.system_preamble_enabled,
                        },
                    );
                }
            }
        }

        let duration_ms = start_at.elapsed().as_millis();
        let error = last_error.unwrap_or_else(|| anyhow!("未知错误（未捕获到 error 对象）"));

        log_event_best_effort(
            &workspace_root,
            AgentEvent::RequestEnd {
                ts: ts(),
                trace_id: trace_id.clone(),
                ok: false,
                duration_ms,
                attempts,
                endpoint: current.endpoint.as_str().to_string(),
                tools: current.tools_enabled,
                system_preamble: current.system_preamble_enabled,
                error: Some(error.to_string()),
            },
        );

        if attempts > 1 {
            log_event_best_effort(
                &workspace_root,
                AgentEvent::RetryExhausted {
                    ts: ts(),
                    trace_id: trace_id.clone(),
                    attempts,
                    last_error: error.to_string(),
                },
            );
        }

        Err(TracedModelError::new(trace_id, error))
    }
}

enum RequestMode {
    Prompt {
        prompt: String,
    },
    Chat {
        prompt: String,
        history: Vec<Message>,
    },
}

/// 带 trace_id 的响应结果。
#[derive(Debug, Clone)]
pub struct TracedModelResponse {
    pub trace_id: String,
    pub content: String,
}

/// 带 trace_id 的错误包装。
///
/// 设计目的：
/// - 让上层 UI 在展示错误时能直接给出 trace_id；
/// - 用户可用 trace_id 快速在 `.order/logs/` 中定位同一次失败的详细链路。
#[derive(Debug)]
pub struct TracedModelError {
    trace_id: String,
    error: anyhow::Error,
}

impl TracedModelError {
    fn new(trace_id: String, error: anyhow::Error) -> Self {
        Self { trace_id, error }
    }

    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }
}

impl std::fmt::Display for TracedModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[trace_id={}] {}", self.trace_id, self.error)
    }
}

impl std::error::Error for TracedModelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::SystemTime};

    use anyhow::anyhow;

    use super::{
        BuiltClient, Connection, DEFAULT_AGENT_MAX_TURNS, ModelEndpoint, NegotiatedCapabilities,
        Provider,
    };
    use crate::model::capabilities::{
        CapabilityResolver, CapabilityWritebackContext, ProviderCapabilities,
        ProviderCapabilitiesOverride,
    };

    #[test]
    fn should_retry_without_tools_when_error_mentions_tool_definitions() {
        let error = anyhow!("RequestError: Failed to get tool definitions");
        assert!(Connection::should_retry_without_tools(&error));
    }

    #[test]
    fn should_not_retry_without_tools_for_unrelated_error() {
        let error = anyhow!("HttpError: 401 Unauthorized");
        assert!(!Connection::should_retry_without_tools(&error));
    }

    #[test]
    fn should_retry_without_responses_for_responses_endpoint_error() {
        let error = anyhow!("404 Not Found: unknown endpoint /v1/responses");
        assert!(Connection::should_retry_without_responses_api(&error));
    }

    #[test]
    fn should_not_retry_without_responses_for_unrelated_error() {
        let error = anyhow!("401 Unauthorized");
        assert!(!Connection::should_retry_without_responses_api(&error));
    }

    #[test]
    fn should_retry_without_stream_when_error_indicates_stream_unsupported() {
        let error = anyhow!("400 Bad Request: streaming is not supported by current endpoint");
        assert!(Connection::should_retry_without_stream(&error));
    }

    #[test]
    fn should_not_retry_without_stream_for_unrelated_error() {
        let error = anyhow!("401 Unauthorized");
        assert!(!Connection::should_retry_without_stream(&error));
    }

    #[test]
    fn build_codex_client_should_apply_default_max_turns() {
        // 这里使用本地构建路径做回归校验，确保不会因为 rig 默认值变更再次退化为 0。
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should be buildable");
        let _guard = runtime.enter();

        let connection = Connection::new(
            Provider::Codex,
            String::new(),
            "test-key".to_string(),
            "gpt-5.3-codex".to_string(),
            true,
            None,
            None,
        );
        let negotiated = NegotiatedCapabilities {
            provider_capabilities: ProviderCapabilities {
                supports_tools: true,
                supports_system_preamble: true,
                supports_responses_api: false,
                supports_stream: true,
            },
            tools_enabled: true,
            system_preamble_enabled: true,
            endpoint: ModelEndpoint::ChatCompletions,
            stream_enabled: true,
            sources: vec!["test".to_string()],
        };

        let built = connection
            .build_client(&negotiated)
            .expect("codex client should be buildable");
        match built {
            BuiltClient::Codex(agent) => {
                assert_eq!(agent.default_max_turns, Some(DEFAULT_AGENT_MAX_TURNS));
            }
            _ => panic!("expected codex client"),
        }
    }

    #[test]
    fn build_codex_client_should_apply_configured_max_turns() {
        // 配置值应覆盖默认值，便于用户按模型/网关特性调整多轮上限。
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should be buildable");
        let _guard = runtime.enter();

        let connection = Connection::new(
            Provider::Codex,
            String::new(),
            "test-key".to_string(),
            "gpt-5.3-codex".to_string(),
            true,
            Some(24),
            None,
        );
        let negotiated = NegotiatedCapabilities {
            provider_capabilities: ProviderCapabilities {
                supports_tools: true,
                supports_system_preamble: true,
                supports_responses_api: false,
                supports_stream: true,
            },
            tools_enabled: true,
            system_preamble_enabled: true,
            endpoint: ModelEndpoint::ChatCompletions,
            stream_enabled: true,
            sources: vec!["test".to_string()],
        };

        let built = connection
            .build_client(&negotiated)
            .expect("codex client should be buildable");
        match built {
            BuiltClient::Codex(agent) => {
                assert_eq!(agent.default_max_turns, Some(24));
            }
            _ => panic!("expected codex client"),
        }
    }

    #[test]
    fn capability_cache_writeback_failure_should_not_break_flow() {
        // 这里故意把“工作区根路径”指向一个普通文件，触发缓存写回失败分支。
        // 验证目标是：写回失败只记录日志，不影响后续降级流程继续执行。
        let stamp = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_root = std::env::temp_dir().join(format!(
            "order-connection-writeback-fail-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&temp_root).expect("temp directory should be created");
        let fake_workspace_root = temp_root.join("root_is_a_file");
        fs::write(&fake_workspace_root, "x").expect("temp marker file should be created");

        let resolver = CapabilityResolver::default();
        let downgraded_caps = ProviderCapabilities {
            supports_tools: false,
            supports_system_preamble: true,
            supports_responses_api: false,
            supports_stream: true,
        };
        Connection::writeback_fallback_cache_best_effort(
            &resolver,
            &fake_workspace_root,
            "trace-test",
            Provider::OpenAI,
            None,
            "gpt-test",
            downgraded_caps,
            &CapabilityWritebackContext::runtime("tools_not_supported"),
        );

        // 若主流程未被中断，后续降级能力仍可继续应用。
        let applied = ProviderCapabilities {
            supports_tools: true,
            supports_system_preamble: true,
            supports_responses_api: true,
            supports_stream: true,
        }
        .downgrade(ProviderCapabilitiesOverride {
            supports_tools: Some(false),
            ..Default::default()
        });
        assert!(!applied.supports_tools);

        let _ = fs::remove_dir_all(temp_root);
    }
}
