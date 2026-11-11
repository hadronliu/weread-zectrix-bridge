//! Node 浏览器适配器子进程客户端（协议 v2）。
//!
//! 单一读取任务分发指令响应与事件；所有响应为分类型结构
//! （frame / status / ack），错误带 code / message / recoverable。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::{mpsc, oneshot, Mutex};

/// Node 侧上报事件（转发给 worker 统一处理）。
#[derive(Clone, Debug)]
pub enum NodeEvent {
    /// 适配器进程与浏览器上下文已就绪（不代表已登录）。
    Ready {
        browser_mode: String,
    },
    AuthChanged {
        logged_in: bool,
    },
    CandidatesChanged {
        candidates: Vec<Candidate>,
    },
    RendererClosed,
    Log(String),
    /// 适配器主动上报关闭（随后进程退出）。
    Closed {
        reason: String,
    },
}

use crate::state::Candidate;

/// 结构化控制器错误。
#[derive(Clone, Debug)]
pub struct NodeError {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
}

impl NodeError {
    pub fn new(code: &str, message: impl Into<String>) -> NodeError {
        NodeError {
            code: code.to_string(),
            message: message.into(),
            recoverable: true,
        }
    }
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}（{}）", self.message, self.code)
    }
}

impl NodeError {
    /// 面向用户的错误文案：不可恢复的操作额外提示。
    pub fn user_message(&self) -> String {
        if self.recoverable {
            self.to_string()
        } else {
            format!("{self}（该请求不可自动重试，请检查后重试）")
        }
    }
}

/// 类型化指令响应。
#[derive(Clone, Debug)]
pub enum NodeReply {
    /// 一帧 400×300 设备画面。
    Frame(Vec<u8>),
    /// 状态数据（候选列表 / URL 等）。
    Status(Value),
    Ack,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<NodeReply, NodeError>>>>>;

/// 通过 stdin/stdout JSON Lines 驱动的浏览器适配器客户端。
pub struct Browser {
    child: Child,
    stdin: AsyncMutex<ChildStdin>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    /// stdout EOF / 进程退出标记。
    pub exited: Arc<AtomicBool>,
}

/// 单条指令超时：LOAD_BOOK 加载书籍较慢，其余默认。
fn command_timeout(cmd: &str) -> Duration {
    match cmd {
        "LOAD_BOOK" => Duration::from_secs(70),
        _ => Duration::from_secs(35),
    }
}

impl Browser {
    /// 启动适配器；返回客户端与其事件流（Ready/AuthChanged/…）。
    pub fn spawn(
        node_bin: &str,
        controller_js: &str,
        args: &[(&str, String)],
    ) -> Result<(Browser, mpsc::Receiver<NodeEvent>), String> {
        if !std::path::Path::new(controller_js).exists() {
            return Err(format!("找不到控制器脚本：{controller_js}"));
        }
        let mut command = Command::new(node_bin);
        command.arg(controller_js);
        for (k, v) in args {
            command.arg(k).arg(v);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Playwright 的提示信息直接透传给终端。
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("启动 Node 适配器失败（{node_bin}）：{e}"))?;

        let stdin = child.stdin.take().ok_or("无法获取适配器 stdin")?;
        let stdout = child.stdout.take().ok_or("无法获取适配器 stdout")?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::channel::<NodeEvent>(256);
        let exited = Arc::new(AtomicBool::new(false));

        // 单一读取任务：分发指令响应与事件，检测子进程退出（stdout EOF）。
        tokio::spawn({
            let pending = pending.clone();
            let exited = exited.clone();
            async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            if handle_node_event(&v, &event_tx).await {
                                continue;
                            }
                            deliver_reply(&v, &pending).await;
                        }
                        _ => break,
                    }
                }
                // 子进程退出：唤醒所有等待者并广播退出。
                pending.lock().await.clear();
                exited.store(true, Ordering::SeqCst);
            }
        });

        Ok((
            Browser {
                child,
                stdin: AsyncMutex::new(stdin),
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
                exited,
            },
            event_rx,
        ))
    }

    /// 发送指令并等待类型化响应。
    pub async fn request(&self, cmd: &str, payload: Value) -> Result<NodeReply, NodeError> {
        if self.exited.load(Ordering::SeqCst) {
            return Err(NodeError::new("adapter_exited", "浏览器适配器已退出"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        // JSON Lines 协议：必须以换行符结尾，否则 Node 侧 readline 不会触发 line 事件。
        let mut line = json!({ "id": id, "cmd": cmd, "payload": payload }).to_string();
        line.push('\n');
        let written = {
            let mut stdin = self.stdin.lock().await;
            match stdin.write_all(line.as_bytes()).await {
                Ok(()) => stdin.flush().await,
                Err(e) => Err(e),
            }
        };
        if let Err(e) = written {
            self.pending.lock().await.remove(&id);
            return Err(NodeError::new(
                "write_failed",
                format!("向适配器写入失败：{e}"),
            ));
        }
        match tokio::time::timeout(command_timeout(cmd), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(NodeError::new("adapter_exited", "浏览器适配器已退出")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(NodeError::new(
                    "reply_timeout",
                    format!("适配器响应超时（{cmd}）"),
                ))
            }
        }
    }

    /// 优雅关闭：关闭 stdin 触发 Node 自行收尾，宽限期后强杀。
    pub async fn shutdown(mut self) {
        let _ = self.stdin.lock().await.shutdown().await;
        match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
            }
        }
    }
}

/// 处理 Node 侧事件；返回 true 表示该行是事件而非指令响应。
async fn handle_node_event(v: &Value, event_tx: &mpsc::Sender<NodeEvent>) -> bool {
    let Some(event) = v.get("event").and_then(Value::as_str) else {
        return false;
    };
    let node_event = match event {
        "controller_ready" => {
            let mode = v
                .get("browserMode")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            NodeEvent::Ready { browser_mode: mode }
        }
        "auth_changed" => NodeEvent::AuthChanged {
            logged_in: v.get("loggedIn").and_then(Value::as_bool).unwrap_or(false),
        },
        "candidates_changed" => {
            let mut candidates = Vec::new();
            if let Some(list) = v.get("candidates").and_then(Value::as_array) {
                for item in list {
                    let title = item.get("title").and_then(Value::as_str).unwrap_or("");
                    let url = item.get("url").and_then(Value::as_str).unwrap_or("");
                    let book_id = item.get("bookId").and_then(Value::as_str).unwrap_or("");
                    if !url.is_empty() {
                        candidates.push(Candidate {
                            title: title.to_string(),
                            url: url.to_string(),
                            book_id: book_id.to_string(),
                        });
                    }
                }
            }
            NodeEvent::CandidatesChanged { candidates }
        }
        "renderer_closed" => NodeEvent::RendererClosed,
        "log" => {
            let message = v.get("message").and_then(Value::as_str).unwrap_or("");
            NodeEvent::Log(message.to_string())
        }
        "closed" => NodeEvent::Closed {
            reason: v
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        },
        other => NodeEvent::Log(format!("未知事件：{other}")),
    };
    let _ = event_tx.send(node_event).await;
    true
}

async fn deliver_reply(v: &Value, pending: &Pending) {
    let Some(id) = v.get("id").and_then(Value::as_u64) else {
        return;
    };
    let mut map = pending.lock().await;
    let Some(tx) = map.remove(&id) else {
        return;
    };
    let reply = if v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        parse_ok_reply(v)
    } else {
        let error = v.get("error").cloned().unwrap_or(Value::Null);
        Err(NodeError {
            code: error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("未知错误")
                .to_string(),
            recoverable: error
                .get("recoverable")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        })
    };
    let _ = tx.send(reply);
}

fn parse_ok_reply(v: &Value) -> Result<NodeReply, NodeError> {
    let result = v.get("result").cloned().unwrap_or(Value::Null);
    match result.get("kind").and_then(Value::as_str) {
        Some("frame") => {
            let b64 = result.get("pngB64").and_then(Value::as_str).unwrap_or("");
            B64.decode(b64)
                .map(NodeReply::Frame)
                .map_err(|e| NodeError::new("bad_frame", format!("PNG Base64 解码失败：{e}")))
        }
        Some("status") => Ok(NodeReply::Status(result)),
        Some("ack") | None => Ok(NodeReply::Ack),
        Some(other) => Err(NodeError::new(
            "bad_reply",
            format!("未知响应类型：{other}"),
        )),
    }
}
