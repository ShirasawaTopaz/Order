# Order

`Order` 是一个基于 Rust 的终端交互工具（TUI）工作区，包含主程序、编辑器渲染层、模型连接层与预留的 LSP 模块。

## 项目结构

本仓库使用 Cargo Workspace 组织：

- `crates/order`：程序入口（可执行文件）。
- `crates/rander`：TUI 与编辑器界面逻辑。
- `crates/core`：核心能力（命令、模型连接、类型定义）。
- `crates/lsp`：LSP 相关模块（当前为预留骨架）。

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

## 模型 Provider 与密钥

`core` 中已支持以下 Provider 枚举：

- `OpenAI`
- `Claude`
- `Gemini`
- `OpenAIAPI`

当连接配置未显式传入 `api_key` 时，会按 Provider 读取环境变量：

- `OPENAI_API_KEY`
- `ANTHROPIC_API_KEY`
- `GEMINI_API_KEY`

## 当前状态说明

- 编辑器与主界面可运行，支持基本输入与焦点切换流程。
- `lsp` 模块为预留结构，后续可按需求扩展。
- 部分模型信息读取逻辑仍为 TODO（见 `core` 模块实现）。

