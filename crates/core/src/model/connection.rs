use anyhow::{Context, Result, anyhow};
use rig::{
    agent::Agent,
    client::CompletionClient,
    completion::Prompt,
    providers::{
        anthropic::{self, completion::CompletionModel as AnthropicCompletionModel},
        gemini::{self, CompletionModel as GeminiCompletionModel},
        openai::{
            self,
            completion::CompletionModel as OpenAICompletionModel,
            responses_api::ResponsesCompletionModel as OpenAIResponsesCompletionModel,
        },
    },
};

/// 默认系统提示。
///
/// 当前保持为空，后续可按 provider 细化。
pub const PREMABLE: &str = "";

/// 大模型提供商类型。
#[derive(Debug, Clone, Copy)]
pub enum Provider {
    OpenAI,
    Claude,
    Gemini,
    OpenAIAPI,
}

/// 统一封装已构建的 Agent。
#[derive(Clone)]
pub enum BuiltClient {
    OpenAI(Agent<OpenAIResponsesCompletionModel>),
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
}

/// 连接配置：保存 provider、地址、密钥和模型选择。
#[derive(Clone)]
pub struct Connection {
    provider: Provider,
    api_url: String,
    api_key: String,
    agent_select: String,
    client: Option<BuiltClient>,
}

impl Connection {
    /// 创建连接配置。
    pub fn new(provider: Provider, api_url: String, api_key: String, agent_select: String) -> Self {
        Self {
            provider,
            api_url,
            api_key,
            agent_select,
            client: None,
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

    /// 解析可用 API Key：优先使用连接配置，其次读取环境变量。
    fn resolve_api_key(&self, env_var: &str) -> Result<String> {
        if !self.api_key.trim().is_empty() {
            return Ok(self.api_key.trim().to_string());
        }
        std::env::var(env_var).with_context(|| format!("{env_var} 未设置且 connection.api_key 为空"))
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
                let api_key = self.resolve_api_key("OPENAI_API_KEY")?;
                let mut builder = openai::Client::builder().api_key(api_key);
                if let Some(base_url) = custom_base_url.as_ref() {
                    builder = builder.base_url(base_url);
                }

                let client = builder.build()?;
                Ok(BuiltClient::OpenAI(
                    client.agent(&self.agent_select).preamble(PREMABLE).build(),
                ))
            }
            Provider::Claude => {
                let api_key = self.resolve_api_key("ANTHROPIC_API_KEY")?;
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
                let api_key = self.resolve_api_key("GEMINI_API_KEY")?;
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
                let api_key = self.resolve_api_key("OPENAI_API_KEY")?;
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

    /// 对外响应接口：懒加载 client 后发送 prompt。
    pub async fn response(&mut self, prompt: String) -> Result<String> {
        if self.client.is_none() {
            self.client = Some(self.builder()?);
        }

        match self.client.as_ref() {
            Some(client) => client.prompt(prompt).await,
            None => Err(anyhow!("client 未初始化")),
        }
    }
}
