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
    types::{LspEvent, LspServerCheckItem, LspServerCheckReport},
};

/// 澶氳瑷€ LSP 瀹㈡埛绔€?///
/// 鏍稿績鑱岃矗锛?/// - 鎸夎瑷€鎯版€ф媺璧疯瑷€鏈嶅姟鍣紝闄嶄綆璧勬簮鍗犵敤锛?/// - 缁熶竴灏佽 didOpen/didChange/琛ュ叏/璇箟楂樹寒 绛夎兘鍔涳紱
/// - 灏?LSP 鍘熷鍗忚缁嗚妭闅旂鍦?crate 鍐咃紝鍚戜笂灞傛彁渚涚ǔ瀹氫簨浠躲€?
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
    /// 鍒涘缓绌哄鎴风锛屼笉浼氱珛鍗冲惎鍔ㄤ换浣曡瑷€鏈嶅姟銆?    
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            status_message: "LSP 未启动".to_string(),
            last_action: "idle".to_string(),
        }
    }

    /// 鑾峰彇鐘舵€佹爮鏂囨湰銆?    
    pub fn status_message(&self) -> &str {
        &self.status_message
    }

    /// 鑾峰彇鏈€杩戝姩浣滄弿杩般€?    
    pub fn last_action(&self) -> &str {
        &self.last_action
    }

    /// 鏄惁瀛樺湪鑷冲皯涓€涓椿璺冧細璇濄€?    
    pub fn is_running(&self) -> bool {
        self.sessions.values().any(|session| session.running)
    }

    /// 判断指定语言会话是否正在运行。
    ///
    /// 该能力用于上层在“自动激活”场景下做幂等控制，
    /// 避免每一帧都重复发送 `didOpen`。
    pub fn is_language_running(&self, language: LspLanguage) -> bool {
        self.sessions
            .get(&language)
            .is_some_and(|session| session.running)
    }

    /// 妫€鏌ユ墍鏈夊彈鏀寔璇█鏈嶅姟鍣ㄦ槸鍚﹀彲鐢ㄣ€?    ///
    /// 璇ュ嚱鏁颁笉浼氬惎鍔ㄦ湇鍔″櫒杩涚▼锛屽彧閫氳繃绯荤粺 PATH 鍙墽琛屾€у仛蹇€熸帰娴嬨€?    
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

    /// 鏍规嵁鏂囦欢璺緞纭繚璇█浼氳瘽宸插惎鍔ㄣ€?    
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

    /// 纭繚鎸囧畾璇█浼氳瘽宸插惎鍔ㄣ€?    
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

    /// 鍚屾浼氳瘽瀛樻椿鐘舵€併€?    
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

    /// 鎷夊彇鎵€鏈変細璇濅簨浠躲€?    
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

    /// 鍙戦€?`textDocument/didOpen`銆?    
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
            .with_context(|| format!("didOpen 璺緞杞崲澶辫触: {}", file_path.display()))?;
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

    /// 鍙戦€?`textDocument/didClose`銆?    
    pub fn send_did_close(&mut self, file_path: &Path) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("didClose 璺緞杞崲澶辫触: {}", file_path.display()))?;
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

    /// 鍙戦€?`textDocument/didSave`銆?    
    pub fn send_did_save(&mut self, file_path: &Path, text: &str) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("didSave 璺緞杞崲澶辫触: {}", file_path.display()))?;
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

    /// 鍙戦€?`textDocument/didChange`銆?    
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
            .with_context(|| format!("didChange 璺緞杞崲澶辫触: {}", file_path.display()))?;

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

        // 閲囩敤鈥滀粠鍚庡埌鍓嶁€濈殑缂栬緫椤哄簭锛岄伩鍏嶅尯闂存紓绉汇€?        content_changes.reverse();

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

    /// 鍙戦€?`textDocument/willSave`銆?    
    pub fn send_will_save(&mut self, file_path: &Path) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("willSave 璺緞杞崲澶辫触: {}", file_path.display()))?;
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

    /// 鍙戦€?`textDocument/willSaveWaitUntil` 璇锋眰銆?    
    pub fn send_will_save_wait_until(&mut self, file_path: &Path) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        let file_uri = protocol::path_to_file_uri(file_path).with_context(|| {
            format!(
                "willSaveWaitUntil 璺緞杞崲澶辫触: {}",
                file_path.display()
            )
        })?;
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

    /// 璇锋眰琛ュ叏銆?    
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
            return Ok(());
        };

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("completion 璺緞杞崲澶辫触: {}", file_path.display()))?;
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

    /// 璇锋眰璇箟楂樹寒 token銆?    
    pub fn request_semantic_tokens(&mut self, file_path: &Path) -> Result<()> {
        let Some(language) = detect_language(file_path) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get_mut(&language) else {
            return Ok(());
        };

        let file_uri = protocol::path_to_file_uri(file_path)
            .with_context(|| format!("semanticTokens 璺緞杞崲澶辫触: {}", file_path.display()))?;
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

    /// 鍋滄鍏ㄩ儴浼氳瘽銆?    
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

/// 鍐呴儴绾跨▼娑堟伅銆?
enum ReaderMessage {
    Event(LspEvent),
    Response(Value),
}

/// 鍗曡瑷€浼氳瘽銆?
struct LspSession {
    language: LspLanguage,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    running: bool,
    initialized: bool,
    request_id: u64,
    initialize_request_id: Option<u64>,
    event_rx: Option<Receiver<LspEvent>>,
    reader_rx: Receiver<ReaderMessage>,
    semantic_token_types: Vec<String>,
    semantic_token_modifiers: Vec<String>,
    pending_messages: Vec<Value>,
    pending_will_save_wait_until: HashMap<u64, PathBuf>,
    pending_completion: HashMap<u64, PathBuf>,
    pending_semantic_tokens: HashMap<u64, PathBuf>,
}

impl LspSession {
    /// 鍚姩浼氳瘽骞跺畬鎴愬垵濮嬪寲銆?    
    fn spawn(workspace_root: &Path, language: LspLanguage) -> Result<Self> {
        let (binary, args) = language.server_command();
        let mut command = Command::new(binary);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
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
            .ok_or_else(|| anyhow!("鏃犳硶鑾峰彇 {} 鏍囧噯杈撳叆", language.language_id()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("鏃犳硶鑾峰彇 {} 鏍囧噯杈撳嚭", language.language_id()))?;

        let (reader_tx, reader_rx) = mpsc::channel::<ReaderMessage>();
        spawn_reader_thread(stdout, reader_tx, language);

        let (_event_tx, event_rx) = mpsc::channel::<LspEvent>();

        let mut session = Self {
            language,
            child: Some(child),
            stdin: Some(stdin),
            running: true,
            initialized: false,
            request_id: 1,
            initialize_request_id: None,
            event_rx: Some(event_rx),
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
            pending_will_save_wait_until: HashMap::new(),
            pending_completion: HashMap::new(),
            pending_semantic_tokens: HashMap::new(),
        };

        session.send_initialize_sequence(workspace_root)?;
        Ok(session)
    }

    /// 鍒锋柊璇诲彇绾跨▼娑堟伅锛屽苟鏄犲皠涓洪珮灞備簨浠躲€?    
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

    /// 灏嗗搷搴旀槧灏勪负楂樺眰浜嬩欢銆?    
    fn map_response(&mut self, response: Value) -> Option<LspEvent> {
        let request_id = protocol::response_request_id(&response)?;

        if self.initialize_request_id == Some(request_id) {
            self.initialize_request_id = None;
            if let Some((token_types, token_modifiers)) =
                protocol::parse_semantic_legend_from_initialize_response(&response)
            {
                self.semantic_token_types = token_types;
                self.semantic_token_modifiers = token_modifiers;
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
            return Some(LspEvent::CompletionItems {
                file_path,
                items: protocol::parse_completion_items_from_response(&response),
            });
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

        None
    }

    /// 鍙戦€佸垵濮嬪寲鎻℃墜銆?    
    fn send_initialize_sequence(&mut self, workspace_root: &Path) -> Result<()> {
        let root_uri = protocol::path_to_file_uri(workspace_root).with_context(|| {
            format!(
                "宸ヤ綔鍖鸿矾寰勬棤娉曡浆鎹负 URI: {}",
                workspace_root.display()
            )
        })?;

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
                    "textDocument": {
                        "completion": {
                            "completionItem": {
                                "snippetSupport": true
                            }
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

    /// 鍙戦€佸崟鏉℃秷鎭€?
    fn send_message(&mut self, value: &Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("{} stdin 不可用", self.language.language_id()))?;
        protocol::send_message(stdin, value)
    }

    /// 在初始化完成前缓存消息，避免服务端尚未就绪时丢失 didOpen/semantic 请求。
    fn send_or_queue_message(&mut self, value: &Value) -> Result<()> {
        if self.initialized {
            return self.send_message(value);
        }

        self.pending_messages.push(value.clone());
        Ok(())
    }

    /// 初始化成功后按原顺序回放缓存消息，保证编辑状态与服务端一致。
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

    /// 鍒嗛厤璇锋眰 id銆?
    fn next_request_id(&mut self) -> u64 {
        let request_id = self.request_id;
        self.request_id = self.request_id.saturating_add(1);
        request_id
    }

    /// 鍚屾杩涚▼鐘舵€併€?    
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

    /// 鍋滄浼氳瘽銆?    
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        self.event_rx = None;
        self.running = false;
    }

    /// 鍏煎浜嬩欢閫氶亾鐨勯潪闃诲璇诲彇銆?    
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

/// 鍚姩璇诲彇绾跨▼锛岃礋璐ｅ崗璁В鍖呬笌鍩虹鍒嗙被銆?
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
                        "{} 杈撳嚭璇诲彇澶辫触: {}",
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

            if protocol::response_request_id(&message).is_some() {
                let _ = reader_tx.send(ReaderMessage::Response(message));
            }
        }
    });
}

impl LspClient {
    /// 浠庡悇浼氳瘽璇诲彇绾跨▼涓彁鍙栧苟鍚堝苟浜嬩欢銆?    
    fn drain_session_events(&mut self) -> Vec<LspEvent> {
        let mut events = Vec::new();
        for session in self.sessions.values_mut() {
            events.extend(session.drain_reader_messages());
        }
        events
    }
}

/// 鍒ゆ柇鍛戒护鏄惁鍙湪褰撳墠绯荤粺 PATH 涓墽琛屻€?///
/// 浣跨敤骞冲彴鍘熺敓鍛戒护锛圵indows: `where`锛屽叾浠? `which`锛夎繘琛屾娴嬶紝
/// 鍙互閬垮厤鐩存帴灏濊瘯鍚姩 LSP 杩涚▼甯︽潵鐨勫壇浣滅敤銆?
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

    use super::{LspEvent, LspLanguage, LspSession, ReaderMessage};

    /// 鏋勯€犱竴涓渶灏忎細璇濓紝鐢ㄤ簬楠岃瘉 initialize 涓?semanticTokens 鐨勫搷搴斾覆鑱斻€?    ///
    /// 杩欓噷涓嶅惎鍔ㄧ湡瀹炶瑷€鏈嶅姟锛岃€屾槸鐩存帴鍠傚叆 JSON-RPC 鍝嶅簲锛?    /// 杩欐牱鍙互绋冲畾澶嶇幇鈥渓egend 鏄犲皠閿欒瀵艰嚧楂樹寒澶辨晥鈥濈殑鍥炲綊鍦烘櫙銆?    
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
            request_id: 3,
            initialize_request_id: Some(1),
            event_rx: None,
            reader_rx,
            semantic_token_types: vec!["type".to_string(), "function".to_string()],
            semantic_token_modifiers: vec!["declaration".to_string()],
            pending_messages: Vec::new(),
            pending_will_save_wait_until: HashMap::new(),
            pending_completion: HashMap::new(),
            pending_semantic_tokens,
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
            _ => panic!("杩斿洖浜嬩欢绫诲瀷閿欒锛屾湡鏈?SemanticTokens"),
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
}
