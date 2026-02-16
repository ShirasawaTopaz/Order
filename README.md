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

## Windows 编码兼容

- 程序启动时会自动检测并将 Windows 控制台输入/输出编码切换为 UTF-8（code page `65001`）。
- 历史、日志、模型配置等文本文件统一按 UTF-8（无 BOM）+ LF 读写；检测到 BOM/CRLF 时会给出提示并做兼容处理。
- 若终端仍出现中文乱码，可优先检查以下项：
  - PowerShell：在配置文件中设置 `[Console]::InputEncoding=[Text.UTF8Encoding]::UTF8` 与 `[Console]::OutputEncoding=[Text.UTF8Encoding]::UTF8`。
  - Windows Terminal：使用支持中文的等宽字体（如 `Cascadia Mono PL`、`Sarasa Mono SC`）。
  - 环境变量：若手动设置 `LANG`/`LC_ALL`，确保值包含 `UTF-8`。

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
- `/capability`
- `/editor`

其中 `/editor` 可进入内置编辑器视图。
`/capability` 当前支持缓存重置：
- `/capability reset`：按当前 provider/model 清理能力缓存。
- `/capability reset all`：清空 `.order/capabilities.json` 中全部记录。
- `/capability reset <provider>`：按 provider 清理。
- `/capability reset <provider> <model>`：按 provider + model 精确清理。
`/status` 除了近 24h 统计外，还会展示当前生效能力、缓存降级原因与 TTL 状态。

流式与中断说明：
- 正常发送消息后，响应会以增量方式实时渲染到对话区。
- 请求进行中可用 `/cancel` 中断；此时 `Ctrl+C` 也会执行“取消请求”，而不是直接退出程序。
- 当没有进行中的请求时，`Ctrl+C` 仍按原行为退出程序。
- 流式请求默认带有超时与自动重试（指数退避 + 抖动），仅在“未产出正文增量且判定为可重试错误”时触发，避免重复输出污染会话。
- 主对话界面默认支持鼠标框选历史文本进行复制；进入 `/editor` 后会临时启用鼠标捕获以支持编辑器交互。

## 对话上下文

- 同一次运行内，发送给 Codex/OpenAI 类模型的请求会自动携带三层上下文：
  - 短期上下文：最近 N 轮完整消息。
  - 中期摘要：当历史被裁剪时自动生成阶段摘要（目标/已完成/阻塞点）。
  - 长期记忆：项目规则、偏好、关键决策（持久化在 `.order/context/memory.json`）。
- 当前输入会作为独立 prompt 发送，不会在历史中重复注入。
- 错误消息与 `/history` 命令回显不会写入模型上下文，避免污染后续对话。
- 可通过环境变量 `ORDER_TASK_ID` 指定长期记忆归档任务 ID；未设置时默认使用 `default`。

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
- `default_max_turns`：可选；agent 多轮上限（`0` 表示使用系统默认值，当前默认 `12`）

当 `provider` 为 `openai` 或 `codex` 且 `support_tools = true` 时，会启用内置文件工具：

- `ReadTool`：读取工作区内文件（仅相对路径、UTF-8、大小受限）
- `WriteTool`：写入工作区内文件（仅相对路径、默认写入 LF、大小受限；需要用户确认时会弹出 `同意 / 不同意 / 同意之后一切修改` 三选项，可用 `↑/↓ + Enter` 选择）
- `SearchFileTool`：在工作区内递归搜索关键字（仅相对路径、结果数量受限，返回可直接传给 `ReadTool` 的相对路径）

补充说明：

- `openai` 与 `codex` 在未显式设置 `support_tools` 时默认启用工具调用，可通过显式设置 `support_tools = false` 关闭。
- 为提升定位源码稳定性，`SearchFileTool` 默认会跳过 `.git`、`target`、`node_modules` 等高噪声目录；如需搜索这些目录，可将 `path` 直接指定为该目录本身。

## LSP 能力说明

编辑器当前支持以下语言的 LSP 诊断、补全、语义高亮，以及基础编辑工作流（rename / format / quick fix）：

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
- 代码补全由 LSP 异步返回并在编辑器中缓存，并以光标附近的 popover 浮层展示。
- rename 支持弹窗输入新符号名并通过 `textDocument/rename` 触发跨文件编辑。
- quick fix 支持 `textDocument/codeAction` + `workspace/applyEdit` 闭环，优先执行 `quickfix` 动作。

## editor 快捷键

进入方式：在主界面输入 `/editor`。

### 通用按键

- `Ctrl + C`：退出 editor（同时结束当前程序会话）

### NORMAL 模式

- `i`：进入 `INSERT` 模式
- `v`：进入 `VISUAL` 模式
- `h/j/k/l` 或 `←/↓/↑/→`：移动光标；当焦点在目录树时用于目录树上下移动/进入
- `Enter`：目录树焦点下打开选中项；或执行待确认命令（`w` / `q`）
- `Esc`：清空当前命令缓冲并保持 `NORMAL`

### VISUAL 模式

- `Esc` 或 `v`：返回 `NORMAL` 模式
- `h/j/k/l` 或 `←/↓/↑/→`：移动光标（当前未实现选区操作，仅提供模式与导航体验）

### INSERT 模式

- `Esc` 或 `jk`：返回 `NORMAL` 模式
- `Tab`：有补全候选时确认补全；无候选时插入 4 个空格
- `Shift + Tab`：有补全候选时上移选中项
- `Backspace`：删除
- `Enter`：有补全候选时确认补全；无候选时换行
- `↑/↓`：有补全候选时切换选中项；无候选时移动光标
- `←/→`：移动光标

### TERMINAL 模式

- `Esc` 再 `Esc`：返回 `NORMAL` 模式
- `Esc` 后按 `Ctrl + n`：返回 `NORMAL` 模式

### BUFFER PICKER 模式

- `a` 到 `z`：按字母选择缓冲区
- `Esc`：取消选择并返回 `NORMAL`

### RENAME INPUT 模式

- `Enter`：确认并发送 `textDocument/rename`
- `Backspace`：删除输入
- `Esc`：取消并返回 `NORMAL`

### NORMAL 命令（直接输入，无需冒号）

| 命令 | 说明 |
| --- | --- |
| `w` | 保存当前文件 |
| `q` | 退出 editor |
| `fs` | 保存会话到 `.order_editor.session` |
| `fl` | 加载会话并刷新目录树 |
| `sv` | 垂直分屏 |
| `sp` | 水平分屏 |
| `sh` | 显示并聚焦左侧目录树 |
| `sl` | 聚焦右侧窗格（无右侧窗格时回到编辑区） |
| `sj` | 聚焦下方窗格（无下方窗格时回到编辑区） |
| `sk` | 聚焦上方主窗格 |
| `tn` | 新建 TAB |
| `tl` | 切到下一个 TAB |
| `th` | 切到上一个 TAB |
| `tc` | 关闭当前 TAB |
| `tb` | 切换目录树显示/隐藏 |
| `tt` | 切换 TagBar 显示/隐藏 |
| `te` | 进入 `TERMINAL` 模式 |
| `e` / `ff` | 进入 `BUFFER PICKER` 模式 |
| `pi` | 焦点切到目录树 |
| `pu` | 焦点切到编辑区 |
| `ci` | 补全候选上移 |
| `cu` | 补全候选下移 |
| `fa` | 搜索并跳转到光标所在单词的下一处出现位置 |
| `fh` | 在状态栏展示命令历史 |
| `fc` | 强制回到 `NORMAL` 模式 |
| `lc` | 执行 LSP 服务器可用性检查（PATH 中是否可用） |
| `lr` | 打开 LSP rename 输入框（Enter 确认，Esc 取消） |
| `lf` | 对当前文件发送 LSP format 请求 |
| `lq` | 对当前光标发送 LSP quick fix 请求 |
| `[g` | 跳到上一条诊断 |
| `]g` | 跳到下一条诊断 |
| `K` | 显示当前诊断详情 |
| `fb` | 切换 editor 主题 |
