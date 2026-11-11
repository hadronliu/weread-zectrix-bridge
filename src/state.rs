//! 应用状态、阅读会话与设置持久化。
//!
//! 设备连接状态与登录状态独立记录；投送会话带自增 id，
//! 换书/排版变化都会产生新会话，旧会话的迟到帧不允许覆盖新书画面。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// 设备画面格式（对应 zectrix-sdk ImageFormat）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputFormat {
    Mono,
    Gray4,
    Gray16,
}

impl OutputFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Mono => "mono",
            OutputFormat::Gray4 => "gray4",
            OutputFormat::Gray16 => "gray16",
        }
    }

    pub fn parse(s: &str) -> Option<OutputFormat> {
        match s {
            "mono" => Some(OutputFormat::Mono),
            "gray4" => Some(OutputFormat::Gray4),
            "gray16" => Some(OutputFormat::Gray16),
            _ => None,
        }
    }

    pub fn to_sdk(self) -> zectrix_sdk::ImageFormat {
        match self {
            OutputFormat::Mono => zectrix_sdk::ImageFormat::Mono,
            OutputFormat::Gray4 => zectrix_sdk::ImageFormat::Gray4,
            OutputFormat::Gray16 => zectrix_sdk::ImageFormat::Gray16,
        }
    }
}

/// 可调设置：随管理页实时修改并持久化。
#[derive(Clone, Debug)]
pub struct Settings {
    pub format: OutputFormat,
    pub delay_ms: u64,
    /// 灰度拉伸黑点；0 表示保留原始截图。
    pub black_point: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            format: OutputFormat::Gray4,
            delay_ms: 200,
            black_point: 200,
        }
    }
}

/// 最后一次成功投送的书籍（重启后自动恢复“同书”，不承诺精确段落）。
#[derive(Clone, Debug)]
pub struct LastCast {
    pub url: String,
    pub title: String,
}

/// 持久化文件内容：settings.json。
#[derive(Clone, Debug)]
pub struct Persisted {
    pub settings: Settings,
    pub last_cast: Option<LastCast>,
}

impl Default for Persisted {
    fn default() -> Self {
        Persisted {
            settings: Settings::default(),
            last_cast: None,
        }
    }
}

/// 会话生命周期：loading → active ↔ paused。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionState {
    Loading,
    Active,
    Paused,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Loading => "loading",
            SessionState::Active => "active",
            SessionState::Paused => "paused",
        }
    }
}

/// 一次投送会话：与设备当前绑定书籍一一对应。
#[derive(Clone, Debug)]
pub struct Session {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub state: SessionState,
    pub error: Option<String>,
}

/// 候选阅读标签（来自浏览器浏览页上下文）。
#[derive(Clone, Debug)]
pub struct Candidate {
    pub title: String,
    pub url: String,
    pub book_id: String,
}

/// 全局共享状态：仅通过短临界区同步读写，不在持锁时 await。
#[derive(Debug)]
pub struct App {
    pub browser_alive: bool,
    pub browser_mode: String,
    pub device_connected: bool,
    pub device_desc: Option<String>,
    pub device_error: Option<String>,
    /// None = 尚未探测到登录态。
    pub logged_in: Option<bool>,
    pub session: Option<Session>,
    pub session_seq: u64,
    pub candidates: Vec<Candidate>,
    /// 最新一次“设备已确认”的画面，用于管理页预览；断线时保留并标注。
    pub last_frame: Option<Vec<u8>>,
    pub settings: Settings,
    pub last_cast: Option<LastCast>,
    /// settings.json 路径；None 表示不落盘（测试等场景）。
    pub persist_path: Option<PathBuf>,
}

impl App {
    pub fn new(
        settings: Settings,
        last_cast: Option<LastCast>,
        persist_path: Option<PathBuf>,
    ) -> App {
        App {
            browser_alive: false,
            browser_mode: String::new(),
            device_connected: false,
            device_desc: None,
            device_error: None,
            logged_in: None,
            session: None,
            session_seq: 0,
            candidates: Vec::new(),
            last_frame: None,
            settings,
            last_cast,
            persist_path,
        }
    }

    /// 生成新的会话 id 并登记 Loading 状态。
    pub fn begin_session(&mut self, url: &str, title: &str) -> Session {
        self.session_seq += 1;
        let session = Session {
            id: self.session_seq,
            url: url.to_string(),
            title: title.to_string(),
            state: SessionState::Loading,
            error: None,
        };
        self.session = Some(session.clone());
        session
    }

    /// 会话切换失败时恢复旧会话（保持设备绑定不变）。
    pub fn restore_session(&mut self, previous: Option<Session>, error: String) {
        if let Some(prev) = previous {
            println!("[bridge] 新书加载失败，恢复旧会话 #{}：{error}", prev.id);
            self.session = Some(prev);
        } else if let Some(cur) = self.session.as_mut() {
            cur.state = SessionState::Paused;
            cur.error = Some(error);
        }
    }

    /// 渲染页关闭：会话进入待恢复（保留最后预览）。
    pub fn session_renderer_closed(&mut self) {
        if let Some(session) = self.session.as_mut() {
            if session.state == SessionState::Active {
                session.state = SessionState::Paused;
                session.error = Some("渲染页已关闭".into());
            }
        }
    }

    /// 持久化设置与最后投送书籍；失败仅记录，不影响运行。
    pub fn persist(&self) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };
        let last_cast = self.last_cast.as_ref().map(|c| {
            json!({
                "url": c.url,
                "title": c.title,
            })
        });
        let value = json!({
            "format": self.settings.format.as_str(),
            "delayMs": self.settings.delay_ms,
            "blackPoint": self.settings.black_point,
            "lastCast": last_cast,
        });
        if let Err(e) = write_json_file(path, &value) {
            println!("[bridge] 写入设置失败（{}）：{e}", path.display());
        }
    }

    /// 状态快照（管理页轮询）。
    pub fn snapshot(&self) -> Value {
        json!({
            "browser": {
                "alive": self.browser_alive,
                "mode": self.browser_mode,
            },
            "device": {
                "connected": self.device_connected,
                "desc": self.device_desc,
                "error": self.device_error,
            },
            "auth": { "loggedIn": self.logged_in },
            "session": self.session.as_ref().map(|s| json!({
                "id": s.id,
                "url": s.url,
                "title": s.title,
                "state": s.state.as_str(),
                "error": s.error,
            })),
            "candidates": self.candidates.iter().map(|c| json!({
                "title": c.title,
                "url": c.url,
                "bookId": c.book_id,
            })).collect::<Vec<_>>(),
            "settings": {
                "format": self.settings.format.as_str(),
                "delayMs": self.settings.delay_ms,
                "blackPoint": self.settings.black_point,
            },
        })
    }
}

/// 读取 settings.json；损坏或缺省时回退默认值。
pub fn load_persisted(path: &Path) -> Persisted {
    let mut persisted = Persisted::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return persisted;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        println!("[bridge] settings.json 解析失败，使用默认设置");
        return persisted;
    };
    if let Some(format) = value.get("format").and_then(Value::as_str) {
        if let Some(f) = OutputFormat::parse(format) {
            persisted.settings.format = f;
        }
    }
    if let Some(ms) = value.get("delayMs").and_then(Value::as_u64) {
        persisted.settings.delay_ms = ms.min(5000);
    }
    if let Some(bp) = value.get("blackPoint").and_then(Value::as_u64) {
        persisted.settings.black_point = (bp.min(249)) as u32;
    }
    if let Some(cast) = value.get("lastCast") {
        let url = cast.get("url").and_then(Value::as_str).unwrap_or("");
        let title = cast.get("title").and_then(Value::as_str).unwrap_or("");
        if !url.is_empty() {
            persisted.last_cast = Some(LastCast {
                url: url.to_string(),
                title: title.to_string(),
            });
        }
    }
    persisted
}

fn write_json_file(path: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).unwrap_or_default(),
    )
}
