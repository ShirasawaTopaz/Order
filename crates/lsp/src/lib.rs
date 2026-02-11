//! LSP 客户端能力入口。
//!
//! 目录拆分说明：
//! - `types`：对外数据结构与事件定义；
//! - `language`：语言识别与语言服务器路由策略；
//! - `protocol`：LSP JSON-RPC 报文编解码工具；
//! - `client`：多语言 LSP 客户端管理实现。

mod client;
mod language;
mod protocol;
mod types;

pub use client::LspClient;
pub use language::{
    LspLanguage, all_languages, detect_language, detect_language_from_path_or_name,
};
pub use types::{
    DiagnosticItem, DiagnosticSeverity, LspCompletionItem, LspEvent, LspSemanticToken,
    LspServerCheckItem, LspServerCheckReport, LspTextEdit,
};
