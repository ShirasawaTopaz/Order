# Order

`Order` 是一个基于 Rust 的终端交互工具（TUI）工作区，包含主程序、编辑器渲染层、模型连接层与预留的 LSP 模块。
本项目是实验性项目，绝大多数代码由codex和glm生成，仅在必要时手动编写。
## 项目结构

本仓库使用 Cargo Workspace 组织：

- `crates/order`：程序入口（可执行文件）。
- `crates/rander`：TUI 与编辑器界面逻辑。
- `crates/core`：核心能力（命令、模型连接、类型定义）。
- `crates/lsp`：多语言 LSP 客户端与协议适配层。

## 环境要求

- Rust（建议使用稳定版，且支持 `edition = 2024`）
- Cargo
- 支持 ANSI/鼠标事件的终端（Windows Terminal、PowerShell、WezTerm 等）

## 快速开始

在仓库根目录执行：

```bash
cargo run
```

常用开发命令：

```bash
# 检查整个工作区
cargo check --workspace
```
## 主界面命令

主界面输入框支持以下命令：

- `/help`
- `/exit`
- `/cancel`
- `/history`
- `/skills`
- `/rules`
- `/settings`
- `/status`
- `/editor`

其中 `/editor` 可进入内置编辑器视图。

`/settings` 目前用于生成模型配置：

- 启动时若未检测到任何模型配置文件，会默认探测 Codex；可用则自动写入 `.order/model.json`。
- 默认探测 Codex 是否可用；若可用则写入 `.order/model.json`，并在主界面 `Model` 面板展示为 `codex/<model>`。
- 若已存在配置文件且不想覆盖，可直接跳过；如需覆盖请使用 `/settings force`。

## 模型 Provider 与密钥

`core` 中已支持以下 Provider 枚举：

- `OpenAI`
- `Codex`
- `Claude`
- `Gemini`
- `OpenAIAPI`

当连接配置未显式传入 `api_key` 时，会按 Provider 读取环境变量：

- `CODEX_API_KEY`（Codex 优先；未设置会回退到 `OPENAI_API_KEY`）
- `OPENAI_API_KEY`
- `ANTHROPIC_API_KEY`
- `GEMINI_API_KEY`

## 模型配置（推荐）

推荐在仓库根目录创建 `.order/model.json`（可参考 `.order/model.example.json`）。

配置文件支持字段（常用）：

- `provider`：`openai` / `codex` / `claude` / `gemini` / `openaiapi`
- `model`：例如 `gpt-5.3-codex`
- `api_url`：可选，自定义 Base URL
- `token`：可选；为空时会读取对应环境变量
- `support_tools`：是否启用工具调用（见下）

当 `provider` 为 `openai` 或 `codex` 且 `support_tools = true` 时，会启用内置文件工具：

- `ReadTool`：读取工作区内文件（仅相对路径、UTF-8、大小受限）
- `WriteTool`：写入工作区内文件（仅相对路径、默认写入 LF、大小受限）
- `SearchFileTool`：在工作区内递归搜索关键字（仅相对路径、结果数量受限）

## LSP 能力说明

编辑器当前支持以下语言的 LSP 诊断、补全与语义高亮：

- Rust（`rust-analyzer`）
- Python（`pylsp`）
- TypeScript / JavaScript（`typescript-language-server --stdio`）
- HTML（`vscode-html-language-server --stdio`）
- CSS（`vscode-css-language-server --stdio`）
- Vue（`vue-language-server --stdio`）
- Java（`jdtls`）
- Go（`gopls`）
- C / C++（`clangd`）

说明：

- 语言服务器采用“按需启动”，在首次打开对应语言文件时自动拉起。
- Rust 代码高亮已切换为由 `rust-analyzer` 返回的语义 token 驱动。
- 代码补全由 LSP 异步返回并在编辑器中缓存展示。

编辑器内可执行命令：

- `lc`：执行 LSP 服务器可用性检查（是否在 PATH 中可用）。
  - 全部可用：状态栏显示通过摘要。
  - 有缺失：状态栏显示缺失语言与安装建议。
