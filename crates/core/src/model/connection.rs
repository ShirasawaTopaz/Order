use anyhow::{Result, anyhow};
use rig::{
    agent::Agent,
    client::CompletionClient,
    completion::{Chat, Message, Prompt},
    providers::{
        anthropic::{self, completion::CompletionModel as AnthropicCompletionModel},
        gemini::{self, CompletionModel as GeminiCompletionModel},
        openai::{
            self, completion::CompletionModel as OpenAICompletionModel,
            responses_api::ResponsesCompletionModel as OpenAIResponsesCompletionModel,
        },
    },
};

use crate::tool::{read::ReadTool, search_file::SearchFileTool, write::WriteTool};

/// 默认系统提示。
///
/// 当前保持为空，后续可按 provider 细化。
pub const PREMABLE: &str = "";

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
    Codex(Agent<OpenAICompletionModel>),
    Claude(Agent<AnthropicCompletionModel>),
    Gemini(Agent<GeminiCompletionModel>),
    OpenAIAPI(Agent<OpenAICompletionModel>),
}

impl BuiltClient {
    /// 使用统一入口发送提示词，并返回模型文本响应。
    pub async fn prompt(&self, prompt: String) -> Result<String> {
        match self {
            BuiltClient::OpenAI(client) => client
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
}

/// 连接配置：保存 provider、地址、密钥和模型选择。
#[derive(Clone)]
pub struct Connection {
    provider: Provider,
    api_url: String,
    api_key: String,
    agent_select: String,
    support_tools: bool,
}

impl Connection {
    /// 创建连接配置。
    pub fn new(
        provider: Provider,
        api_url: String,
        api_key: String,
        agent_select: String,
        support_tools: bool,
    ) -> Self {
        Self {
            provider,
            api_url,
            api_key,
            agent_select,
            support_tools,
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

    /// 构建并返回统一客户端枚举。
    pub fn builder(&self) -> Result<BuiltClient> {
        let custom_base_url = self.normalized_api_url();

        match self.provider {
            Provider::OpenAI => {
                let api_key = self.resolve_api_key(&["OPENAI_API_KEY"])?;
                let mut builder = openai::Client::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;

                // 只有当用户显式开启 `support_tools` 时才注入文件工具。
                // 这样做可以避免默认行为具备“写文件”等副作用，降低误用风险。
                let agent = if self.support_tools() {
                    client
                        .agent(&self.agent_select)
                        .preamble(PREMABLE)
                        .tool(ReadTool)
                        .tool(WriteTool)
                        .tool(SearchFileTool)
                        .build()
                } else {
                    client.agent(&self.agent_select).preamble(PREMABLE).build()
                };

                Ok(BuiltClient::OpenAI(agent))
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

                // Codex 的主要价值在于"带工具的编码工作流"，因此当用户开启时自动注入工具集。
                let agent = if self.support_tools() {
                    client
                        .agent(&self.agent_select)
                        .preamble(PREMABLE)
                        .tool(ReadTool)
                        .tool(WriteTool)
                        .tool(SearchFileTool)
                        .build()
                } else {
                    client.agent(&self.agent_select).preamble(PREMABLE).build()
                };

                Ok(BuiltClient::Codex(agent))
            }
            Provider::Claude => {
                let api_key = self.resolve_api_key(&["ANTHROPIC_API_KEY"])?;
                let mut builder = anthropic::Client::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;
                Ok(BuiltClient::Claude(
                    client.agent(&self.agent_select).preamble(PREMABLE).build(),
                ))
            }
            Provider::Gemini => {
                let api_key = self.resolve_api_key(&["GEMINI_API_KEY"])?;
                let mut builder = gemini::Client::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;
                Ok(BuiltClient::Gemini(
                    client.agent(&self.agent_select).preamble(PREMABLE).build(),
                ))
            }
            Provider::OpenAIAPI => {
                let api_key = self.resolve_api_key(&["OPENAI_API_KEY"])?;
                let mut builder = openai::CompletionsClient::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;
                Ok(BuiltClient::OpenAIAPI(
                    client.agent(&self.agent_select).preamble(PREMABLE).build(),
                ))
            }
        }
    }

    /// 对外响应接口：发送单轮请求。
    pub async fn response(&mut self, prompt: String) -> Result<String> {
        // 这里按请求重建客户端，而不是跨请求缓存。
        //
        // 原因：当启用 tools 时，rig 内部 ToolServer 通过 `tokio::spawn` 绑定在当前 runtime；
        // 如果在“每次请求新建 runtime”的上层调用模式下复用旧 client，第二轮很容易出现
        // `Failed to get tool definitions`（旧 runtime 已结束，tool handle 失效）。
        let client = self.builder()?;
        client.prompt(prompt).await
    }

    /// 对外响应接口：发送带历史的多轮请求。
    pub async fn response_with_history(
        &mut self,
        prompt: String,
        history: Vec<Message>,
    ) -> Result<String> {
        // 与 `response` 保持一致：每轮重建，避免 tool server 绑定到已销毁 runtime。
        let client = self.builder()?;
        client.chat(prompt, history).await
    }
}
