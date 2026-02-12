use std::{collections::HashSet, env, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 模型连接信息。
///
/// 该结构被 `rander` 与 `core::model::connection` 共同使用，
/// 用于描述一次可执行的 LLM 连接参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Provider 的 API 基础地址。
    ///
    /// 当为空字符串时，连接层会回退到 provider 默认地址。
    pub api_url: String,
    /// API Token / Key。
    ///
    /// 允许为空；为空时连接层会继续尝试读取对应环境变量。
    pub token: String,
    /// 模型名，例如 `gpt-4o-mini`。
    pub model_name: String,
    /// 是否支持 tools。
    pub support_tools: bool,
    /// Provider 名称，例如 `openai` / `claude` / `gemini`。
    pub provider_name: String,
    /// 模型最大上下文长度。
    pub model_max_context: u32,
    /// 模型最大输出长度。
    pub model_max_output: u32,
    /// 模型最大 token 总预算。
    pub model_max_tokens: u32,
}

type ModelInfoList = Vec<ModelInfo>;

/// 统一读取用户可用模型列表。
///
/// 读取顺序：
/// 1. 运行目录或显式路径中的配置文件；
/// 2. `ORDER_MODEL_*` 显式环境变量；
/// 3. Provider 通用环境变量兜底（如 `OPENAI_API_KEY`）。
pub fn get_user_model_info_list() -> Result<Option<ModelInfoList>> {
    let mut list = Vec::new();

    if let Some(config) = load_model_config_file()? {
        if let Some(current) = config.current {
            push_unique_model(&mut list, current);
        }
        for model in config.models {
            push_unique_model(&mut list, model);
        }
    }

    if let Some(from_env) = load_explicit_model_from_env() {
        push_unique_model(&mut list, from_env);
    }

    // 若用户未显式配置任何模型，则尝试从最常见的 provider 环境变量推导一个可用模型。
    if list.is_empty()
        && let Some(fallback) = load_fallback_model_from_provider_env()
    {
        list.push(fallback);
    }

    if list.is_empty() {
        Ok(None)
    } else {
        Ok(Some(list))
    }
}

/// 获取当前使用模型。
///
/// 优先级：
/// 1. `ORDER_MODEL_*` 显式环境变量；
/// 2. 配置文件中的 `current` / `current_model` / 首个模型；
/// 3. Provider 通用环境变量兜底。
///
/// 特殊处理：当配置文件的 token 为空且 provider 为 codex/openai 时，
/// 尝试从 Codex CLI 配置补充 token 和 api_url。
pub fn get_current_model_info() -> Result<Option<ModelInfo>> {
    if let Some(model) = load_explicit_model_from_env() {
        return Ok(Some(model));
    }

    if let Some(config) = load_model_config_file()?
        && let Some(mut model) = select_current_model(config)
    {
        // 当配置文件的 token 为空且 provider 为 codex/openai 时，
        // 尝试从 Codex CLI 配置补充 token 和 api_url
        if model.token.trim().is_empty() && is_openai_like_provider(&model.provider_name) {
            if let Some(codex_config) = load_codex_cli_config() {
                if model.token.trim().is_empty() {
                    model.token = codex_config.token;
                }
                if model.api_url.trim().is_empty() {
                    model.api_url = codex_config.api_url;
                }
            }
        }
        return Ok(Some(model));
    }

    Ok(load_fallback_model_from_provider_env())
}

/// 判断 provider 是否为 OpenAI 兼容类型。
fn is_openai_like_provider(provider_name: &str) -> bool {
    matches!(
        provider_name.trim().to_ascii_lowercase().as_str(),
        "openai" | "codex" | "openaiapi" | "openai_api"
    )
}

/// 仅从配置文件中读取当前模型信息，不使用任何环境变量兜底。
///
/// 设计原因：
/// - 启动阶段需要判断“是否已有可用配置文件”，避免被环境变量干扰；
/// - 若直接使用 `get_current_model_info()`，会把临时环境兜底模型当成已有配置，
///   导致自动探测 Codex 被错误跳过。
pub fn get_current_model_info_from_config() -> Result<Option<ModelInfo>> {
    if let Some(config) = load_model_config_file()? {
        return Ok(select_current_model(config));
    }

    Ok(None)
}

/// 解析配置文件并返回“当前模型 + 模型列表”。
#[derive(Debug, Default, Clone)]
struct ModelConfigFile {
    current: Option<ModelInfo>,
    models: Vec<ModelInfo>,
    current_model_name: Option<String>,
}

/// 将模型追加到列表，按 `provider + model_name` 去重。
fn push_unique_model(list: &mut Vec<ModelInfo>, model: ModelInfo) {
    let duplicated = list.iter().any(|existing| {
        existing
            .provider_name
            .eq_ignore_ascii_case(&model.provider_name)
            && existing.model_name.eq_ignore_ascii_case(&model.model_name)
    });
    if !duplicated {
        list.push(model);
    }
}

/// 从显式的 `ORDER_MODEL_*` 环境变量读取当前模型。
///
/// 只有 provider 与 model 两个关键字段同时存在时，才认为配置有效。
fn load_explicit_model_from_env() -> Option<ModelInfo> {
    let provider_name = first_non_empty_env(&["ORDER_MODEL_PROVIDER", "ORDER_PROVIDER"])?;
    let model_name = first_non_empty_env(&["ORDER_MODEL_NAME", "ORDER_MODEL"])?;

    let api_url =
        first_non_empty_env(&["ORDER_MODEL_API_URL", "ORDER_API_URL"]).unwrap_or_default();
    let token = first_non_empty_env(&["ORDER_MODEL_TOKEN", "ORDER_API_KEY"]).unwrap_or_default();

    let support_tools = first_non_empty_env(&["ORDER_MODEL_SUPPORT_TOOLS"])
        .as_deref()
        .map(parse_bool_text)
        .unwrap_or(false);
    let model_max_context = first_non_empty_env(&["ORDER_MODEL_MAX_CONTEXT"])
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let model_max_output = first_non_empty_env(&["ORDER_MODEL_MAX_OUTPUT"])
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let model_max_tokens = first_non_empty_env(&["ORDER_MODEL_MAX_TOKENS"])
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);

    Some(normalize_model_info(ModelInfo {
        api_url,
        token,
        model_name,
        support_tools,
        provider_name,
        model_max_context,
        model_max_output,
        model_max_tokens,
    }))
}

/// 从 provider 通用环境变量兜底出一个可用模型。
///
/// 该兜底逻辑的目标是“尽量可用”：
/// - 若存在 API Key，默认给出常见模型名；
/// - 模型名可通过 provider 对应环境变量覆盖。
fn load_fallback_model_from_provider_env() -> Option<ModelInfo> {
    // 优先尝试读取本地 Codex CLI 配置
    if let Some(model) = load_codex_cli_config() {
        return Some(model);
    }

    // Codex 作为 OpenAI 的"编码型"模型常被单独配置。
    // 优先读取 `CODEX_API_KEY`，避免与通用 OpenAI 配置互相覆盖。
    if let Ok(token) = env::var("CODEX_API_KEY")
        && !token.trim().is_empty()
    {
        let model_name = env::var("CODEX_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "gpt-5.3-codex".to_string());
        let api_url = env::var("CODEX_BASE_URL").unwrap_or_default();
        return Some(ModelInfo {
            api_url,
            token,
            model_name,
            support_tools: false,
            provider_name: "codex".to_string(),
            model_max_context: 0,
            model_max_output: 0,
            model_max_tokens: 0,
        });
    }

    if let Ok(token) = env::var("OPENAI_API_KEY")
        && !token.trim().is_empty()
    {
        let model_name = env::var("OPENAI_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "gpt-4o-mini".to_string());
        let api_url = env::var("OPENAI_BASE_URL").unwrap_or_default();
        return Some(ModelInfo {
            api_url,
            token,
            model_name,
            support_tools: false,
            provider_name: "openai".to_string(),
            model_max_context: 0,
            model_max_output: 0,
            model_max_tokens: 0,
        });
    }

    if let Ok(token) = env::var("ANTHROPIC_API_KEY")
        && !token.trim().is_empty()
    {
        let model_name = env::var("ANTHROPIC_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "claude-3-5-sonnet-latest".to_string());
        return Some(ModelInfo {
            api_url: String::new(),
            token,
            model_name,
            support_tools: false,
            provider_name: "claude".to_string(),
            model_max_context: 0,
            model_max_output: 0,
            model_max_tokens: 0,
        });
    }

    if let Ok(token) = env::var("GEMINI_API_KEY")
        && !token.trim().is_empty()
    {
        let model_name = env::var("GEMINI_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "gemini-2.0-flash".to_string());
        return Some(ModelInfo {
            api_url: String::new(),
            token,
            model_name,
            support_tools: false,
            provider_name: "gemini".to_string(),
            model_max_context: 0,
            model_max_output: 0,
            model_max_tokens: 0,
        });
    }

    None
}

/// 选择当前模型。
///
/// 选择顺序：`current` -> `current_model_name` 命中的模型 -> 列表首个模型。
fn select_current_model(config: ModelConfigFile) -> Option<ModelInfo> {
    if let Some(current) = config.current {
        return Some(current);
    }

    if let Some(current_model_name) = config.current_model_name {
        let selected = config
            .models
            .iter()
            .find(|model| model.model_name.eq_ignore_ascii_case(&current_model_name))
            .cloned();
        if selected.is_some() {
            return selected;
        }
    }

    config.models.first().cloned()
}

/// 读取模型配置文件。
fn load_model_config_file() -> Result<Option<ModelConfigFile>> {
    let paths = candidate_config_paths()?;
    let Some(config_path) = paths.into_iter().find(|path| path.exists()) else {
        return Ok(None);
    };

    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("读取模型配置失败: {}", config_path.display()))?;
    if content.trim().is_empty() {
        return Ok(Some(ModelConfigFile::default()));
    }

    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("解析模型配置 JSON 失败: {}", config_path.display()))?;
    let config = parse_model_config_value(&value)
        .with_context(|| format!("模型配置结构无效: {}", config_path.display()))?;

    Ok(Some(config))
}

/// 列出可用配置路径候选。
fn candidate_config_paths() -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();

    if let Some(explicit_path) = first_non_empty_env(&["ORDER_MODEL_CONFIG"]) {
        paths.push(PathBuf::from(explicit_path));
    }

    let current_dir = env::current_dir().context("获取当前运行目录失败")?;
    for relative in [
        "model.json",
        "Model.json",
        "models.json",
        "config/model.json",
        "config/models.json",
        ".order/model.json",
    ] {
        paths.push(current_dir.join(relative));
    }

    // 去重，避免同一路径被重复解析。
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    Ok(paths)
}

/// 解析模型配置 JSON。
///
/// 支持的配置形态：
/// - 单模型对象；
/// - 模型数组；
/// - `{ "current": {...}, "models": [...] }`；
/// - `{ "current_model": "xxx", "models": [...] }`。
fn parse_model_config_value(value: &Value) -> Result<ModelConfigFile> {
    match value {
        Value::Array(items) => {
            let models = items
                .iter()
                .filter_map(parse_model_info_from_value)
                .collect();
            Ok(ModelConfigFile {
                current: None,
                models,
                current_model_name: None,
            })
        }
        Value::Object(object) => {
            // 若根对象本身就是模型结构，则将其视作当前模型。
            if let Some(single_model) = parse_model_info_from_value(value) {
                return Ok(ModelConfigFile {
                    current: Some(single_model.clone()),
                    models: vec![single_model],
                    current_model_name: None,
                });
            }

            let current = object.get("current").and_then(parse_model_info_from_value);
            let models = object
                .get("models")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(parse_model_info_from_value)
                        .collect()
                })
                .unwrap_or_default();

            let current_model_name = read_string_key(
                object,
                &["current_model", "current_model_name", "selected_model"],
            );

            Ok(ModelConfigFile {
                current,
                models,
                current_model_name,
            })
        }
        _ => Ok(ModelConfigFile::default()),
    }
}

/// 将 JSON 值解析为 `ModelInfo`。
///
/// 为提升兼容性，字段支持蛇形与驼峰命名，也兼容常见别名。
fn parse_model_info_from_value(value: &Value) -> Option<ModelInfo> {
    let object = value.as_object()?;

    let provider_name = read_string_key(object, &["provider_name", "providerName", "provider"])?;
    let model_name = read_string_key(object, &["model_name", "modelName", "model", "name"])?;

    let api_url =
        read_string_key(object, &["api_url", "apiUrl", "base_url", "baseUrl"]).unwrap_or_default();
    let token = read_string_key(object, &["token", "api_key", "apiKey", "key"]).unwrap_or_default();
    let support_tools = read_bool_key(object, &["support_tools", "supportTools"]).unwrap_or(false);
    let model_max_context =
        read_u32_key(object, &["model_max_context", "modelMaxContext"]).unwrap_or(0);
    let model_max_output =
        read_u32_key(object, &["model_max_output", "modelMaxOutput"]).unwrap_or(0);
    let model_max_tokens =
        read_u32_key(object, &["model_max_tokens", "modelMaxTokens"]).unwrap_or(0);

    Some(normalize_model_info(ModelInfo {
        api_url,
        token,
        model_name,
        support_tools,
        provider_name,
        model_max_context,
        model_max_output,
        model_max_tokens,
    }))
}

/// 对模型信息做轻量标准化。
fn normalize_model_info(mut model: ModelInfo) -> ModelInfo {
    model.api_url = model.api_url.trim().to_string();
    model.token = model.token.trim().to_string();
    model.model_name = model.model_name.trim().to_string();
    model.provider_name = model.provider_name.trim().to_string();

    // 若总 token 未配置，则尽量从上下文/输出上限推导一个可用值。
    if model.model_max_tokens == 0 {
        model.model_max_tokens = model.model_max_context.max(model.model_max_output);
    }

    model
}

/// 从对象中读取第一个非空字符串字段。
fn read_string_key(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        object
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

/// 从对象中读取 `u32` 字段，支持数字或可解析数字字符串。
fn read_u32_key(object: &Map<String, Value>, keys: &[&str]) -> Option<u32> {
    keys.iter().find_map(|key| {
        let value = object.get(*key)?;
        if let Some(number) = value.as_u64() {
            return u32::try_from(number).ok();
        }
        if let Some(text) = value.as_str() {
            return text.trim().parse::<u32>().ok();
        }
        None
    })
}

/// 从对象中读取布尔字段，支持布尔值或文本值。
fn read_bool_key(object: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| {
        let value = object.get(*key)?;
        if let Some(flag) = value.as_bool() {
            return Some(flag);
        }
        value.as_str().map(parse_bool_text)
    })
}

/// 将文本解析为布尔值。
fn parse_bool_text(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "y"
    )
}

/// 读取一组环境变量中的第一个非空值。
fn first_non_empty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

/// 从本地 Codex CLI 配置目录读取模型信息。
///
/// Codex CLI 配置文件位置：
/// - Windows: `%USERPROFILE%\.codex\`
/// - Unix: `~/.codex/`
///
/// 配置文件：
/// - `auth.json`: `{ "OPENAI_API_KEY": "xxx" }`
/// - `config.toml`: 模型和 API URL 配置
fn load_codex_cli_config() -> Option<ModelInfo> {
    let codex_dir = if cfg!(windows) {
        env::var("USERPROFILE")
            .ok()
            .map(|p| PathBuf::from(p).join(".codex"))
    } else {
        env::var("HOME")
            .ok()
            .map(|p| PathBuf::from(p).join(".codex"))
    };

    let codex_dir = codex_dir.filter(|p| p.exists())?;

    let token = load_codex_auth_json(&codex_dir)?;
    let (model_name, api_url) = load_codex_config_toml(&codex_dir);

    Some(ModelInfo {
        api_url,
        token,
        model_name,
        support_tools: false,
        provider_name: "codex".to_string(),
        model_max_context: 0,
        model_max_output: 0,
        model_max_tokens: 0,
    })
}

/// 从 Codex CLI auth.json 读取 API Key。
fn load_codex_auth_json(codex_dir: &PathBuf) -> Option<String> {
    let auth_path = codex_dir.join("auth.json");
    if !auth_path.exists() {
        return None;
    }

    let content = fs::read_to_string(&auth_path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;

    value
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
}

/// 从 Codex CLI config.toml 读取模型名和 API URL。
///
/// 返回 (model_name, base_url)
fn load_codex_config_toml(codex_dir: &PathBuf) -> (String, String) {
    let config_path = codex_dir.join("config.toml");
    if !config_path.exists() {
        return ("gpt-5.3-codex".to_string(), String::new());
    }

    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return ("gpt-5.3-codex".to_string(), String::new()),
    };

    let doc: toml::Value = match content.parse() {
        Ok(d) => d,
        Err(_) => return ("gpt-5.3-codex".to_string(), String::new()),
    };

    let model_name = doc
        .get("model")
        .and_then(toml::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("gpt-5.3-codex")
        .to_string();

    let model_provider = doc.get("model_provider").and_then(toml::Value::as_str);

    let base_url = model_provider
        .and_then(|provider| {
            doc.get("model_providers")
                .and_then(|providers| providers.get(provider))
                .and_then(|p| p.get("base_url"))
                .and_then(toml::Value::as_str)
        })
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    (model_name, base_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn parse_single_model_object() {
        let value = json!({
            "provider": "openai",
            "model": "gpt-4o-mini",
            "api_key": "abc",
            "api_url": "https://api.openai.com/v1"
        });

        let model = parse_model_info_from_value(&value).expect("should parse model");
        assert_eq!(model.provider_name, "openai");
        assert_eq!(model.model_name, "gpt-4o-mini");
        assert_eq!(model.token, "abc");
        assert_eq!(model.api_url, "https://api.openai.com/v1");
    }

    #[test]
    fn parse_config_with_current_model_name() {
        let value = json!({
            "current_model": "gemini-2.0-flash",
            "models": [
                {"provider": "openai", "model": "gpt-4o-mini"},
                {"provider": "gemini", "model": "gemini-2.0-flash"}
            ]
        });

        let config = parse_model_config_value(&value).expect("config should parse");
        let current = select_current_model(config).expect("should select one model");
        assert_eq!(current.provider_name, "gemini");
        assert_eq!(current.model_name, "gemini-2.0-flash");
    }

    #[test]
    fn parse_bool_text_works() {
        assert!(parse_bool_text("true"));
        assert!(parse_bool_text("YES"));
        assert!(parse_bool_text("1"));
        assert!(!parse_bool_text("false"));
        assert!(!parse_bool_text("0"));
    }

    #[test]
    fn test_load_codex_cli_config() {
        if let Some(model) = load_codex_cli_config() {
            println!("[TEST] Codex CLI config loaded:");
            println!("  provider: {}", model.provider_name);
            println!("  model: {}", model.model_name);
            println!(
                "  token: {}",
                if model.token.is_empty() {
                    "EMPTY"
                } else {
                    "SET"
                }
            );
            println!(
                "  api_url: {}",
                if model.api_url.is_empty() {
                    "EMPTY"
                } else {
                    &model.api_url
                }
            );
            assert!(!model.token.is_empty(), "Token should not be empty");
        } else {
            println!("[TEST] No Codex CLI config found (this is expected if not configured)");
        }
    }

    #[test]
    fn test_get_current_model_info_with_codex_fallback() {
        if let Ok(Some(model)) = get_current_model_info() {
            println!("[TEST] Current model info:");
            println!("  provider: {}", model.provider_name);
            println!("  model: {}", model.model_name);
            println!(
                "  token: {}",
                if model.token.is_empty() {
                    "EMPTY"
                } else {
                    "SET"
                }
            );
            println!(
                "  api_url: {}",
                if model.api_url.is_empty() {
                    "EMPTY"
                } else {
                    &model.api_url
                }
            );
        } else {
            println!("[TEST] No model info available");
        }
    }

    #[test]
    fn candidate_config_paths_contains_runtime_paths() {
        let paths = candidate_config_paths().expect("should build candidate paths");
        let has_model_json = paths
            .iter()
            .any(|path| path.ends_with(Path::new("model.json")));
        assert!(has_model_json);
    }
}
