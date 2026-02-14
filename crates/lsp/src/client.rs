use std::{
    collections::HashMap,
    io::BufReader,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::{
    language::{LspLanguage, all_languages, detect_language},
    protocol,
    types::{
        DiagnosticItem, LspCommand, LspEvent, LspServerCapabilities, LspServerCheckItem,
        LspServerCheckReport,
    },
};

pub struct LspClient {
    sessions: HashMap<LspLanguage, LspSession>,
    status_message: String,
    last_action: String,
}

impl Default for LspClient {
    fn default() -> Self {
        Self::new()
    }
}

impl LspClient {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            status_message: "LSP 未启动".to_string(),
            last_action: "idle".to_string(),
        }
    }

    pub fn status_message(&self) -> &str {
        &self.status_message
    }

    pub fn last_action(&self) -> &str {
        &self.last_action
    }

    pub fn is_running(&self) -> bool {
        self.sessions.values().any(|session| session.running)
    }

    pub fn is_language_running(&self, language: LspLanguage) -> bool {
        self.sessions
            .get(&language)
            .is_some_and(|session| session.running)
    }

    pub fn check_server_availability(&self) -> LspServerCheckReport {
        let mut items = Vec::new();
        for language in all_languages() {
            let (binary, _) = language.server_command();
            let available = is_command_available(binary);
            items.push(LspServerCheckItem {
                language: language.display_name().to_string(),
                server_command: binary.to_string(),
                available,
                install_hint: language.install_hint().to_string(),
            });
        }
        LspServerCheckReport { items }
    }

    pub fn ensure_started_for_file(
        &mut self,
        workspace_root: &Path,
        file_path: &Path,
    ) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        self.ensure_started_for_language(workspace_root, language)
    }

    pub fn ensure_started_for_language(
        &mut self,
        workspace_root: &Path,
        language: LspLanguage,
    ) -> Result<()> {
        if let Some(session) = self.sessions.get_mut(&language)
            && session.sync_running_state()?
        {
            return Ok(());
        }

        let session = match LspSession::spawn(workspace_root, language) {
            Ok(session) => session,
            Err(error) => {
                let (binary, _) = language.server_command();
                self.status_message = format!(
                    "{} LSP 启动失败：缺少命令 `{}`。{}",
                    language.display_name(),
                    binary,
                    language.install_hint()
                );
                self.last_action = format!("spawn failed({})", language.language_id());
                return Err(error);
            }
        };
        self.sessions.insert(language, session);
        self.status_message = format!("{} 已启动", language.language_id());
        self.last_action = format!("spawn({})", language.language_id());
        Ok(())
    }

    pub fn sync_running_state(&mut self) -> Result<()> {
        let mut exited_languages = Vec::new();
        for (language, session) in &mut self.sessions {
            if !session.sync_running_state()? {
                exited_languages.push(*language);
            }
        }

        for language in exited_languages {
            self.sessions.remove(&language);
        }
        Ok(())
    }

    pub fn poll_events(&mut self) -> Vec<LspEvent> {
        let mut events = self.drain_session_events();
        for event in &events {
            match event {
                LspEvent::Status(text) => {
                    self.status_message = text.clone();
                    self.last_action = "status update".to_string();
                }
                LspEvent::PublishDiagnostics { .. } => {
                    self.last_action = "publishDiagnostics".to_string();
                }
                LspEvent::WillSaveWaitUntilEdits { edits, .. } => {
                    self.last_action = format!("willSaveWaitUntil({} edits)", edits.len());
                }
                LspEvent::CompletionItems { items, .. } => {
                    self.last_action = format!("completion({})", items.len());
                }
                LspEvent::SemanticTokens { tokens, .. } => {
                    self.last_action = format!("semanticTokens({})", tokens.len());
                }
                LspEvent::FormattingEdits { edits, .. } => {
                    self.last_action = format!("formatting({} edits)", edits.len());
                }
                LspEvent::RenameWorkspaceEdit { new_name, .. } => {
                    self.last_action = format!("rename({})", new_name);
                }
                LspEvent::CodeActions { actions, .. } => {
                    self.last_action = format!("codeAction({})", actions.len());
                }
                LspEvent::WorkspaceApplyEditRequest { request_id, .. } => {
                    self.last_action = format!("workspace/applyEdit(request:{request_id})");
                }
                LspEvent::RustAnalyzerStatus { message, done } => {
                    self.last_action = if *done {
                        format!("rust-analyzer ready({})", message)
                    } else {
                        format!("rust-analyzer loading({})", message)
                    };
                }
            }
        }

        for session in self.sessions.values_mut() {
            if !session.running {
                continue;
            }
            while let Some(event) = session.drain_legacy_events() {
                events.push(event);
            }
        }

        events
    }

    pub fn send_did_open(
        &mut self,
        workspace_root: &Path,
        file_path: &Path,
        text: &str,
        version: i32,
    ) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };

        self.ensure_started_for_language(workspace_root, language)?;
        let session = self
            .sessions
            .get_mut(&language)
            .ok_or_else(|| anyhow!("{} 会话不存在", language.language_id()))?;

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("didOpen 路径转换失败: {}", file_path.display()))?;
        let did_open = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": language.language_id(),
                    "version": version,
                    "text": text
                }
            }
        });

        session.send_or_queue_message(&did_open)?;
        self.last_action = format!("didOpen({})", language.language_id());
        Ok(())
    }

    pub fn send_did_close(&mut self, file_path: &Path) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("didClose 路径转换失败: {}", file_path.display()))?;
        let did_close = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": {
                "textDocument": { "uri": file_uri }
            }
        });
        session.send_or_queue_message(&did_close)?;
        self.last_action = format!("didClose({})", language.language_id());
        Ok(())
    }

    pub fn send_did_save(&mut self, file_path: &Path, text: &str) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("didSave 路径转换失败: {}", file_path.display()))?;
        let did_save = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didSave",
            "params": {
                "textDocument": { "uri": file_uri },
                "text": text
            }
        });
        session.send_or_queue_message(&did_save)?;
        self.last_action = format!("didSave({})", language.language_id());
        Ok(())
    }

    pub fn send_did_change(
        &mut self,
        file_path: &Path,
        old_text: &str,
        new_text: &str,
        version: i32,
    ) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("didChange 路径转换失败: {}", file_path.display()))?;

        let mut content_changes: Vec<Value> = protocol::compute_incremental_changes(
            old_text, new_text,
        )
        .into_iter()
        .map(|change| {
            serde_json::json!({
                "range": {
                    "start": { "line": change.start_line, "character": change.start_character },
                    "end": { "line": change.end_line, "character": change.end_character }
                },
                "text": change.new_text
            })
        })
        .collect();

        if content_changes.is_empty() {
            self.last_action = format!("didChange(noop:{})", language.language_id());
            return Ok(());
        }

        content_changes.reverse();

        let did_change = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "version": version
                },
                "contentChanges": content_changes
            }
        });

        session.send_or_queue_message(&did_change)?;
        self.last_action = format!("didChange({})", language.language_id());
        Ok(())
    }

    pub fn send_will_save(&mut self, file_path: &Path) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("willSave 路径转换失败: {}", file_path.display()))?;
        let will_save = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/willSave",
            "params": {
                "textDocument": { "uri": file_uri },
                "reason": 1
            }
        });

        session.send_or_queue_message(&will_save)?;
        self.last_action = format!("willSave({})", language.language_id());
        Ok(())
    }

    pub fn send_will_save_wait_until(&mut self, file_path: &Path) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        // 已确认不支持时直接跳过，避免每次保存都触发服务端错误响应。
        if !session.will_save_wait_until_supported {
            self.last_action = format!("willSaveWaitUntil(skip:{})", language.language_id());
            return Ok(());
        }

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("willSaveWaitUntil 路径转换失败: {}", file_path.display()))?;
        let request_id = session.next_request_id();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "textDocument/willSaveWaitUntil",
            "params": {
                "textDocument": { "uri": file_uri },
                "reason": 1
            }
        });

        session
            .pending_will_save_wait_until
            .insert(request_id, file_path.to_path_buf());
        session.send_or_queue_message(&request)?;
        self.last_action = format!("willSaveWaitUntil({})", language.language_id());
        Ok(())
    }

    pub fn request_completion(
        &mut self,
        file_path: &Path,
        line: usize,
        character: usize,
    ) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Err(anyhow!("{} LSP 会话不存在", language.display_name()));
        };

        if !session.running {
            return Err(anyhow!("{} LSP 会话未运行", language.display_name()));
        }

        if !session.initialized {
            return Err(anyhow!("{} LSP 正在初始化", language.display_name()));
        }

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("completion 路径转换失败: {}", file_path.display()))?;
        let request_id = session.next_request_id();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": file_uri },
                "position": {
                    "line": line,
                    "character": character
                },
                "context": {
                    "triggerKind": 1
                }
            }
        });

        session
            .pending_completion
            .insert(request_id, file_path.to_path_buf());
        session.send_or_queue_message(&request)?;
        self.last_action = format!("completion request({})", language.language_id());
        Ok(())
    }

    pub fn request_semantic_tokens(&mut self, file_path: &Path) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("semanticTokens 路径转换失败: {}", file_path.display()))?;
        let request_id = session.next_request_id();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "textDocument/semanticTokens/full",
            "params": {
                "textDocument": { "uri": file_uri }
            }
        });

        session
            .pending_semantic_tokens
            .insert(request_id, file_path.to_path_buf());
        session.send_or_queue_message(&request)?;
        self.last_action = format!("semanticTokens request({})", language.language_id());
        Ok(())
    }

    /// 请求 `textDocument/formatting`。
    pub fn request_formatting(
        &mut self,
        file_path: &Path,
        tab_size: usize,
        insert_spaces: bool,
    ) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Err(anyhow!("{} LSP 会话不存在", language.display_name()));
        };

        if !session.running {
            return Err(anyhow!("{} LSP 会话未运行", language.display_name()));
        }
        if !session.initialized {
            return Err(anyhow!("{} LSP 正在初始化", language.display_name()));
        }
        if !session.capabilities.formatting {
            return Err(anyhow!(
                "{} LSP 不支持 textDocument/formatting",
                language.display_name()
            ));
        }

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("formatting 路径转换失败: {}", file_path.display()))?;
        let request_id = session.next_request_id();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "textDocument/formatting",
            "params": {
                "textDocument": { "uri": file_uri },
                "options": {
                    "tabSize": tab_size,
                    "insertSpaces": insert_spaces
                }
            }
        });

        session
            .pending_formatting
            .insert(request_id, file_path.to_path_buf());
        session.send_or_queue_message(&request)?;
        self.last_action = format!("formatting request({})", language.language_id());
        Ok(())
    }

    /// 请求 `textDocument/rename`。
    pub fn request_rename(
        &mut self,
        file_path: &Path,
        line: usize,
        character: usize,
        new_name: &str,
    ) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Err(anyhow!("{} LSP 会话不存在", language.display_name()));
        };

        if !session.running {
            return Err(anyhow!("{} LSP 会话未运行", language.display_name()));
        }
        if !session.initialized {
            return Err(anyhow!("{} LSP 正在初始化", language.display_name()));
        }
        if !session.capabilities.rename {
            return Err(anyhow!(
                "{} LSP 不支持 textDocument/rename",
                language.display_name()
            ));
        }
        if new_name.trim().is_empty() {
            return Err(anyhow!("rename 新名称不能为空"));
        }

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("rename 路径转换失败: {}", file_path.display()))?;
        let request_id = session.next_request_id();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "textDocument/rename",
            "params": {
                "textDocument": { "uri": file_uri },
                "position": {
                    "line": line,
                    "character": character
                },
                "newName": new_name
            }
        });

        session.pending_rename.insert(
            request_id,
            PendingRename {
                file_path: file_path.to_path_buf(),
                new_name: new_name.to_string(),
            },
        );
        session.send_or_queue_message(&request)?;
        self.last_action = format!("rename request({})", language.language_id());
        Ok(())
    }

    /// 请求 `textDocument/codeAction`（仅 quick fix）。
    pub fn request_code_actions(
        &mut self,
        file_path: &Path,
        line: usize,
        character: usize,
        diagnostics: &[DiagnosticItem],
    ) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Err(anyhow!("{} LSP 会话不存在", language.display_name()));
        };

        if !session.running {
            return Err(anyhow!("{} LSP 会话未运行", language.display_name()));
        }
        if !session.initialized {
            return Err(anyhow!("{} LSP 正在初始化", language.display_name()));
        }
        if !session.capabilities.code_action {
            return Err(anyhow!(
                "{} LSP 不支持 textDocument/codeAction",
                language.display_name()
            ));
        }

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("codeAction 路径转换失败: {}", file_path.display()))?;
        let request_id = session.next_request_id();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": file_uri },
                "range": {
                    "start": {
                        "line": line,
                        "character": character
                    },
                    "end": {
                        "line": line,
                        "character": character
                    }
                },
                "context": {
                    "diagnostics": serialize_diagnostics_for_code_action(diagnostics),
                    "only": ["quickfix"],
                    "triggerKind": 1
                }
            }
        });

        session
            .pending_code_action
            .insert(request_id, file_path.to_path_buf());
        session.send_or_queue_message(&request)?;
        self.last_action = format!("codeAction request({})", language.language_id());
        Ok(())
    }

    /// 请求 `workspace/executeCommand`。
    pub fn execute_command(&mut self, file_path: &Path, command: &LspCommand) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Err(anyhow!("{} LSP 会话不存在", language.display_name()));
        };

        if !session.running {
            return Err(anyhow!("{} LSP 会话未运行", language.display_name()));
        }
        if !session.initialized {
            return Err(anyhow!("{} LSP 正在初始化", language.display_name()));
        }
        if !session.capabilities.execute_command {
            return Err(anyhow!(
                "{} LSP 不支持 workspace/executeCommand",
                language.display_name()
            ));
        }

        let request_id = session.next_request_id();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "workspace/executeCommand",
            "params": {
                "command": command.command.clone(),
                "arguments": command.arguments.clone()
            }
        });

        session.pending_execute_command.insert(
            request_id,
            PendingExecuteCommand {
                file_path: file_path.to_path_buf(),
                title: command.title.clone(),
            },
        );
        session.send_or_queue_message(&request)?;
        self.last_action = format!("executeCommand({})", language.language_id());
        Ok(())
    }

    /// 回包 `workspace/applyEdit` 请求。
    ///
    /// 这里显式返回 `applied/failureReason`，是为了让服务端明确感知客户端应用结果，
    /// 避免 quick fix 命令在服务端侧出现“已执行但客户端未落地”的状态漂移。
    pub fn respond_workspace_apply_edit(
        &mut self,
        language: LspLanguage,
        request_id: u64,
        applied: bool,
        failure_reason: Option<&str>,
    ) -> Result<()> {
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        let mut result = serde_json::json!({
            "applied": applied
        });
        if !applied && let Some(reason) = failure_reason {
            result["failureReason"] = serde_json::json!(reason);
        }

        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": result
        });
        session.send_or_queue_message(&response)?;
        self.last_action = format!("workspace/applyEdit response({request_id})");
        Ok(())
    }

    pub fn stop_all(&mut self) {
        for session in self.sessions.values_mut() {
            session.stop();
        }
        self.sessions.clear();
        self.status_message = "LSP 已停止".to_string();
        self.last_action = "stop".to_string();
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.stop_all();
    }
}

enum ReaderMessage {
    Event(LspEvent),
    Response(Value),
}

#[derive(Debug, Clone)]
struct PendingRename {
    file_path: PathBuf,
    new_name: String,
}

#[derive(Debug, Clone)]
struct PendingExecuteCommand {
    file_path: PathBuf,
    title: String,
}

#[derive(Debug, Clone, Copy)]
enum PendingRequestKind {
    WillSaveWaitUntil,
    Completion,
    SemanticTokens,
    Formatting,
    Rename,
    CodeAction,
    ExecuteCommand,
}

struct LspSession {
    language: LspLanguage,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    running: bool,
    initialized: bool,
    capabilities: LspServerCapabilities,
    request_id: u64,
    initialize_request_id: Option<u64>,
    event_rx: Option<Receiver<LspEvent>>,
    reader_rx: Receiver<ReaderMessage>,
    semantic_token_types: Vec<String>,
    semantic_token_modifiers: Vec<String>,
    pending_messages: Vec<Value>,
    /// 服务端是否支持 `textDocument/willSaveWaitUntil`。
    ///
    /// 默认按“支持”处理；若服务端返回 method-not-found，
    /// 则自动降级为不再发送该请求，避免保存时反复出现 unknown request。
    will_save_wait_until_supported: bool,
    pending_will_save_wait_until: HashMap<u64, PathBuf>,
    pending_completion: HashMap<u64, PathBuf>,
    pending_semantic_tokens: HashMap<u64, PathBuf>,
    pending_formatting: HashMap<u64, PathBuf>,
    pending_rename: HashMap<u64, PendingRename>,
    pending_code_action: HashMap<u64, PathBuf>,
    pending_execute_command: HashMap<u64, PendingExecuteCommand>,
}

impl LspSession {
    fn spawn(workspace_root: &Path, language: LspLanguage) -> Result<Self> {
        let (binary, args) = language.server_command();
        let mut command = Command::new(binary);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(workspace_root);

        let mut child = command.spawn().with_context(|| {
            format!(
                "启动 {} 失败，请确认已安装并在 PATH 中可用",
                language.language_id()
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("无法获取 {} 标准输入", language.language_id()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("无法获取 {} 标准输出", language.language_id()))?;

        let (reader_tx, reader_rx) = mpsc::channel::<ReaderMessage>();
        spawn_reader_thread(stdout, reader_tx, language);

        let mut session = Self {
            language,
            child: Some(child),
            stdin: Some(stdin),
            running: true,
            initialized: false,
            capabilities: LspServerCapabilities::default(),
            request_id: 1,
            initialize_request_id: None,
            event_rx: None,
            reader_rx,
            semantic_token_types: language
                .semantic_token_types()
                .iter()
                .map(|item| (*item).to_string())
                .collect(),
            semantic_token_modifiers: language
                .semantic_token_modifiers()
                .iter()
                .map(|item| (*item).to_string())
                .collect(),
            pending_messages: Vec::new(),
            will_save_wait_until_supported: true,
            pending_will_save_wait_until: HashMap::new(),
            pending_completion: HashMap::new(),
            pending_semantic_tokens: HashMap::new(),
            pending_formatting: HashMap::new(),
            pending_rename: HashMap::new(),
            pending_code_action: HashMap::new(),
            pending_execute_command: HashMap::new(),
        };

        session.send_initialize_sequence(workspace_root)?;
        Ok(session)
    }

    fn drain_reader_messages(&mut self) -> Vec<LspEvent> {
        let mut events = Vec::new();

        loop {
            match self.reader_rx.try_recv() {
                Ok(ReaderMessage::Event(event)) => events.push(event),
                Ok(ReaderMessage::Response(response)) => {
                    if let Some(event) = self.map_response(response) {
                        events.push(event);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.running = false;
                    break;
                }
            }
        }

        events
    }

    fn map_response(&mut self, response: Value) -> Option<LspEvent> {
        let request_id = protocol::response_request_id(&response);

        if self.initialize_request_id.is_some() && self.initialize_request_id == request_id {
            self.initialize_request_id = None;
            if let Some((token_types, token_modifiers)) =
                protocol::parse_semantic_legend_from_initialize_response(&response)
            {
                self.semantic_token_types = token_types;
                self.semantic_token_modifiers = token_modifiers;
            }
            if let Some(capabilities) =
                protocol::parse_server_capabilities_from_initialize_response(&response)
            {
                self.capabilities = capabilities;
            }

            self.initialized = true;
            if let Err(error) = self.flush_pending_messages() {
                return Some(LspEvent::Status(format!(
                    "{} 初始化后发送队列失败: {}",
                    self.language.language_id(),
                    error
                )));
            }
            return None;
        }

        let Some(request_id) = request_id else {
            return None;
        };

        let pending_kind = self.pending_request_kind(request_id);

        if let Some(error) = response.get("error") {
            self.clear_pending_request(request_id);

            // 统一处理“方法不存在”降级，避免后续重复误调用。
            if let Some(kind) = pending_kind
                && is_method_not_found_error(error)
            {
                self.disable_capability_for_missing_method(kind);

                // willSaveWaitUntil 是保存链路上的可选能力，降级后无需打断用户流程。
                if matches!(kind, PendingRequestKind::WillSaveWaitUntil) {
                    return None;
                }
            }

            let error_msg = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            return Some(LspEvent::Status(format!(
                "[LSP错误] 请求 {} 失败: {}",
                request_id, error_msg
            )));
        }

        if !protocol::has_result(&response) {
            return None;
        }

        if let Some(file_path) = self.pending_will_save_wait_until.remove(&request_id) {
            return Some(LspEvent::WillSaveWaitUntilEdits {
                file_path,
                edits: protocol::parse_text_edits_from_response(&response),
            });
        }

        if let Some(file_path) = self.pending_completion.remove(&request_id) {
            let items = protocol::parse_completion_items_from_response(&response);
            return Some(LspEvent::CompletionItems { file_path, items });
        }

        if let Some(file_path) = self.pending_semantic_tokens.remove(&request_id) {
            return Some(LspEvent::SemanticTokens {
                file_path,
                tokens: protocol::parse_semantic_tokens_from_response(
                    &response,
                    &self.semantic_token_types,
                    &self.semantic_token_modifiers,
                ),
            });
        }

        if let Some(file_path) = self.pending_formatting.remove(&request_id) {
            return Some(LspEvent::FormattingEdits {
                file_path,
                edits: protocol::parse_text_edits_from_response(&response),
            });
        }

        if let Some(pending) = self.pending_rename.remove(&request_id) {
            return Some(LspEvent::RenameWorkspaceEdit {
                file_path: pending.file_path,
                new_name: pending.new_name,
                edit: protocol::parse_workspace_edit_from_response(&response),
            });
        }

        if let Some(file_path) = self.pending_code_action.remove(&request_id) {
            return Some(LspEvent::CodeActions {
                file_path,
                actions: protocol::parse_code_actions_from_response(&response),
            });
        }

        if let Some(pending) = self.pending_execute_command.remove(&request_id) {
            return Some(LspEvent::Status(format!(
                "{} 命令已执行：{}（{}）",
                self.language.display_name(),
                pending.title,
                pending.file_path.display()
            )));
        }

        None
    }

    /// 判断请求 id 对应的待处理请求类型。
    fn pending_request_kind(&self, request_id: u64) -> Option<PendingRequestKind> {
        if self.pending_will_save_wait_until.contains_key(&request_id) {
            return Some(PendingRequestKind::WillSaveWaitUntil);
        }
        if self.pending_completion.contains_key(&request_id) {
            return Some(PendingRequestKind::Completion);
        }
        if self.pending_semantic_tokens.contains_key(&request_id) {
            return Some(PendingRequestKind::SemanticTokens);
        }
        if self.pending_formatting.contains_key(&request_id) {
            return Some(PendingRequestKind::Formatting);
        }
        if self.pending_rename.contains_key(&request_id) {
            return Some(PendingRequestKind::Rename);
        }
        if self.pending_code_action.contains_key(&request_id) {
            return Some(PendingRequestKind::CodeAction);
        }
        if self.pending_execute_command.contains_key(&request_id) {
            return Some(PendingRequestKind::ExecuteCommand);
        }
        None
    }

    /// 清理指定请求 id 的 pending 状态。
    fn clear_pending_request(&mut self, request_id: u64) {
        self.pending_will_save_wait_until.remove(&request_id);
        self.pending_completion.remove(&request_id);
        self.pending_semantic_tokens.remove(&request_id);
        self.pending_formatting.remove(&request_id);
        self.pending_rename.remove(&request_id);
        self.pending_code_action.remove(&request_id);
        self.pending_execute_command.remove(&request_id);
    }

    /// 遇到 method-not-found 时按请求类型做能力降级。
    fn disable_capability_for_missing_method(&mut self, kind: PendingRequestKind) {
        match kind {
            PendingRequestKind::WillSaveWaitUntil => {
                self.will_save_wait_until_supported = false;
            }
            PendingRequestKind::Formatting => {
                self.capabilities.formatting = false;
            }
            PendingRequestKind::Rename => {
                self.capabilities.rename = false;
            }
            PendingRequestKind::CodeAction => {
                self.capabilities.code_action = false;
            }
            PendingRequestKind::ExecuteCommand => {
                self.capabilities.execute_command = false;
            }
            PendingRequestKind::Completion | PendingRequestKind::SemanticTokens => {}
        }
    }

    fn send_initialize_sequence(&mut self, workspace_root: &Path) -> Result<()> {
        let root_uri = protocol::path_to_file_uri(workspace_root)
            .with_context(|| format!("工作区路径无法转换为 URI: {}", workspace_root.display()))?;

        let initialize_request_id = self.next_request_id();
        self.initialize_request_id = Some(initialize_request_id);

        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": initialize_request_id,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "clientInfo": {
                    "name": "order-editor",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "rootUri": root_uri,
                "capabilities": {
                    "workspace": {
                        "applyEdit": true,
                        "workspaceEdit": {
                            "documentChanges": true
                        }
                    },
                    "textDocument": {
                        "completion": {
                            "completionItem": {
                                "snippetSupport": true
                            }
                        },
                        "codeAction": {
                            "dynamicRegistration": false,
                            "codeActionLiteralSupport": {
                                "codeActionKind": {
                                    "valueSet": [
                                        "quickfix",
                                        "refactor",
                                        "source"
                                    ]
                                }
                            }
                        },
                        "rename": {
                            "dynamicRegistration": false
                        },
                        "formatting": {
                            "dynamicRegistration": false
                        },
                        "semanticTokens": {
                            "dynamicRegistration": false,
                            "requests": {
                                "full": true
                            },
                            "tokenTypes": self.language.semantic_token_types(),
                            "tokenModifiers": self.language.semantic_token_modifiers(),
                            "formats": ["relative"]
                        }
                    }
                },
                "workspaceFolders": []
            }
        });
        self.send_message(&initialize)?;

        let initialized = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        self.send_message(&initialized)
    }

    fn send_message(&mut self, value: &Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("{} stdin 不可用", self.language.language_id()))?;
        protocol::send_message(stdin, value)
    }

    fn send_or_queue_message(&mut self, value: &Value) -> Result<()> {
        if self.initialized {
            return self.send_message(value);
        }

        self.pending_messages.push(value.clone());
        Ok(())
    }

    fn flush_pending_messages(&mut self) -> Result<()> {
        if !self.initialized || self.pending_messages.is_empty() {
            return Ok(());
        }

        let pending_messages = std::mem::take(&mut self.pending_messages);
        for message in pending_messages {
            self.send_message(&message)?;
        }

        Ok(())
    }

    fn next_request_id(&mut self) -> u64 {
        let request_id = self.request_id;
        self.request_id = self.request_id.saturating_add(1);
        request_id
    }

    fn sync_running_state(&mut self) -> Result<bool> {
        let Some(child) = self.child.as_mut() else {
            self.running = false;
            return Ok(false);
        };

        match child
            .try_wait()
            .with_context(|| format!("检查 {} 状态失败", self.language.language_id()))?
        {
            Some(_) => {
                self.running = false;
                self.child = None;
                self.stdin = None;
                Ok(false)
            }
            None => {
                self.running = true;
                Ok(true)
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        self.event_rx = None;
        self.running = false;
    }

    fn drain_legacy_events(&mut self) -> Option<LspEvent> {
        let rx = self.event_rx.as_ref()?;
        match rx.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.event_rx = None;
                self.running = false;
                None
            }
        }
    }
}

fn spawn_reader_thread(
    stdout: std::process::ChildStdout,
    reader_tx: Sender<ReaderMessage>,
    language: LspLanguage,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);

        loop {
            let message = match protocol::read_next_message(&mut reader) {
                Ok(Some(value)) => value,
                Ok(None) => {
                    let _ = reader_tx.send(ReaderMessage::Event(LspEvent::Status(format!(
                        "{} 输出流关闭",
                        language.language_id()
                    ))));
                    return;
                }
                Err(error) => {
                    let _ = reader_tx.send(ReaderMessage::Event(LspEvent::Status(format!(
                        "{} 输出读取失败: {}",
                        language.language_id(),
                        error
                    ))));
                    return;
                }
            };

            if protocol::is_publish_diagnostics(&message) {
                let (file_path, items) = protocol::parse_publish_diagnostics(&message);
                if let Some(file_path) = file_path {
                    let _ = reader_tx.send(ReaderMessage::Event(LspEvent::PublishDiagnostics {
                        file_path,
                        items,
                    }));
                }
                continue;
            }

            if protocol::is_progress_notification(&message)
                && language == LspLanguage::Rust
                && let Some((message, done)) = protocol::parse_rust_analyzer_progress(&message)
            {
                let _ = reader_tx.send(ReaderMessage::Event(LspEvent::RustAnalyzerStatus {
                    message,
                    done,
                }));
                continue;
            }

            if protocol::is_workspace_apply_edit_request(&message)
                && let Some((request_id, label, edit)) =
                    protocol::parse_workspace_apply_edit_request(&message)
            {
                let _ = reader_tx.send(ReaderMessage::Event(LspEvent::WorkspaceApplyEditRequest {
                    language,
                    request_id,
                    label,
                    edit,
                }));
                continue;
            }

            // 仅把“method 为空的消息”视为 response，避免把服务端请求误判进响应分支。
            if protocol::response_request_id(&message).is_some() && message.get("method").is_none()
            {
                let _ = reader_tx.send(ReaderMessage::Response(message));
            }
        }
    });
}

impl LspClient {
    fn drain_session_events(&mut self) -> Vec<LspEvent> {
        let mut events = Vec::new();
        for session in self.sessions.values_mut() {
            events.extend(session.drain_reader_messages());
        }
        events
    }
}

/// 将内部诊断结构转换为 `codeAction` 上下文所需的 LSP 诊断 JSON。
///
/// quick fix 能力高度依赖诊断上下文，若只传空数组，服务端通常只能返回很少的动作。
fn serialize_diagnostics_for_code_action(diagnostics: &[DiagnosticItem]) -> Vec<Value> {
    diagnostics
        .iter()
        .map(|item| {
            let mut diagnostic = serde_json::json!({
                "range": {
                    "start": {
                        "line": item.lsp_start_line,
                        "character": item.lsp_start_character
                    },
                    "end": {
                        "line": item.lsp_end_line,
                        "character": item.lsp_end_character
                    }
                },
                "severity": item.severity.to_lsp_number(),
                "message": item.message.clone()
            });
            if let Some(source) = item.source.as_deref() {
                diagnostic["source"] = serde_json::json!(source);
            }
            if let Some(code) = item.code.as_deref() {
                diagnostic["code"] = serde_json::json!(code);
            }
            diagnostic
        })
        .collect()
}

/// 判断错误是否属于“方法不存在”。
///
/// 优先使用标准 JSON-RPC code `-32601`，并兼容常见字符串错误文本，
/// 以覆盖不同语言服务器的实现差异。
fn is_method_not_found_error(error: &Value) -> bool {
    if error
        .get("code")
        .and_then(Value::as_i64)
        .is_some_and(|code| code == -32601)
    {
        return true;
    }

    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    message.contains("method not found") || message.contains("unknown request")
}

fn is_command_available(command: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        Command::new("where")
            .arg(command)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(not(target_os = "windows"))]
    {
        Command::new("which")
            .arg(command)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf, sync::mpsc};

    use serde_json::json;

    use super::{LspEvent, LspLanguage, LspServerCapabilities, LspSession, ReaderMessage};

    fn build_minimal_session() -> LspSession {
        let (_reader_tx, reader_rx) = mpsc::channel::<ReaderMessage>();
        let mut pending_semantic_tokens = HashMap::new();
        pending_semantic_tokens.insert(2, PathBuf::from("main.rs"));

        LspSession {
            language: LspLanguage::Rust,
            child: None,
            stdin: None,
            running: true,
            initialized: false,
            capabilities: LspServerCapabilities {
                rename: true,
                code_action: true,
                formatting: true,
                execute_command: true,
            },
            request_id: 3,
            initialize_request_id: Some(1),
            event_rx: None,
            reader_rx,
            semantic_token_types: vec!["type".to_string(), "function".to_string()],
            semantic_token_modifiers: vec!["declaration".to_string()],
            pending_messages: Vec::new(),
            will_save_wait_until_supported: true,
            pending_will_save_wait_until: HashMap::new(),
            pending_completion: HashMap::new(),
            pending_semantic_tokens,
            pending_formatting: HashMap::new(),
            pending_rename: HashMap::new(),
            pending_code_action: HashMap::new(),
            pending_execute_command: HashMap::new(),
        }
    }

    #[test]
    fn initialize_legend_should_drive_semantic_token_mapping() {
        let mut session = build_minimal_session();
        session.initialize_request_id = Some(1);

        let initialize_response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "capabilities": {
                    "semanticTokensProvider": {
                        "legend": {
                            "tokenTypes": ["macro", "parameter"],
                            "tokenModifiers": ["documentation"]
                        }
                    }
                }
            }
        });

        assert!(session.map_response(initialize_response).is_none());
        assert_eq!(
            session.semantic_token_types,
            vec!["macro".to_string(), "parameter".to_string()]
        );

        let semantic_response = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "data": [
                    0, 0, 5, 0, 1
                ]
            }
        });

        let event = session
            .map_response(semantic_response)
            .expect("semanticTokens 响应应被映射为上层事件");

        match event {
            LspEvent::SemanticTokens { file_path, tokens } => {
                assert_eq!(file_path, PathBuf::from("main.rs"));
                assert_eq!(tokens.len(), 1);
                assert_eq!(tokens[0].token_type, "macro");
                assert_eq!(tokens[0].token_modifiers, vec!["documentation"]);
            }
            _ => panic!("返回事件类型错误，期望 SemanticTokens"),
        }
    }

    #[test]
    fn initialize_response_should_mark_session_initialized() {
        let mut session = build_minimal_session();

        let initialize_response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "capabilities": {
                    "semanticTokensProvider": {
                        "legend": {
                            "tokenTypes": ["macro"],
                            "tokenModifiers": []
                        }
                    }
                }
            }
        });

        assert!(session.map_response(initialize_response).is_none());
        assert!(session.initialized);
    }

    #[test]
    fn initialize_response_should_capture_server_capabilities() {
        let mut session = build_minimal_session();

        let initialize_response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "capabilities": {
                    "renameProvider": true,
                    "codeActionProvider": {
                        "codeActionKinds": ["quickfix"]
                    },
                    "documentFormattingProvider": false,
                    "executeCommandProvider": {
                        "commands": ["rust-analyzer.applySourceChange"]
                    },
                    "semanticTokensProvider": {
                        "legend": {
                            "tokenTypes": ["macro"],
                            "tokenModifiers": []
                        }
                    }
                }
            }
        });

        assert!(session.map_response(initialize_response).is_none());
        assert!(session.capabilities.rename);
        assert!(session.capabilities.code_action);
        assert!(!session.capabilities.formatting);
        assert!(session.capabilities.execute_command);
    }

    #[test]
    fn will_save_wait_until_unknown_request_should_disable_request() {
        let mut session = build_minimal_session();
        session
            .pending_will_save_wait_until
            .insert(9, PathBuf::from("main.rs"));

        let response = json!({
            "jsonrpc": "2.0",
            "id": 9,
            "error": {
                "code": -32601,
                "message": "unknown request: textDocument/willSaveWaitUntil"
            }
        });

        let event = session.map_response(response);
        assert!(event.is_none());
        assert!(!session.will_save_wait_until_supported);
        assert!(!session.pending_will_save_wait_until.contains_key(&9));
    }

    #[test]
    fn formatting_unknown_request_should_disable_capability() {
        let mut session = build_minimal_session();
        session
            .pending_formatting
            .insert(7, PathBuf::from("main.rs"));

        let response = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "error": {
                "code": -32601,
                "message": "method not found"
            }
        });

        let _ = session.map_response(response);
        assert!(!session.capabilities.formatting);
        assert!(!session.pending_formatting.contains_key(&7));
    }
}
