use std::path::PathBuf;

use serde_json::Value;

use crate::language::LspLanguage;

/// LSP 诊断级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl DiagnosticSeverity {
    /// 将 LSP 标准中的数字级别映射为内部枚举。
    ///
    /// 这样做的原因是 UI 层只关心语义等级，不应耦合具体数字常量。
    pub fn from_lsp_number(value: u64) -> Self {
        match value {
            1 => Self::Error,
            2 => Self::Warning,
            3 => Self::Information,
            4 => Self::Hint,
            _ => Self::Warning,
        }
    }

    /// 返回用于状态栏和诊断面板展示的短文本。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
            Self::Hint => "hint",
        }
    }

    /// 将内部诊断级别映射回 LSP 规范数字。
    ///
    /// 这里单独提供反向映射，是为了在 `codeAction` 请求里复用诊断上下文，
    /// 让服务端能基于已有诊断返回更准确的 quick fix。
    pub fn to_lsp_number(self) -> u8 {
        match self {
            Self::Error => 1,
            Self::Warning => 2,
            Self::Information => 3,
            Self::Hint => 4,
        }
    }
}

/// 归一化后的诊断信息。
#[derive(Debug, Clone)]
pub struct DiagnosticItem {
    pub file_path: PathBuf,
    pub line: u64,
    pub column: u64,
    pub severity: DiagnosticSeverity,
    pub message: String,
    /// 诊断起始位置（0-based，LSP 原始坐标）。
    ///
    /// UI 展示使用 `line/column`，而 LSP 二次请求（如 `codeAction`）需要原始坐标，
    /// 因此两套坐标都保留，避免来回换算带来的精度丢失。
    pub lsp_start_line: usize,
    pub lsp_start_character: usize,
    /// 诊断结束位置（0-based，LSP 原始坐标）。
    pub lsp_end_line: usize,
    pub lsp_end_character: usize,
    pub source: Option<String>,
    pub code: Option<String>,
}

/// LSP `TextEdit` 的简化结构。
#[derive(Debug, Clone)]
pub struct LspTextEdit {
    pub start_line: usize,
    pub start_character: usize,
    pub end_line: usize,
    pub end_character: usize,
    pub new_text: String,
}

/// LSP 补全项的简化结构。
#[derive(Debug, Clone)]
pub struct LspCompletionItem {
    pub label: String,
    pub insert_text: Option<String>,
    pub detail: Option<String>,
}

/// LSP 语义高亮 Token。
#[derive(Debug, Clone)]
pub struct LspSemanticToken {
    pub line: usize,
    pub start: usize,
    pub length: usize,
    pub token_type: String,
    pub token_modifiers: Vec<String>,
}

/// LSP `WorkspaceEdit` 中单文件的编辑集合。
#[derive(Debug, Clone)]
pub struct LspWorkspaceFileEdit {
    pub file_path: PathBuf,
    pub edits: Vec<LspTextEdit>,
}

/// LSP `WorkspaceEdit` 的简化结构。
#[derive(Debug, Clone, Default)]
pub struct LspWorkspaceEdit {
    pub document_edits: Vec<LspWorkspaceFileEdit>,
}

impl LspWorkspaceEdit {
    pub fn is_empty(&self) -> bool {
        self.document_edits.is_empty()
    }
}

/// LSP `Command` 的简化结构。
#[derive(Debug, Clone)]
pub struct LspCommand {
    pub title: String,
    pub command: String,
    pub arguments: Vec<Value>,
}

/// LSP `CodeAction` 的简化结构。
#[derive(Debug, Clone)]
pub struct LspCodeAction {
    pub title: String,
    pub kind: Option<String>,
    pub is_preferred: bool,
    pub edit: Option<LspWorkspaceEdit>,
    pub command: Option<LspCommand>,
}

/// 由服务端 `initialize` 响应归一化出的能力标记。
#[derive(Debug, Clone, Copy, Default)]
pub struct LspServerCapabilities {
    pub rename: bool,
    pub code_action: bool,
    pub formatting: bool,
    pub execute_command: bool,
}

/// 由 LSP 客户端发给上层 UI 的事件。
#[derive(Debug, Clone)]
pub enum LspEvent {
    Status(String),
    PublishDiagnostics {
        file_path: PathBuf,
        items: Vec<DiagnosticItem>,
    },
    /// 绑定到文件路径后的 `willSaveWaitUntil` 编辑结果。
    WillSaveWaitUntilEdits {
        file_path: PathBuf,
        edits: Vec<LspTextEdit>,
    },
    /// 异步补全返回。
    CompletionItems {
        file_path: PathBuf,
        items: Vec<LspCompletionItem>,
    },
    /// 异步语义高亮返回。
    SemanticTokens {
        file_path: PathBuf,
        tokens: Vec<LspSemanticToken>,
    },
    /// `textDocument/formatting` 返回。
    FormattingEdits {
        file_path: PathBuf,
        edits: Vec<LspTextEdit>,
    },
    /// `textDocument/rename` 返回。
    RenameWorkspaceEdit {
        file_path: PathBuf,
        new_name: String,
        edit: Option<LspWorkspaceEdit>,
    },
    /// `textDocument/codeAction` 返回。
    CodeActions {
        file_path: PathBuf,
        actions: Vec<LspCodeAction>,
    },
    /// 服务端主动请求客户端执行 `workspace/applyEdit`。
    ///
    /// 该请求常见于“执行 quick fix 命令后由服务端回推编辑”，
    /// 需要上层应用完编辑后再回包，才能形成完整修复闭环。
    WorkspaceApplyEditRequest {
        language: LspLanguage,
        request_id: u64,
        label: Option<String>,
        edit: LspWorkspaceEdit,
    },
    /// rust-analyzer 项目加载状态。
    ///
    /// 通过 `$ /progress`（实际方法名为 `$/progress`）通知提取，
    /// 用于在状态栏展示“加载中 / 已就绪”，并在就绪后触发一次语义高亮刷新。
    RustAnalyzerStatus {
        message: String,
        done: bool,
    },
}

/// 单个语言服务器可用性检查结果。
#[derive(Debug, Clone)]
pub struct LspServerCheckItem {
    pub language: String,
    pub server_command: String,
    pub available: bool,
    pub install_hint: String,
}

/// 全量 LSP 服务器可用性检查报告。
#[derive(Debug, Clone)]
pub struct LspServerCheckReport {
    pub items: Vec<LspServerCheckItem>,
}

impl LspServerCheckReport {
    /// 统计可用服务器数量。
    pub fn available_count(&self) -> usize {
        self.items.iter().filter(|item| item.available).count()
    }

    /// 统计不可用服务器数量。
    pub fn missing_count(&self) -> usize {
        self.items.iter().filter(|item| !item.available).count()
    }
}
