use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::encoding::{read_utf8_text_with_report, write_utf8_text_with_report};

use super::connection::Provider;

const CACHE_FILE_VERSION: u32 = 2;
const DEFAULT_CACHE_TTL_SECONDS: u64 = 24 * 60 * 60;
const DEFAULT_CACHE_CONFIDENCE: f32 = 0.8;

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
    pub fn source_tags(&self) -> Vec<String> {
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
    /// 是否启用 streaming。
    pub stream_enabled: bool,
    /// 能力来源标签（用于日志解释）。
    pub sources: Vec<String>,
}

/// 能力缓存来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityCacheSource {
    RuntimeWriteback,
    ConfigOverride,
    ManualReset,
    #[default]
    Unknown,
}

impl CapabilityCacheSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityCacheSource::RuntimeWriteback => "runtime_writeback",
            CapabilityCacheSource::ConfigOverride => "config_override",
            CapabilityCacheSource::ManualReset => "manual_reset",
            CapabilityCacheSource::Unknown => "unknown",
        }
    }
}

/// 缓存写回上下文。
///
/// 通过显式上下文字段把“为什么写回、写回可信度和有效期”落盘，
/// 便于 `/status` 与日志直接解释当前降级状态。
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityWritebackContext {
    pub reason: String,
    pub source: CapabilityCacheSource,
    pub ttl_seconds: u64,
    pub confidence: f32,
}

impl CapabilityWritebackContext {
    pub fn runtime(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            source: CapabilityCacheSource::RuntimeWriteback,
            ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
            confidence: DEFAULT_CACHE_CONFIDENCE,
        }
    }

    pub fn with_ttl_seconds(mut self, ttl_seconds: u64) -> Self {
        // TTL 为 0 会导致“每次都视为过期”并反复探测，因此这里最小限制为 1 秒。
        self.ttl_seconds = ttl_seconds.max(1);
        self
    }

    pub fn with_confidence(mut self, confidence: f32) -> Self {
        self.confidence = confidence.clamp(0.0, 1.0);
        self
    }
}

impl Default for CapabilityWritebackContext {
    fn default() -> Self {
        Self::runtime("runtime_fallback")
    }
}

/// 缓存快照（供 `/status` 诊断展示）。
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityCacheSnapshot {
    pub provider: String,
    pub api_url: String,
    pub model: String,
    pub capabilities: ProviderCapabilities,
    pub reason: Option<String>,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub ttl_seconds: u64,
    pub confidence: f32,
    pub source: CapabilityCacheSource,
    pub expired: bool,
    pub expires_at: Option<String>,
    pub remaining_ttl_seconds: Option<u64>,
}

/// 能力缓存文件。
///
/// 文件位置：`.order/capabilities.json`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CapabilityCacheFile {
    #[serde(default = "cache_file_version")]
    version: u32,
    #[serde(default)]
    entries: Vec<CapabilityCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CapabilityCacheEntry {
    provider: String,
    api_url: String,
    model: String,
    capabilities: ProviderCapabilities,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    first_seen_at: Option<String>,
    #[serde(default)]
    last_seen_at: Option<String>,
    #[serde(default)]
    ttl: Option<u64>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    source: Option<CapabilityCacheSource>,
    /// 兼容 v1 字段：当新字段缺失时回退使用 `updated_at`。
    #[serde(default)]
    updated_at: Option<String>,
}

impl CapabilityCacheEntry {
    fn matches(&self, provider: &str, api_url: &str, model: &str) -> bool {
        self.provider.eq_ignore_ascii_case(provider)
            && self.api_url.eq_ignore_ascii_case(api_url)
            && self.model.eq_ignore_ascii_case(model)
    }

    fn effective_ttl_seconds(&self) -> u64 {
        self.ttl
            .unwrap_or(DEFAULT_CACHE_TTL_SECONDS)
            .max(1)
            .min(365 * 24 * 60 * 60)
    }

    fn effective_confidence(&self) -> f32 {
        self.confidence
            .unwrap_or(DEFAULT_CACHE_CONFIDENCE)
            .clamp(0.0, 1.0)
    }

    fn effective_source(&self) -> CapabilityCacheSource {
        self.source.unwrap_or_default()
    }

    fn effective_first_seen_text(&self, now_text: &str) -> String {
        self.first_seen_at
            .clone()
            .or_else(|| self.updated_at.clone())
            .or_else(|| self.last_seen_at.clone())
            .unwrap_or_else(|| now_text.to_string())
    }

    fn effective_last_seen_text(&self, now_text: &str) -> String {
        self.last_seen_at
            .clone()
            .or_else(|| self.updated_at.clone())
            .or_else(|| self.first_seen_at.clone())
            .unwrap_or_else(|| now_text.to_string())
    }

    fn parse_utc(text: &str) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|value| value.with_timezone(&Utc))
    }

    fn to_snapshot(&self, now: DateTime<Utc>) -> CapabilityCacheSnapshot {
        let now_text = now.to_rfc3339();
        let first_seen_at = self.effective_first_seen_text(&now_text);
        let last_seen_at = self.effective_last_seen_text(&now_text);
        let ttl_seconds = self.effective_ttl_seconds();
        let source = self.effective_source();
        let confidence = self.effective_confidence();

        let expires_at_dt = Self::parse_utc(&last_seen_at)
            .map(|last_seen| last_seen + ChronoDuration::seconds(ttl_seconds as i64));

        let expired = expires_at_dt.is_some_and(|expires_at| expires_at <= now);
        let expires_at = expires_at_dt.map(|value| value.to_rfc3339());
        let remaining_ttl_seconds = expires_at_dt
            .and_then(|expires_at| (expires_at - now).num_seconds().try_into().ok())
            .filter(|value: &u64| *value > 0);

        CapabilityCacheSnapshot {
            provider: self.provider.clone(),
            api_url: self.api_url.clone(),
            model: self.model.clone(),
            capabilities: self.capabilities,
            reason: self.reason.clone(),
            first_seen_at,
            last_seen_at,
            ttl_seconds,
            confidence,
            source,
            expired,
            expires_at,
            remaining_ttl_seconds,
        }
    }
}

impl CapabilityCacheFile {
    fn get_snapshot(
        &self,
        provider: &str,
        api_url: &str,
        model: &str,
        now: DateTime<Utc>,
    ) -> Option<CapabilityCacheSnapshot> {
        self.entries
            .iter()
            .find(|entry| entry.matches(provider, api_url, model))
            .map(|entry| entry.to_snapshot(now))
    }

    fn get_active_capabilities(
        &self,
        provider: &str,
        api_url: &str,
        model: &str,
        now: DateTime<Utc>,
    ) -> Option<ProviderCapabilities> {
        let snapshot = self.get_snapshot(provider, api_url, model, now)?;
        if snapshot.expired {
            None
        } else {
            Some(snapshot.capabilities)
        }
    }

    fn upsert(
        &mut self,
        provider: &str,
        api_url: &str,
        model: &str,
        capabilities: ProviderCapabilities,
        context: &CapabilityWritebackContext,
    ) {
        let now = Local::now().to_rfc3339();
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|entry| entry.matches(provider, api_url, model))
        {
            let first_seen = existing
                .first_seen_at
                .clone()
                .or_else(|| existing.updated_at.clone())
                .or_else(|| existing.last_seen_at.clone())
                .unwrap_or_else(|| now.clone());

            existing.capabilities = capabilities;
            existing.reason = Some(context.reason.clone());
            existing.first_seen_at = Some(first_seen);
            existing.last_seen_at = Some(now.clone());
            existing.ttl = Some(context.ttl_seconds.max(1));
            existing.confidence = Some(context.confidence.clamp(0.0, 1.0));
            existing.source = Some(context.source);
            existing.updated_at = Some(now);
            return;
        }

        self.entries.push(CapabilityCacheEntry {
            provider: provider.to_string(),
            api_url: api_url.to_string(),
            model: model.to_string(),
            capabilities,
            reason: Some(context.reason.clone()),
            first_seen_at: Some(now.clone()),
            last_seen_at: Some(now.clone()),
            ttl: Some(context.ttl_seconds.max(1)),
            confidence: Some(context.confidence.clamp(0.0, 1.0)),
            source: Some(context.source),
            updated_at: Some(now),
        });
    }

    fn remove_matching(&mut self, provider: Option<&str>, model: Option<&str>) -> usize {
        let before = self.entries.len();
        self.entries.retain(|entry| {
            let provider_match = provider
                .map(|filter| entry.provider.eq_ignore_ascii_case(filter))
                .unwrap_or(true);
            let model_match = model
                .map(|filter| entry.model.eq_ignore_ascii_case(filter))
                .unwrap_or(true);
            !(provider_match && model_match)
        });
        before.saturating_sub(self.entries.len())
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
            let now = Utc::now();
            if let Some(snapshot) = cache.get_snapshot(provider_name, &normalized_url, model, now) {
                if snapshot.expired {
                    sources.push("cache:expired".to_string());
                } else if let Some(active_caps) =
                    cache.get_active_capabilities(provider_name, &normalized_url, model, now)
                {
                    caps = active_caps;
                    sources.push("cache:active".to_string());
                }
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

    /// 查询缓存快照（包含 TTL 是否过期）。
    pub fn inspect_cache_entry(
        &self,
        workspace_root: &Path,
        provider: Provider,
        api_url: Option<&str>,
        model: &str,
    ) -> Result<Option<CapabilityCacheSnapshot>> {
        let provider_name = provider_name(provider);
        let normalized_url = normalize_api_url(api_url);
        let cache = load_cache_file(workspace_root)?;
        Ok(cache.get_snapshot(provider_name, &normalized_url, model, Utc::now()))
    }

    /// 将“降级后的能力”写入缓存（使用默认上下文）。
    pub fn writeback_cache(
        &self,
        workspace_root: &Path,
        provider: Provider,
        api_url: Option<&str>,
        model: &str,
        capabilities: ProviderCapabilities,
    ) -> Result<()> {
        self.writeback_cache_with_context(
            workspace_root,
            provider,
            api_url,
            model,
            capabilities,
            &CapabilityWritebackContext::default(),
        )
    }

    /// 将“降级后的能力”写入缓存（带元数据上下文）。
    pub fn writeback_cache_with_context(
        &self,
        workspace_root: &Path,
        provider: Provider,
        api_url: Option<&str>,
        model: &str,
        capabilities: ProviderCapabilities,
        context: &CapabilityWritebackContext,
    ) -> Result<()> {
        let provider_name = provider_name(provider);
        let normalized_url = normalize_api_url(api_url);

        let mut cache = load_cache_file(workspace_root).unwrap_or_default();
        cache.upsert(provider_name, &normalized_url, model, capabilities, context);
        save_cache_file(workspace_root, &cache)
    }

    /// 重置能力缓存。
    ///
    /// - `provider` 为空时匹配所有 provider；
    /// - `model` 为空时匹配所有 model；
    /// - 两者都为空时等同于清空整个缓存文件。
    pub fn reset_cache_entries(
        &self,
        workspace_root: &Path,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<usize> {
        let path = cache_file_path(workspace_root);
        if !path.exists() {
            return Ok(0);
        }

        let mut cache = load_cache_file(workspace_root)?;
        let removed = cache.remove_matching(provider, model);
        if removed == 0 {
            return Ok(0);
        }

        save_cache_file(workspace_root, &cache)?;
        Ok(removed)
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

fn cache_file_version() -> u32 {
    CACHE_FILE_VERSION
}

fn cache_file_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".order").join("capabilities.json")
}

fn load_cache_file(workspace_root: &Path) -> Result<CapabilityCacheFile> {
    let path = cache_file_path(workspace_root);
    if !path.exists() {
        return Ok(CapabilityCacheFile {
            version: CACHE_FILE_VERSION,
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
            version: CACHE_FILE_VERSION,
            entries: Vec::new(),
        });
    }

    let mut file: CapabilityCacheFile = serde_json::from_str(&text)
        .with_context(|| format!("解析能力缓存 JSON 失败: {}", path.display()))?;
    if file.version == 0 {
        file.version = CACHE_FILE_VERSION;
    }
    Ok(file)
}

fn save_cache_file(workspace_root: &Path, cache: &CapabilityCacheFile) -> Result<()> {
    let path = cache_file_path(workspace_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建能力缓存目录失败: {}", parent.display()))?;
    }

    // 写回时统一升级版本号，确保新字段可以被后续流程稳定读取。
    let mut normalized = cache.clone();
    normalized.version = CACHE_FILE_VERSION;

    // 统一以 pretty JSON + 尾随换行写入，便于手动查看与减少 diff 噪音。
    let mut text = serde_json::to_string_pretty(&normalized).context("序列化能力缓存 JSON 失败")?;
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

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_workspace(case_name: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "order-capability-{case_name}-{}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp workspace should be created");
        path
    }

    fn cleanup_workspace(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn resolve_should_apply_active_cache() {
        let workspace = temp_workspace("active-cache");
        let resolver = CapabilityResolver::default();

        let downgraded = ProviderCapabilities {
            supports_tools: false,
            supports_system_preamble: true,
            supports_responses_api: false,
            supports_stream: true,
        };
        resolver
            .writeback_cache_with_context(
                &workspace,
                Provider::OpenAI,
                None,
                "gpt-test",
                downgraded,
                &CapabilityWritebackContext::runtime("tools_not_supported")
                    .with_ttl_seconds(24 * 60 * 60)
                    .with_confidence(0.9),
            )
            .expect("writeback should succeed");

        let resolved = resolver
            .resolve(&workspace, Provider::OpenAI, None, "gpt-test", true, None)
            .expect("resolve should succeed");

        assert!(!resolved.tools_enabled);
        assert!(resolved.sources.iter().any(|value| value == "cache:active"));
        cleanup_workspace(&workspace);
    }

    #[test]
    fn resolve_should_ignore_expired_cache_entry() {
        let workspace = temp_workspace("expired-cache");
        let expired_time = (Utc::now() - ChronoDuration::hours(30)).to_rfc3339();
        let cache = CapabilityCacheFile {
            version: CACHE_FILE_VERSION,
            entries: vec![CapabilityCacheEntry {
                provider: "openai".to_string(),
                api_url: String::new(),
                model: "gpt-test".to_string(),
                capabilities: ProviderCapabilities {
                    supports_tools: false,
                    supports_system_preamble: true,
                    supports_responses_api: false,
                    supports_stream: false,
                },
                reason: Some("tools_not_supported".to_string()),
                first_seen_at: Some(expired_time.clone()),
                last_seen_at: Some(expired_time),
                ttl: Some(60),
                confidence: Some(0.9),
                source: Some(CapabilityCacheSource::RuntimeWriteback),
                updated_at: None,
            }],
        };
        save_cache_file(&workspace, &cache).expect("cache should be saved");

        let resolved = CapabilityResolver::default()
            .resolve(&workspace, Provider::OpenAI, None, "gpt-test", true, None)
            .expect("resolve should succeed");

        // OpenAI 默认 tools=true；若缓存过期应回到静态默认，而不是沿用旧降级结果。
        assert!(resolved.tools_enabled);
        assert!(
            resolved
                .sources
                .iter()
                .any(|value| value == "cache:expired")
        );
        cleanup_workspace(&workspace);
    }

    #[test]
    fn reset_cache_entries_should_filter_by_provider_and_model() {
        let workspace = temp_workspace("reset-cache");
        let resolver = CapabilityResolver::default();
        let caps = ProviderCapabilities {
            supports_tools: false,
            supports_system_preamble: true,
            supports_responses_api: false,
            supports_stream: false,
        };

        resolver
            .writeback_cache_with_context(
                &workspace,
                Provider::OpenAI,
                None,
                "model-a",
                caps,
                &CapabilityWritebackContext::runtime("tools_not_supported"),
            )
            .expect("first writeback should succeed");
        resolver
            .writeback_cache_with_context(
                &workspace,
                Provider::Codex,
                None,
                "model-b",
                caps,
                &CapabilityWritebackContext::runtime("tools_not_supported"),
            )
            .expect("second writeback should succeed");

        let removed = resolver
            .reset_cache_entries(&workspace, Some("openai"), Some("model-a"))
            .expect("reset should succeed");
        assert_eq!(removed, 1);

        let openai_entry = resolver
            .inspect_cache_entry(&workspace, Provider::OpenAI, None, "model-a")
            .expect("inspect should succeed");
        assert!(openai_entry.is_none());

        let codex_entry = resolver
            .inspect_cache_entry(&workspace, Provider::Codex, None, "model-b")
            .expect("inspect should succeed");
        assert!(codex_entry.is_some());

        cleanup_workspace(&workspace);
    }
}
