use std::path::Path;

/// 编辑器支持的语言类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LspLanguage {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Html,
    Css,
    Vue,
    Java,
    Go,
    C,
    Cpp,
}

/// 返回编辑器支持的全部语言列表。
///
/// 该列表用于统一执行“LSP 服务器可用性检查”，
/// 避免命令分散在多个调用点导致检查口径不一致。
pub fn all_languages() -> &'static [LspLanguage] {
    const LANGUAGES: [LspLanguage; 11] = [
        LspLanguage::Rust,
        LspLanguage::Python,
        LspLanguage::TypeScript,
        LspLanguage::JavaScript,
        LspLanguage::Html,
        LspLanguage::Css,
        LspLanguage::Vue,
        LspLanguage::Java,
        LspLanguage::Go,
        LspLanguage::C,
        LspLanguage::Cpp,
    ];
    &LANGUAGES
}

impl LspLanguage {
    /// 返回人类可读的语言名称。
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::TypeScript => "TypeScript",
            Self::JavaScript => "JavaScript",
            Self::Html => "HTML",
            Self::Css => "CSS",
            Self::Vue => "Vue",
            Self::Java => "Java",
            Self::Go => "Go",
            Self::C => "C",
            Self::Cpp => "C++",
        }
    }

    /// 返回 LSP `languageId`。
    ///
    /// 保持与主流语言服务器约定一致，避免因标识不一致导致补全/诊断能力失效。
    pub fn language_id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Html => "html",
            Self::Css => "css",
            Self::Vue => "vue",
            Self::Java => "java",
            Self::Go => "go",
            Self::C => "c",
            Self::Cpp => "cpp",
        }
    }

    /// 返回该语言建议启动的 LSP 命令。
    ///
    /// 选择“常见可执行名 + 最少参数”，是为了降低用户环境配置门槛。
    pub fn server_command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Rust => ("rust-analyzer", &[]),
            Self::Python => ("pylsp", &[]),
            Self::TypeScript | Self::JavaScript => ("typescript-language-server", &["--stdio"]),
            Self::Html => ("vscode-html-language-server", &["--stdio"]),
            Self::Css => ("vscode-css-language-server", &["--stdio"]),
            Self::Vue => ("vue-language-server", &["--stdio"]),
            Self::Java => ("jdtls", &[]),
            Self::Go => ("gopls", &[]),
            Self::C | Self::Cpp => ("clangd", &[]),
        }
    }

    /// 返回该语言服务器缺失时的安装建议。
    ///
    /// 采用“最常见安装方式 + 可执行文件名”的组合提示，
    /// 可以让用户在状态栏看到可直接执行的修复方向。
    pub fn install_hint(self) -> &'static str {
        match self {
            Self::Rust => "建议安装 rust-analyzer，并确保命令 `rust-analyzer` 可用。",
            Self::Python => "可执行 `pip install python-lsp-server` 后重试。",
            Self::TypeScript | Self::JavaScript => {
                "可执行 `npm i -g typescript typescript-language-server` 后重试。"
            }
            Self::Html | Self::Css => "可执行 `npm i -g vscode-langservers-extracted` 后重试。",
            Self::Vue => "可执行 `npm i -g @vue/language-server` 后重试。",
            Self::Java => "请安装 Eclipse JDT Language Server，并确保 `jdtls` 在 PATH 中。",
            Self::Go => "可执行 `go install golang.org/x/tools/gopls@latest` 后重试。",
            Self::C | Self::Cpp => "请安装 clangd，并确保命令 `clangd` 在 PATH 中。",
        }
    }

    /// 返回语义高亮特性中的 token 类型表。
    ///
    /// 该表用于将语义 token 的数字索引反解为可读字符串。
    pub fn semantic_token_types(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "namespace",
                "type",
                "class",
                "enum",
                "interface",
                "struct",
                "typeParameter",
                "parameter",
                "variable",
                "property",
                "enumMember",
                "event",
                "function",
                "method",
                "macro",
                "keyword",
                "modifier",
                "comment",
                "string",
                "number",
                "regexp",
                "operator",
            ],
            _ => &[
                "namespace",
                "type",
                "class",
                "enum",
                "interface",
                "struct",
                "typeParameter",
                "parameter",
                "variable",
                "property",
                "enumMember",
                "event",
                "function",
                "method",
                "keyword",
                "comment",
                "string",
                "number",
                "operator",
            ],
        }
    }

    /// 返回语义高亮特性中的 token 修饰符表。
    pub fn semantic_token_modifiers(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "declaration",
                "definition",
                "readonly",
                "static",
                "deprecated",
                "abstract",
                "async",
                "modification",
                "documentation",
                "defaultLibrary",
                "mutable",
                "consuming",
                "unsafe",
                "attribute",
                "callable",
            ],
            _ => &[
                "declaration",
                "definition",
                "readonly",
                "static",
                "deprecated",
                "abstract",
                "async",
                "modification",
                "documentation",
                "defaultLibrary",
            ],
        }
    }
}

/// 根据路径识别语言。
pub fn detect_language(path: &Path) -> Option<LspLanguage> {
    let extension = path.extension().and_then(|ext| ext.to_str())?.to_ascii_lowercase();
    match extension.as_str() {
        "rs" => Some(LspLanguage::Rust),
        "py" => Some(LspLanguage::Python),
        "ts" | "tsx" => Some(LspLanguage::TypeScript),
        "js" | "jsx" | "mjs" | "cjs" => Some(LspLanguage::JavaScript),
        "html" | "htm" => Some(LspLanguage::Html),
        "css" | "scss" | "less" => Some(LspLanguage::Css),
        "vue" => Some(LspLanguage::Vue),
        "java" => Some(LspLanguage::Java),
        "go" => Some(LspLanguage::Go),
        "c" | "h" => Some(LspLanguage::C),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some(LspLanguage::Cpp),
        _ => None,
    }
}

/// 根据路径或名称识别语言。
///
/// 对未落盘缓冲区，路径可能为空，此时回退到缓冲区名称后缀判断。
pub fn detect_language_from_path_or_name(path: Option<&Path>, name: &str) -> Option<LspLanguage> {
    if let Some(path) = path
        && let Some(language) = detect_language(path)
    {
        return Some(language);
    }

    let fake_path = Path::new(name);
    detect_language(fake_path)
}
