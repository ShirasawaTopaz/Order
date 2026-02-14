use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::encoding::{read_utf8_text_with_report, write_utf8_text_with_report};

use super::connection::Provider;

/// 模型端点类型（用于区分 responses API 与 chat/completions API）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelEndpoint {
    ResponsesApi,
    ChatCompletions,
}

impl ModelEndpoint {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelEndpoint::ResponsesApi => "responses",
            ModelEndpoint::ChatCompletions => "chat_completions",
        }
    }
}

/// Provider 能力模型。
///
/// 该结构描述“provider 是否支持某能力”，不代表是否启用。
/// 是否启用需要再叠加用户配置（例如 `support_tools`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub supports_tools: bool,
    pub supports_system_preamble: bool,
    pub supports_responses_api: bool,
    pub supports_stream: bool,
}

impl ProviderCapabilities {
    /// 将能力降级（只允许从 true -> false）。
    ///
    /// 这样做的原因：
    /// - 运行时错误更可靠地表明“某能力不可用”；
    /// - 反向升级（false -> true）需要显式探测或用户覆盖，否则容易反复踩坑。
    pub fn downgrade(mut self, delta: ProviderCapabilitiesOverride) -> Self {
        if delta.supports_tools == Some(false) {
            self.supports_tools = false;
        }
        if delta.supports_system_preamble == Some(false) {
            self.supports_system_preamble = false;
        }
        if delta.supports_responses_api == Some(false) {
            self.supports_responses_api = false;
        }
        if delta.supports_stream == Some(false) {
            self.supports_stream = false;
        }
        self
    }
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            supports_tools: false,
            supports_system_preamble: true,
            supports_responses_api: false,
            supports_stream: false,
        }
    }
}

/// 配置层覆盖用的能力结构（字段允许缺失）。
///
/// 之所以用 `Option<bool>`：
/// - 便于只覆盖个别能力；
/// - 避免配置文件缺失字段时误覆盖默认能力。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilitiesOverride {
    pub supports_tools: Option<bool>,
    pub supports_system_preamble: Option<bool>,
    pub supports_responses_api: Option<bool>,
    pub supports_stream: Option<bool>,
}

impl ProviderCapabilitiesOverride {
    /// 将覆盖应用到基础能力上（Some 覆盖 None 不变）。
    pub fn apply_to(&self, mut base: ProviderCapabilities) -> ProviderCapabilities {
        if let Some(value) = self.supports_tools {
            base.supports_tools = value;
        }
        if let Some(value) = self.supports_system_preamble {
            base.supports_system_preamble = value;
        }
        if let Some(value) = self.supports_responses_api {
            base.supports_responses_api = value;
        }
        if let Some(value) = self.supports_stream {
            base.supports_stream = value;
        }
        base
    }

    /// 将覆盖转为可读的“源”描述，便于在日志中解释。
    fn source_tags(&self) -> Vec<String> {
        let mut tags = Vec::new();
        if self.supports_tools.is_some() {
            tags.push("config:tools".to_string());
        }
        if self.supports_system_preamble.is_some() {
            tags.push("config:system_preamble".to_string());
        }
        if self.supports_responses_api.is_some() {
            tags.push("config:responses_api".to_string());
        }
        if self.supports_stream.is_some() {
            tags.push("config:stream".to_string());
        }
        tags
    }
}

/// 本次请求最终协商出的“启用策略”。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedCapabilities {
    /// Provider 侧“支持情况”（叠加了静态/缓存/配置覆盖后的结果）。
    pub provider_capabilities: ProviderCapabilities,
    /// 本次是否启用 tools（= user_support_tools && provider_supports_tools）。
    pub tools_enabled: bool,
    /// 本次是否发送 system preamble。
    pub system_preamble_enabled: bool,
    /// 本次使用的端点类型。
    pub endpoint: ModelEndpoint,
    /// 是否启用 streaming（当前实现未启用，仅做能力记录）。
    pub stream_enabled: bool,
    /// 能力来源标签（用于日志解释）。
    pub sources: Vec<String>,
}

/// 能力缓存文件。
///
/// 文件位置：`.order/capabilities.json`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CapabilityCacheFile {
    version: u32,
    entries: Vec<CapabilityCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CapabilityCacheEntry {
    provider: String,
    api_url: String,
    model: String,
    capabilities: ProviderCapabilities,
    updated_at: String,
}

impl CapabilityCacheFile {
    fn get(&self, provider: &str, api_url: &str, model: &str) -> Option<ProviderCapabilities> {
        self.entries.iter().find_map(|entry| {
            if entry.provider.eq_ignore_ascii_case(provider)
                && entry.api_url.eq_ignore_ascii_case(api_url)
                && entry.model.eq_ignore_ascii_case(model)
            {
                Some(entry.capabilities)
            } else {
                None
            }
        })
    }

    fn upsert(&mut self, provider: &str, api_url: &str, model: &str, caps: ProviderCapabilities) {
        let now = Local::now().to_rfc3339();
        if let Some(existing) = self.entries.iter_mut().find(|entry| {
            entry.provider.eq_ignore_ascii_case(provider)
                && entry.api_url.eq_ignore_ascii_case(api_url)
                && entry.model.eq_ignore_ascii_case(model)
        }) {
            existing.capabilities = caps;
            existing.updated_at = now;
            return;
        }

        self.entries.push(CapabilityCacheEntry {
            provider: provider.to_string(),
            api_url: api_url.to_string(),
            model: model.to_string(),
            capabilities: caps,
            updated_at: now,
        });
    }
}

/// 能力解析器：按“静态默认 -> 探测/运行时缓存 -> 配置覆盖”聚合。
#[derive(Debug, Default, Clone)]
pub struct CapabilityResolver;

impl CapabilityResolver {
    /// 解析当前连接应使用的能力与启用策略。
    pub fn resolve(
        &self,
        workspace_root: &Path,
        provider: Provider,
        api_url: Option<&str>,
        model: &str,
        user_support_tools: bool,
        config_override: Option<&ProviderCapabilitiesOverride>,
    ) -> Result<NegotiatedCapabilities> {
        let provider_name = provider_name(provider);
        let normalized_url = normalize_api_url(api_url);

        // 静态默认能力：内置“已知 provider”的常识配置。
        let mut sources = vec!["static".to_string()];
        let mut caps = static_default_capabilities(provider, api_url);

        // 探测/运行时缓存：用于避免“同一坑反复踩”。
        if let Ok(cache) = load_cache_file(workspace_root) {
            if let Some(cached) = cache.get(provider_name, &normalized_url, model) {
                caps = cached;
                sources.push("cache".to_string());
            }
        }

        // 配置覆盖：用户显式声明优先级最高。
        if let Some(override_caps) = config_override {
            caps = override_caps.apply_to(caps);
            sources.extend(override_caps.source_tags());
        }

        // 协商“启用策略”：支持情况 + 用户显式开关。
        let tools_enabled = user_support_tools && caps.supports_tools;
        let system_preamble_enabled = caps.supports_system_preamble;
        let endpoint = match provider {
            Provider::OpenAI if caps.supports_responses_api => ModelEndpoint::ResponsesApi,
            Provider::OpenAI => ModelEndpoint::ChatCompletions,
            _ => ModelEndpoint::ChatCompletions,
        };

        Ok(NegotiatedCapabilities {
            provider_capabilities: caps,
            tools_enabled,
            system_preamble_enabled,
            endpoint,
            stream_enabled: caps.supports_stream,
            sources,
        })
    }

    /// 将“降级后的能力”写入缓存。
    ///
    /// 设计目标：
    /// - 遇到协议不兼容错误后立刻回写；
    /// - 下一次请求直接按降级结果构建 client，避免重复失败。
    pub fn writeback_cache(
        &self,
        workspace_root: &Path,
        provider: Provider,
        api_url: Option<&str>,
        model: &str,
        capabilities: ProviderCapabilities,
    ) -> Result<()> {
        let provider_name = provider_name(provider);
        let normalized_url = normalize_api_url(api_url);

        let mut cache = load_cache_file(workspace_root).unwrap_or_default();
        cache.upsert(provider_name, &normalized_url, model, capabilities);
        save_cache_file(workspace_root, &cache)
    }
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAI => "openai",
        Provider::Codex => "codex",
        Provider::Claude => "claude",
        Provider::Gemini => "gemini",
        Provider::OpenAIAPI => "openaiapi",
    }
}

fn normalize_api_url(api_url: Option<&str>) -> String {
    api_url
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string()
}

/// 静态默认能力表。
///
/// 注意：这里尽量“保守”：
/// - 对自定义 base_url（常见网关/代理）默认关闭 responses/tools；
/// - 这样可以减少首次请求 4xx/协议错误，再通过配置覆盖或缓存探测升级。
fn static_default_capabilities(provider: Provider, api_url: Option<&str>) -> ProviderCapabilities {
    let has_custom_base_url = api_url
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);

    match provider {
        Provider::OpenAI => ProviderCapabilities {
            supports_tools: !has_custom_base_url,
            supports_system_preamble: true,
            supports_responses_api: !has_custom_base_url,
            supports_stream: true,
        },
        Provider::Codex => ProviderCapabilities {
            supports_tools: true,
            supports_system_preamble: true,
            supports_responses_api: false,
            supports_stream: true,
        },
        Provider::OpenAIAPI => ProviderCapabilities {
            supports_tools: false,
            supports_system_preamble: true,
            supports_responses_api: false,
            supports_stream: false,
        },
        Provider::Claude => ProviderCapabilities {
            supports_tools: false,
            supports_system_preamble: true,
            supports_responses_api: false,
            supports_stream: false,
        },
        Provider::Gemini => ProviderCapabilities {
            supports_tools: false,
            supports_system_preamble: true,
            supports_responses_api: false,
            supports_stream: false,
        },
    }
}

fn cache_file_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".order").join("capabilities.json")
}

fn load_cache_file(workspace_root: &Path) -> Result<CapabilityCacheFile> {
    let path = cache_file_path(workspace_root);
    if !path.exists() {
        return Ok(CapabilityCacheFile {
            version: 1,
            entries: Vec::new(),
        });
    }

    let (text, report) = read_utf8_text_with_report(&path)
        .with_context(|| format!("读取能力缓存失败: {}", path.display()))?;
    if report.has_warning() {
        for warning in report.warnings_for(&path) {
            eprintln!("capability cache encoding warning: {warning}");
        }
    }
    if text.trim().is_empty() {
        return Ok(CapabilityCacheFile {
            version: 1,
            entries: Vec::new(),
        });
    }

    let file: CapabilityCacheFile = serde_json::from_str(&text)
        .with_context(|| format!("解析能力缓存 JSON 失败: {}", path.display()))?;
    Ok(file)
}

fn save_cache_file(workspace_root: &Path, cache: &CapabilityCacheFile) -> Result<()> {
    let path = cache_file_path(workspace_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建能力缓存目录失败: {}", parent.display()))?;
    }

    // 统一以 pretty JSON + 尾随换行写入，便于手动查看与减少 diff 噪音。
    let mut text = serde_json::to_string_pretty(cache).context("序列化能力缓存 JSON 失败")?;
    text.push('\n');
    let report = write_utf8_text_with_report(&path, &text)
        .with_context(|| format!("写入能力缓存失败: {}", path.display()))?;
    if report.has_warning() {
        for warning in report.warnings_for(&path) {
            eprintln!("capability cache encoding warning: {warning}");
        }
    }
    Ok(())
}
