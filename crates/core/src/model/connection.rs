use std::{
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

use super::capabilities::{
    CapabilityResolver, ModelEndpoint, NegotiatedCapabilities, ProviderCapabilitiesOverride,
};
use crate::observability::{
    AgentEvent, log_event_best_effort, ts, with_trace_id, workspace_root_best_effort,
};
use crate::tool::{read::ReadTool, search_file::SearchFileTool, write::WriteTool};

/// 默认系统提示。
///
/// 当前保持为空，后续可按 provider 细化。
pub const PREMABLE: &str = "";

/// 将协商结果落地到 rig 的 agent builder（宏形式避免引入具体 builder 类型）。
///
/// 为什么用宏而不是函数：
/// - rig 的 builder 类型在不同 client/feature 下可能包含泛型或生命周期参数；
/// - 使用宏可以直接在调用点做链式调用，减少类型推断/签名耦合带来的编译脆弱性。
macro_rules! build_agent_with_options {
    ($builder:expr, $negotiated:expr) => {{
        let builder = $builder;
        let negotiated = $negotiated;

        if negotiated.tools_enabled {
            if negotiated.system_preamble_enabled && !PREMABLE.trim().is_empty() {
                builder
                    .preamble(PREMABLE)
                    .tool(ReadTool)
                    .tool(WriteTool)
                    .tool(SearchFileTool)
                    .build()
            } else {
                builder
                    .tool(ReadTool)
                    .tool(WriteTool)
                    .tool(SearchFileTool)
                    .build()
            }
        } else if negotiated.system_preamble_enabled && !PREMABLE.trim().is_empty() {
            builder.preamble(PREMABLE).build()
        } else {
            builder.build()
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
        capabilities: Option<ProviderCapabilitiesOverride>,
    ) -> Self {
        Self {
            provider,
            api_url,
            api_key,
            agent_select,
            support_tools,
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
                        let agent =
                            build_agent_with_options!(client.agent(&self.agent_select), negotiated);
                        Ok(BuiltClient::OpenAI(agent))
                    }
                    ModelEndpoint::ChatCompletions => {
                        let mut builder = openai::CompletionsClient::builder().api_key(api_key);
                        if let Some(base_url) = custom_base_url.as_ref() {
                            builder = builder.base_url(base_url);
                        }

                        let client = builder.build()?;
                        let agent =
                            build_agent_with_options!(client.agent(&self.agent_select), negotiated);
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
                let agent = build_agent_with_options!(client.agent(&self.agent_select), negotiated);
                Ok(BuiltClient::Codex(agent))
            }
            Provider::Claude => {
                let api_key = self.resolve_api_key(&["ANTHROPIC_API_KEY"])?;
                let mut builder = anthropic::Client::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;
                let agent = build_agent_with_options!(client.agent(&self.agent_select), negotiated);
                Ok(BuiltClient::Claude(agent))
            }
            Provider::Gemini => {
                let api_key = self.resolve_api_key(&["GEMINI_API_KEY"])?;
                let mut builder = gemini::Client::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;
                let agent = build_agent_with_options!(client.agent(&self.agent_select), negotiated);
                Ok(BuiltClient::Gemini(agent))
            }
            Provider::OpenAIAPI => {
                let api_key = self.resolve_api_key(&["OPENAI_API_KEY"])?;
                let mut builder = openai::CompletionsClient::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;
                let agent = build_agent_with_options!(client.agent(&self.agent_select), negotiated);
                Ok(BuiltClient::OpenAIAPI(agent))
            }
        }
    }

    /// 判断错误是否属于“工具定义不可用”类型。
    ///
    /// 之所以采用文本匹配而不是错误类型匹配：
    /// - 不同 provider/网关返回的错误类型并不统一；
    /// - 统一按关键语义降级，能覆盖更多兼容实现。
    fn should_retry_without_tools(error: &anyhow::Error) -> bool {
        let normalized = error.to_string().to_ascii_lowercase();
        normalized.contains("failed to get tool definitions")
            || normalized.contains("tool definitions")
            || normalized.contains("tool definition")
    }

    /// 判断错误是否属于“responses API 不兼容”类型。
    ///
    /// 仍采用文本匹配的原因与 tools 类似：不同网关的错误格式差异很大。
    fn should_retry_without_responses_api(error: &anyhow::Error) -> bool {
        let normalized = error.to_string().to_ascii_lowercase();
        // 典型场景：网关不支持 `/responses` 端点，返回 404/unknown endpoint。
        (normalized.contains("responses") && normalized.contains("404"))
            || normalized.contains("unknown endpoint")
            || normalized.contains("not found") && normalized.contains("responses")
    }

    /// 判断错误是否属于“streaming 不兼容”类型。
    ///
    /// 这里仍使用关键字匹配而不是类型匹配，原因与其它降级规则一致：
    /// 不同 provider/网关返回的错误结构差异较大，需要保持宽松兼容。
    fn should_retry_without_stream(error: &anyhow::Error) -> bool {
        let normalized = error.to_string().to_ascii_lowercase();
        let mentions_stream = normalized.contains("stream")
            || normalized.contains("streaming")
            || normalized.contains("sse");
        let looks_unsupported = normalized.contains("not support")
            || normalized.contains("unsupported")
            || normalized.contains("unknown")
            || normalized.contains("invalid")
            || normalized.contains("not found")
            || normalized.contains("400")
            || normalized.contains("404");
        mentions_stream && looks_unsupported
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
            return self
                .response_with_history_as_single_event(trace_id, prompt, history, &mut on_event)
                .await;
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
            Err(error) if Self::should_retry_without_stream(&error) => {
                self.response_with_history_as_single_event(trace_id, prompt, history, &mut on_event)
                    .await
            }
            Err(error) => {
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

        // 尝试执行请求，并在必要时降级重试一次。
        let mut attempts: u32 = 1;
        let mut current = negotiated;
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 1..=2 {
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
                    last_error = Some(error);
                }
            }

            let Some(error) = last_error.as_ref() else {
                break;
            };

            // 只允许一次降级重试，避免无限循环。
            if attempt >= 2 {
                break;
            }

            // 运行时降级策略：优先处理 tools，不行再处理 responses API。
            let mut downgrade: Option<(ProviderCapabilitiesOverride, &'static str)> = None;
            if current.tools_enabled && Self::should_retry_without_tools(error) {
                downgrade = Some((
                    ProviderCapabilitiesOverride {
                        supports_tools: Some(false),
                        ..Default::default()
                    },
                    "tools_not_supported",
                ));
            } else if current.endpoint == ModelEndpoint::ResponsesApi
                && Self::should_retry_without_responses_api(error)
            {
                downgrade = Some((
                    ProviderCapabilitiesOverride {
                        supports_responses_api: Some(false),
                        ..Default::default()
                    },
                    "responses_api_not_supported",
                ));
            }

            let Some((downgrade_override, reason)) = downgrade else {
                break;
            };

            log_event_best_effort(
                &workspace_root,
                AgentEvent::RetryScheduled {
                    ts: ts(),
                    trace_id: trace_id.clone(),
                    attempt: attempt + 1,
                    reason: reason.to_string(),
                },
            );

            let from = current.clone();
            let downgraded_provider_caps = from
                .provider_capabilities
                .downgrade(downgrade_override.clone());

            // 写回缓存，确保同一 provider 后续请求不再重复踩坑。
            if let Err(writeback_error) = resolver.writeback_cache(
                &workspace_root,
                self.provider,
                custom_base_url.as_deref(),
                &model,
                downgraded_provider_caps,
            ) {
                // 缓存写回失败不应影响本次请求的降级重试。
                log_event_best_effort(
                    &workspace_root,
                    AgentEvent::ToolCallEnd {
                        ts: ts(),
                        trace_id: trace_id.clone(),
                        tool: "capability_cache_writeback".to_string(),
                        ok: false,
                        duration_ms: 0,
                        error: Some(writeback_error.to_string()),
                    },
                );
            }

            // 本次重试直接使用降级后的能力，避免被配置覆盖重新打开导致重复失败。
            current = NegotiatedCapabilities {
                provider_capabilities: downgraded_provider_caps,
                tools_enabled: from.tools_enabled
                    && downgrade_override.supports_tools != Some(false),
                system_preamble_enabled: from.system_preamble_enabled
                    && downgrade_override.supports_system_preamble != Some(false),
                endpoint: if downgrade_override.supports_responses_api == Some(false) {
                    ModelEndpoint::ChatCompletions
                } else {
                    from.endpoint
                },
                stream_enabled: from.stream_enabled
                    && downgrade_override.supports_stream != Some(false),
                // 运行时降级属于新的来源标签，便于后续排查。
                sources: {
                    let mut tags = from.sources.clone();
                    tags.push(format!("runtime:{reason}"));
                    tags
                },
            };

            log_event_best_effort(
                &workspace_root,
                AgentEvent::FallbackApplied {
                    ts: ts(),
                    trace_id: trace_id.clone(),
                    reason: reason.to_string(),
                    from_endpoint: from.endpoint.as_str().to_string(),
                    to_endpoint: current.endpoint.as_str().to_string(),
                    tools_from: from.tools_enabled,
                    tools_to: current.tools_enabled,
                    system_from: from.system_preamble_enabled,
                    system_to: current.system_preamble_enabled,
                },
            );
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

        if attempts >= 2 {
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
    use anyhow::anyhow;

    use super::Connection;

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
    fn should_retry_without_stream_when_error_indicates_stream_unsupported() {
        let error = anyhow!("400 Bad Request: streaming is not supported by current endpoint");
        assert!(Connection::should_retry_without_stream(&error));
    }

    #[test]
    fn should_not_retry_without_stream_for_unrelated_error() {
        let error = anyhow!("401 Unauthorized");
        assert!(!Connection::should_retry_without_stream(&error));
    }
}
