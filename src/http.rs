//! 本地管理 HTTP 服务：仅绑定 127.0.0.1。
//!
//! 安全约束（设计文档 §协议与接口）：
//! - 校验 Host 头，防 DNS 重绑定；
//! - /api/* 校验 Origin（存在时必须同源）与会话令牌（Cookie 或 X-Auth-Token），
//!   拒绝跨站写请求；
//! - 不做任意代理，不开放任意 URL 导航。
//!
//! 实现为最小 HTTP/1.1（Connection: close），不引入额外依赖。

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, RwLock};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

use crate::state::App;

/// 需要经 worker 串行队列执行的 API 操作。
#[derive(Clone, Debug)]
pub enum ApiOp {
    /// 打开/聚焦书架浏览页。
    Library,
    /// 投送指定阅读 URL 到设备。
    Cast {
        url: String,
    },
    /// 翻页/刷新（与设备物理按键同一路径）。
    Navigation {
        action: String,
    },
    Pause,
    Resume,
    /// 设置修改（原始 JSON，worker 校验字段）。
    Settings(Value),
    /// 重连设备（异步执行，立即返回受理）。
    ReconnectDevice,
    /// 重启浏览器适配器。
    RestartBrowser,
}

/// 需要经 worker 处理的工作项（设备按键 / API 调用 / 适配器事件与退出通知）。
/// gen 为浏览器适配器代数：重启后旧代消息被 worker 忽略。
pub enum Work {
    Key {
        gen: u64,
        key: zectrix_sdk::Key,
    },
    Api {
        op: ApiOp,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Node {
        gen: u64,
        event: crate::browser::NodeEvent,
    },
    AdapterExited {
        gen: u64,
    },
}

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 32 * 1024;
/// worker 处理超时（LOAD_BOOK 可能较慢）。
const API_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

enum Reply {
    Json(u16, Value),
    Png(Vec<u8>),
    Html(String, String), // body, set_cookie
}

/// 启动管理服务；返回后阻塞当前任务直至监听失败。
pub async fn serve(
    port: u16,
    token: &str,
    state: Arc<RwLock<App>>,
    work_tx: mpsc::Sender<Work>,
    index_html: &'static str,
) -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    println!("[bridge] 管理页：http://127.0.0.1:{port}");
    let token = token.to_string();
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(ok) => ok,
            Err(e) => {
                eprintln!("[bridge] 管理服务 accept 失败：{e}");
                continue;
            }
        };
        let state = state.clone();
        let work_tx = work_tx.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                handle_connection(stream, port, &token, state, work_tx, index_html),
            )
            .await;
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    port: u16,
    token: &str,
    state: Arc<RwLock<App>>,
    work_tx: mpsc::Sender<Work>,
    index_html: &'static str,
) {
    let mut reader = BufReader::new(stream);
    let Some(req) = read_request(&mut reader).await else {
        return;
    };
    // Host 校验：仅接受本机回环地址形式，防 DNS 重绑定。
    let host_ok = req
        .headers
        .get("host")
        .map(|h| {
            let h = h.to_ascii_lowercase();
            h == format!("127.0.0.1:{port}")
                || h == format!("localhost:{port}")
                || h == "127.0.0.1"
                || h == "localhost"
        })
        .unwrap_or(false);
    if !host_ok {
        write_reply(
            reader.get_mut(),
            Reply::Json(403, json!({"ok": false, "error": "非法 Host"})),
        )
        .await;
        return;
    }

    let reply = route(&req, port, token, state, work_tx, index_html).await;
    write_reply(reader.get_mut(), reply).await;
}

async fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut head = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    // 逐字节读到 \r\n\r\n（本地单用户场景，简单优先）。
    loop {
        match reader.read(&mut byte).await {
            Ok(0) => return None,
            Ok(_) => {
                head.push(byte[0]);
                if head.len() > MAX_HEADER_BYTES {
                    return None;
                }
                if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    let head_text = String::from_utf8_lossy(&head).to_string();
    let mut lines = head_text.lines();
    let request_line = lines.next()?.to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_uppercase();
    let target = parts.next()?.to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let (path, query) = split_target(&target);
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return None;
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.ok()?;
    }
    Some(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, query_str) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    let mut query = HashMap::new();
    for pair in query_str.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(url_decode(k), url_decode(v));
    }
    (url_decode(&path), query)
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn same_origin(origin: &str, port: u16) -> bool {
    origin == format!("http://127.0.0.1:{port}")
        || origin == format!("http://localhost:{port}")
        || origin == "http://127.0.0.1"
        || origin == "http://localhost"
}

fn authorized(req: &Request, token: &str) -> bool {
    if req.headers.get("x-auth-token").map(String::as_str) == Some(token) {
        return true;
    }
    if let Some(cookie) = req.headers.get("cookie") {
        for pair in cookie.split(';') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=') {
                if k.trim() == "wzb_auth" && v.trim() == token {
                    return true;
                }
            }
        }
    }
    if req.query.get("token").map(String::as_str) == Some(token) {
        return true;
    }
    false
}

async fn route(
    req: &Request,
    port: u16,
    token: &str,
    state: Arc<RwLock<App>>,
    work_tx: mpsc::Sender<Work>,
    index_html: &'static str,
) -> Reply {
    // 跨站请求防御：浏览器跨站发起的写请求会带 Origin，必须同源。
    if req.path.starts_with("/api/") {
        if let Some(origin) = req.headers.get("origin") {
            if origin != "null" && !same_origin(origin, port) {
                return Reply::Json(403, json!({"ok": false, "error": "跨站请求已拒绝"}));
            }
        }
        if !authorized(req, token) {
            return Reply::Json(
                401,
                json!({"ok": false, "error": "未授权（令牌缺失或不匹配）"}),
            );
        }
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => Reply::Html(
            index_html.to_string(),
            format!("wzb_auth={token}; Path=/; SameSite=Strict"),
        ),
        ("GET", "/api/state") => {
            let snapshot = state.read().unwrap().snapshot();
            Reply::Json(200, json!({"ok": true, "state": snapshot}))
        }
        ("GET", "/api/preview") => {
            let frame = state.read().unwrap().last_frame.clone();
            match frame {
                Some(png) => Reply::Png(png),
                None => Reply::Json(404, json!({"ok": false, "error": "尚无已确认画面"})),
            }
        }
        ("POST", "/api/library") => call_worker(work_tx, ApiOp::Library).await,
        ("POST", "/api/cast") => {
            let Some(url) = parse_body_url(&req.body) else {
                return Reply::Json(
                    400,
                    json!({"ok": false, "error": "请求体需要 {\"url\": ...}"}),
                );
            };
            call_worker(work_tx, ApiOp::Cast { url }).await
        }
        ("POST", "/api/navigation") => {
            let Ok(value) = serde_json::from_slice::<Value>(&req.body) else {
                return Reply::Json(400, json!({"ok": false, "error": "请求体不是合法 JSON"}));
            };
            let action = value
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !["next", "prev", "refresh"].contains(&action.as_str()) {
                return Reply::Json(
                    400,
                    json!({"ok": false, "error": "action 必须是 next|prev|refresh"}),
                );
            }
            call_worker(work_tx, ApiOp::Navigation { action }).await
        }
        ("POST", "/api/pause") => call_worker(work_tx, ApiOp::Pause).await,
        ("POST", "/api/resume") => call_worker(work_tx, ApiOp::Resume).await,
        ("POST", "/api/settings") => {
            let Ok(value) = serde_json::from_slice::<Value>(&req.body) else {
                return Reply::Json(400, json!({"ok": false, "error": "请求体不是合法 JSON"}));
            };
            call_worker(work_tx, ApiOp::Settings(value)).await
        }
        ("POST", "/api/device/reconnect") => call_worker(work_tx, ApiOp::ReconnectDevice).await,
        ("POST", "/api/browser/restart") => call_worker(work_tx, ApiOp::RestartBrowser).await,
        _ => Reply::Json(404, json!({"ok": false, "error": "接口不存在"})),
    }
}

fn parse_body_url(body: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<Value>(body).ok()?;
    let url = value.get("url").and_then(Value::as_str)?;
    if url.is_empty() {
        None
    } else {
        Some(url.to_string())
    }
}

async fn call_worker(work_tx: mpsc::Sender<Work>, op: ApiOp) -> Reply {
    let (tx, rx) = oneshot::channel();
    if work_tx.send(Work::Api { op, reply: tx }).await.is_err() {
        return Reply::Json(503, json!({"ok": false, "error": "服务正在退出"}));
    }
    match tokio::time::timeout(API_WAIT, rx).await {
        Ok(Ok(Ok(value))) => Reply::Json(200, value),
        Ok(Ok(Err(e))) => Reply::Json(409, json!({"ok": false, "error": e})),
        Ok(Err(_)) => Reply::Json(503, json!({"ok": false, "error": "处理通道已关闭"})),
        Err(_) => Reply::Json(504, json!({"ok": false, "error": "处理超时"})),
    }
}

async fn write_reply(stream: &mut TcpStream, reply: Reply) {
    let (status, content_type, extra, body): (u16, &str, Option<String>, Vec<u8>) = match reply {
        Reply::Json(status, value) => {
            let body = value.to_string().into_bytes();
            (status, "application/json; charset=utf-8", None, body)
        }
        Reply::Png(png) => (200, "image/png", None, png),
        Reply::Html(html, cookie) => {
            let body = html.into_bytes();
            (
                200,
                "text/html; charset=utf-8",
                Some(format!("Set-Cookie: {cookie}\r\nCache-Control: no-store")),
                body,
            )
        }
    };
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(extra) = extra {
        head.push_str(&extra);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&body).await;
    let _ = stream.shutdown().await;
}

/// 生成 32 字符十六进制会话令牌（/dev/urandom；macOS 目标平台）。
pub fn generate_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    // 兜底：时间戳 + 进程 id + 计数（仅本机回环使用）。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:016x}{:016x}", std::process::id() as u64)
}
