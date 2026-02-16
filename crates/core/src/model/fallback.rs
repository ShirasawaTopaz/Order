use anyhow::Error;
use serde::{Deserialize, Serialize};

use super::capabilities::{ModelEndpoint, NegotiatedCapabilities, ProviderCapabilitiesOverride};

const FALLBACK_REASON_TOOLS_UNSUPPORTED: &str = "tools_not_supported";
const FALLBACK_REASON_RESPONSES_UNSUPPORTED: &str = "responses_api_not_supported";
const FALLBACK_REASON_STREAM_UNSUPPORTED: &str = "stream_not_supported";

/// 标准化错误分类。
///
/// 该枚举用于把 provider/网关返回的“异构错误体”归一为稳定语义，
/// 避免业务层直接依赖某个供应商的文案格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    ToolsUnsupported,
    ResponsesUnsupported,
    StreamUnsupported,
    AuthError,
    RateLimited,
    TransientNetwork,
    Unknown,
}

impl ErrorCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCategory::ToolsUnsupported => "tools_unsupported",
            ErrorCategory::ResponsesUnsupported => "responses_unsupported",
            ErrorCategory::StreamUnsupported => "stream_unsupported",
            ErrorCategory::AuthError => "auth_error",
            ErrorCategory::RateLimited => "rate_limited",
            ErrorCategory::TransientNetwork => "transient_network",
            ErrorCategory::Unknown => "unknown",
        }
    }

    pub fn is_degradable(self) -> bool {
        matches!(
            self,
            ErrorCategory::ToolsUnsupported
                | ErrorCategory::ResponsesUnsupported
                | ErrorCategory::StreamUnsupported
        )
    }
}

/// 请求期望的能力标记。
///
/// 分类器会结合该标记判断“当前错误是否真由某能力不兼容导致”，
/// 从而降低仅靠关键词导致的误判。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestFeatureFlags {
    pub tools_enabled: bool,
    pub stream_enabled: bool,
    pub responses_enabled: bool,
}

impl RequestFeatureFlags {
    pub fn from_negotiated(negotiated: &NegotiatedCapabilities) -> Self {
        Self {
            tools_enabled: negotiated.tools_enabled,
            stream_enabled: negotiated.stream_enabled,
            responses_enabled: negotiated.endpoint == ModelEndpoint::ResponsesApi,
        }
    }
}

/// 分类结果详情。
///
/// 保留 status/provider code/摘要文案，便于日志直接展示“如何得出分类”。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedError {
    pub category: ErrorCategory,
    pub status_code: Option<u16>,
    pub provider_error_code: Option<String>,
    pub endpoint: ModelEndpoint,
    pub request_flags: RequestFeatureFlags,
    pub summary: String,
}

impl ClassifiedError {
    pub fn is_degradable(&self) -> bool {
        self.category.is_degradable()
    }

    /// 对缓存写回的“置信度”给出启发式估计。
    ///
    /// 这样做的原因是不同分类的确定性不同：
    /// - 明确的协议不兼容通常可高置信写回；
    /// - `unknown` 仅作保守记录，避免长期固化错误判断。
    pub fn confidence_hint(&self) -> f32 {
        match self.category {
            ErrorCategory::ToolsUnsupported
            | ErrorCategory::ResponsesUnsupported
            | ErrorCategory::StreamUnsupported => 0.92,
            ErrorCategory::AuthError | ErrorCategory::RateLimited => 0.9,
            ErrorCategory::TransientNetwork => 0.7,
            ErrorCategory::Unknown => 0.45,
        }
    }
}

/// 错误分类器。
#[derive(Debug, Default, Clone)]
pub struct ErrorClassifier;

impl ErrorClassifier {
    pub fn classify(
        &self,
        error: &Error,
        endpoint: ModelEndpoint,
        request_flags: RequestFeatureFlags,
    ) -> ClassifiedError {
        let message = error.to_string();
        let normalized = message.to_ascii_lowercase();
        let status_code = extract_status_code(&normalized);
        let provider_error_code = extract_provider_error_code(&normalized);
        let category = classify_category(
            &normalized,
            status_code,
            provider_error_code.as_deref(),
            request_flags,
        );

        ClassifiedError {
            category,
            status_code,
            provider_error_code,
            endpoint,
            request_flags,
            summary: first_line_summary(&message, 200),
        }
    }
}

/// 降级动作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityFallbackAction {
    DisableTools,
    DisableResponsesApi,
    DisableStream,
}

impl CapabilityFallbackAction {
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityFallbackAction::DisableTools => "disable_tools",
            CapabilityFallbackAction::DisableResponsesApi => "disable_responses_api",
            CapabilityFallbackAction::DisableStream => "disable_stream",
        }
    }
}

/// 单步降级决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityFallbackStep {
    pub action: CapabilityFallbackAction,
    pub reason: &'static str,
    pub from_category: ErrorCategory,
    pub override_caps: ProviderCapabilitiesOverride,
}

impl CapabilityFallbackStep {
    /// 将单步降级应用到当前协商结果，生成下一次尝试要使用的能力。
    pub fn apply_to(&self, from: &NegotiatedCapabilities) -> NegotiatedCapabilities {
        let provider_capabilities = from
            .provider_capabilities
            .downgrade(self.override_caps.clone());
        let mut sources = from.sources.clone();
        sources.push(format!("runtime:{}", self.reason));

        NegotiatedCapabilities {
            provider_capabilities,
            tools_enabled: from.tools_enabled && self.override_caps.supports_tools != Some(false),
            system_preamble_enabled: from.system_preamble_enabled
                && self.override_caps.supports_system_preamble != Some(false),
            endpoint: if self.override_caps.supports_responses_api == Some(false) {
                ModelEndpoint::ChatCompletions
            } else {
                from.endpoint
            },
            stream_enabled: from.stream_enabled
                && self.override_caps.supports_stream != Some(false),
            sources,
        }
    }
}

/// 显式能力降级计划。
///
/// 该状态机只负责“是否还能降级、下一步降级做什么”，
/// 不负责执行请求调用，从而可在不同请求路径（普通/流式）复用。
#[derive(Debug, Clone)]
pub struct CapabilityFallbackPlan {
    max_steps: usize,
    max_attempts: u32,
    applied_actions: Vec<CapabilityFallbackAction>,
}

impl CapabilityFallbackPlan {
    pub fn new(max_steps: usize, max_attempts: u32) -> Self {
        Self {
            max_steps: max_steps.max(1),
            max_attempts: max_attempts.max(1),
            applied_actions: Vec::new(),
        }
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    pub fn steps_taken(&self) -> usize {
        self.applied_actions.len()
    }

    pub fn next_step(
        &mut self,
        current: &NegotiatedCapabilities,
        classified: &ClassifiedError,
    ) -> Option<CapabilityFallbackStep> {
        if self.steps_taken() >= self.max_steps {
            return None;
        }

        let step = match classified.category {
            ErrorCategory::ToolsUnsupported if current.tools_enabled => CapabilityFallbackStep {
                action: CapabilityFallbackAction::DisableTools,
                reason: FALLBACK_REASON_TOOLS_UNSUPPORTED,
                from_category: classified.category,
                override_caps: ProviderCapabilitiesOverride {
                    supports_tools: Some(false),
                    ..Default::default()
                },
            },
            ErrorCategory::ResponsesUnsupported
                if current.endpoint == ModelEndpoint::ResponsesApi =>
            {
                CapabilityFallbackStep {
                    action: CapabilityFallbackAction::DisableResponsesApi,
                    reason: FALLBACK_REASON_RESPONSES_UNSUPPORTED,
                    from_category: classified.category,
                    override_caps: ProviderCapabilitiesOverride {
                        supports_responses_api: Some(false),
                        ..Default::default()
                    },
                }
            }
            ErrorCategory::StreamUnsupported if current.stream_enabled => CapabilityFallbackStep {
                action: CapabilityFallbackAction::DisableStream,
                reason: FALLBACK_REASON_STREAM_UNSUPPORTED,
                from_category: classified.category,
                override_caps: ProviderCapabilitiesOverride {
                    supports_stream: Some(false),
                    ..Default::default()
                },
            },
            _ => return None,
        };

        if self.applied_actions.contains(&step.action) {
            return None;
        }

        self.applied_actions.push(step.action);
        Some(step)
    }
}

fn classify_category(
    normalized: &str,
    status_code: Option<u16>,
    provider_error_code: Option<&str>,
    request_flags: RequestFeatureFlags,
) -> ErrorCategory {
    if is_auth_error(normalized, status_code, provider_error_code) {
        return ErrorCategory::AuthError;
    }
    if is_rate_limited(normalized, status_code, provider_error_code) {
        return ErrorCategory::RateLimited;
    }
    if is_responses_unsupported(normalized, status_code, request_flags) {
        return ErrorCategory::ResponsesUnsupported;
    }
    if is_tools_unsupported(normalized, status_code, request_flags) {
        return ErrorCategory::ToolsUnsupported;
    }
    if is_stream_unsupported(normalized, status_code, request_flags) {
        return ErrorCategory::StreamUnsupported;
    }
    if is_transient_network(normalized, status_code) {
        return ErrorCategory::TransientNetwork;
    }
    ErrorCategory::Unknown
}

fn is_auth_error(
    normalized: &str,
    status_code: Option<u16>,
    provider_error_code: Option<&str>,
) -> bool {
    if matches!(status_code, Some(401 | 403)) {
        return true;
    }

    if has_any(
        normalized,
        &[
            "unauthorized",
            "forbidden",
            "invalid api key",
            "invalid_api_key",
            "authentication failed",
            "auth failed",
            "permission denied",
        ],
    ) {
        return true;
    }

    provider_error_code.is_some_and(|code| {
        has_any(
            code,
            &[
                "invalid_api_key",
                "unauthorized",
                "forbidden",
                "authentication_error",
            ],
        )
    })
}

fn is_rate_limited(
    normalized: &str,
    status_code: Option<u16>,
    provider_error_code: Option<&str>,
) -> bool {
    if status_code == Some(429) {
        return true;
    }

    if has_any(
        normalized,
        &[
            "rate limit",
            "rate_limited",
            "too many requests",
            "quota exceeded",
            "insufficient_quota",
        ],
    ) {
        return true;
    }

    provider_error_code.is_some_and(|code| {
        has_any(
            code,
            &[
                "rate_limit",
                "rate_limited",
                "insufficient_quota",
                "too_many_requests",
            ],
        )
    })
}

fn is_tools_unsupported(
    normalized: &str,
    status_code: Option<u16>,
    flags: RequestFeatureFlags,
) -> bool {
    if !flags.tools_enabled {
        return false;
    }

    let mentions_tool = has_any(
        normalized,
        &[
            "tool definitions",
            "tool definition",
            "tools are not supported",
            "tool call",
            "function_call",
            "tools",
        ],
    );
    let looks_unsupported = has_any(
        normalized,
        &[
            "not support",
            "unsupported",
            "not available",
            "invalid parameter",
            "invalid_request_error",
            "400",
            "404",
        ],
    );
    let explicit_failure = normalized.contains("failed to get tool definitions");

    mentions_tool && (explicit_failure || looks_unsupported || status_code == Some(400))
}

fn is_responses_unsupported(
    normalized: &str,
    status_code: Option<u16>,
    flags: RequestFeatureFlags,
) -> bool {
    if !flags.responses_enabled {
        return false;
    }

    let mentions_responses = has_any(
        normalized,
        &[
            "/responses",
            "responses api",
            "responses endpoint",
            "responses is not supported",
        ],
    );
    let looks_unsupported = has_any(
        normalized,
        &[
            "not found",
            "unknown endpoint",
            "unsupported",
            "not support",
            "404",
        ],
    );

    (mentions_responses && looks_unsupported)
        || (status_code == Some(404) && normalized.contains("responses"))
}

fn is_stream_unsupported(
    normalized: &str,
    status_code: Option<u16>,
    flags: RequestFeatureFlags,
) -> bool {
    if !flags.stream_enabled {
        return false;
    }

    let mentions_stream = has_any(normalized, &["stream", "streaming", "sse"]);
    let looks_unsupported = has_any(
        normalized,
        &[
            "not support",
            "unsupported",
            "invalid",
            "not found",
            "400",
            "404",
        ],
    );

    mentions_stream && (looks_unsupported || status_code == Some(400))
}

fn is_transient_network(normalized: &str, status_code: Option<u16>) -> bool {
    if matches!(status_code, Some(408 | 500 | 502 | 503 | 504)) {
        return true;
    }

    has_any(
        normalized,
        &[
            "timeout",
            "timed out",
            "connection reset",
            "connection refused",
            "temporary failure",
            "temporarily unavailable",
            "dns",
            "network is unreachable",
            "broken pipe",
            "eof",
            "gateway timeout",
            "connection aborted",
        ],
    )
}

fn extract_status_code(normalized: &str) -> Option<u16> {
    for marker in ["\"status\":", "status code", "status="] {
        if let Some(code) = extract_status_after_marker(normalized, marker) {
            return Some(code);
        }
    }

    normalized
        .split(|ch: char| !ch.is_ascii_digit())
        .find_map(|token| {
            if token.len() != 3 {
                return None;
            }
            let value = token.parse::<u16>().ok()?;
            if (400..=599).contains(&value) {
                Some(value)
            } else {
                None
            }
        })
}

fn extract_status_after_marker(normalized: &str, marker: &str) -> Option<u16> {
    let index = normalized.find(marker)?;
    let suffix = normalized.get(index + marker.len()..)?;
    let digits = suffix
        .chars()
        .skip_while(|ch| !ch.is_ascii_digit())
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.len() < 3 {
        return None;
    }
    let value = digits.parse::<u16>().ok()?;
    if (400..=599).contains(&value) {
        Some(value)
    } else {
        None
    }
}

fn extract_provider_error_code(normalized: &str) -> Option<String> {
    for marker in ["\"code\":\"", "\"code\": \"", "code=", "code:"] {
        if let Some(value) = extract_token_after_marker(normalized, marker)
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    None
}

fn extract_token_after_marker(text: &str, marker: &str) -> Option<String> {
    let index = text.find(marker)?;
    let suffix = text.get(index + marker.len()..)?;
    let mut token = String::new();
    for ch in suffix.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            token.push(ch);
            continue;
        }
        break;
    }
    if token.is_empty() { None } else { Some(token) }
}

fn first_line_summary(text: &str, max_chars: usize) -> String {
    let first_line = text.lines().next().unwrap_or(text).trim();
    if first_line.chars().count() <= max_chars {
        return first_line.to_string();
    }
    let mut shortened = first_line
        .chars()
        .take(max_chars.saturating_sub(2))
        .collect::<String>();
    shortened.push_str("..");
    shortened
}

fn has_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;
    use crate::model::capabilities::ProviderCapabilities;

    fn negotiated(tools: bool, responses: bool, stream: bool) -> NegotiatedCapabilities {
        NegotiatedCapabilities {
            provider_capabilities: ProviderCapabilities {
                supports_tools: tools,
                supports_system_preamble: true,
                supports_responses_api: responses,
                supports_stream: stream,
            },
            tools_enabled: tools,
            system_preamble_enabled: true,
            endpoint: if responses {
                ModelEndpoint::ResponsesApi
            } else {
                ModelEndpoint::ChatCompletions
            },
            stream_enabled: stream,
            sources: vec!["test".to_string()],
        }
    }

    #[test]
    fn classify_openai_fixture_tools_unsupported() {
        let classifier = ErrorClassifier::default();
        let error = anyhow!(
            "{{\"error\":{{\"message\":\"tools are not supported\",\"code\":\"tools_not_supported\",\"status\":400}}}}"
        );

        let classified = classifier.classify(
            &error,
            ModelEndpoint::ChatCompletions,
            RequestFeatureFlags {
                tools_enabled: true,
                stream_enabled: false,
                responses_enabled: false,
            },
        );

        assert_eq!(classified.category, ErrorCategory::ToolsUnsupported);
        assert_eq!(classified.status_code, Some(400));
        assert_eq!(
            classified.provider_error_code,
            Some("tools_not_supported".to_string())
        );
    }

    #[test]
    fn classify_codex_fixture_tools_definition_error() {
        let classifier = ErrorClassifier::default();
        let error = anyhow!("RequestError: Failed to get tool definitions (400 Bad Request)");

        let classified = classifier.classify(
            &error,
            ModelEndpoint::ChatCompletions,
            RequestFeatureFlags {
                tools_enabled: true,
                stream_enabled: false,
                responses_enabled: false,
            },
        );

        assert_eq!(classified.category, ErrorCategory::ToolsUnsupported);
    }

    #[test]
    fn classify_openaiapi_fixture_responses_not_found() {
        let classifier = ErrorClassifier::default();
        let error = anyhow!("404 Not Found: unknown endpoint /v1/responses");

        let classified = classifier.classify(
            &error,
            ModelEndpoint::ResponsesApi,
            RequestFeatureFlags {
                tools_enabled: false,
                stream_enabled: false,
                responses_enabled: true,
            },
        );

        assert_eq!(classified.category, ErrorCategory::ResponsesUnsupported);
    }

    #[test]
    fn classify_auth_error() {
        let classifier = ErrorClassifier::default();
        let error = anyhow!("401 Unauthorized: invalid_api_key");
        let classified = classifier.classify(
            &error,
            ModelEndpoint::ChatCompletions,
            RequestFeatureFlags {
                tools_enabled: false,
                stream_enabled: false,
                responses_enabled: false,
            },
        );
        assert_eq!(classified.category, ErrorCategory::AuthError);
    }

    #[test]
    fn classify_rate_limited_error() {
        let classifier = ErrorClassifier::default();
        let error = anyhow!("429 Too Many Requests: insufficient_quota");
        let classified = classifier.classify(
            &error,
            ModelEndpoint::ChatCompletions,
            RequestFeatureFlags {
                tools_enabled: false,
                stream_enabled: false,
                responses_enabled: false,
            },
        );
        assert_eq!(classified.category, ErrorCategory::RateLimited);
    }

    #[test]
    fn fallback_plan_should_only_generate_degradable_step() {
        let mut plan = CapabilityFallbackPlan::new(3, 4);
        let current = negotiated(true, false, true);
        let classified = ClassifiedError {
            category: ErrorCategory::AuthError,
            status_code: Some(401),
            provider_error_code: Some("invalid_api_key".to_string()),
            endpoint: current.endpoint,
            request_flags: RequestFeatureFlags::from_negotiated(&current),
            summary: "401 Unauthorized".to_string(),
        };

        assert!(plan.next_step(&current, &classified).is_none());
        assert_eq!(plan.steps_taken(), 0);
    }

    #[test]
    fn fallback_plan_should_limit_steps() {
        let mut plan = CapabilityFallbackPlan::new(1, 3);
        let current = negotiated(true, true, true);
        let tools_classified = ClassifiedError {
            category: ErrorCategory::ToolsUnsupported,
            status_code: Some(400),
            provider_error_code: None,
            endpoint: current.endpoint,
            request_flags: RequestFeatureFlags::from_negotiated(&current),
            summary: "tools unsupported".to_string(),
        };

        let first = plan
            .next_step(&current, &tools_classified)
            .expect("first step should exist");
        let next_cap = first.apply_to(&current);

        let responses_classified = ClassifiedError {
            category: ErrorCategory::ResponsesUnsupported,
            status_code: Some(404),
            provider_error_code: None,
            endpoint: next_cap.endpoint,
            request_flags: RequestFeatureFlags::from_negotiated(&next_cap),
            summary: "responses unsupported".to_string(),
        };

        assert_eq!(first.action, CapabilityFallbackAction::DisableTools);
        assert!(plan.next_step(&next_cap, &responses_classified).is_none());
    }
}
