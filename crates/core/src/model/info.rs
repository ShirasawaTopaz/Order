use std::sync::OnceLock;

use anyhow::Ok;

#[derive(Clone)]
pub struct ModelInfo{
    pub api_url: String,
    pub token: String,
    pub model_name: String,
    pub support_tools: bool,
    pub provider_name: String,
    pub model_max_context: u32,
    pub model_max_output: u32,
    pub model_max_tokens: u32,
}

type ModelInfoList = Vec<ModelInfo>;

static USER_MODEL_INFO_LIST: OnceLock<ModelInfoList> = OnceLock::new();
static CURRENT_MODEL_INFO: OnceLock<Option<ModelInfo>> = OnceLock::new();

pub fn get_user_model_info_list() -> anyhow::Result<Option<ModelInfoList>> {
    let value = USER_MODEL_INFO_LIST.get_or_init(|| {
        // TODO: 获取本地配置文件
        vec![]
    }).clone();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

pub fn get_current_model_info() -> anyhow::Result<Option<ModelInfo>> {
    Ok(CURRENT_MODEL_INFO.get_or_init(|| {
        // TODO: 获取当前模型信息
        None
    }).clone())
}
