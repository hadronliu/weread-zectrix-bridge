//! weread-zectrix-bridge v2：微信读书网页版 ↔ ZECTRIX Note 4 管理服务。
//!
//! 架构（设计文档 §推荐架构）：
//! ```text
//! 本地管理页(HTTP) → Rust 会话管理器 → JSON Lines v2 → Node 浏览器适配器
//! 设备实体按键 ────→ Rust 同一命令队列 ──→ 专用渲染页 → PNG → SDK → 设备
//! ```
//!
//! Rust 是设备连接、会话版本与命令队列的唯一所有者；Node 负责登录窗口、
//! 浏览页、渲染页与截图。默认关闭预取；换书产生新 sessionId，
//! 旧会话的迟到帧不允许覆盖新书画面。

mod browser;
mod http;
mod state;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

use browser::{Browser, NodeEvent, NodeReply};
use http::{ApiOp, Work};
use state::{App, LastCast, OutputFormat, Persisted, SessionState};
use zectrix_sdk::{Device, DisconnectReason, Event, Key, KeyAction, OpenOptions};

/// 管理页与静态资源。
const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/web/index.html"));
/// 运行时设置文件默认路径（随工程分发目录）。
const DEFAULT_SETTINGS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/settings.json");
/// 设备 Busy 时按任务约定的重试间隔。
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(300);
/// 快速连按的合并窗口。
const KEY_COALESCE: Duration = Duration::from_millis(90);

// ---------------------------------------------------------------- 参数

struct Args {
    ip: Option<String>,
    format: Option<OutputFormat>,
    delay_ms: Option<u64>,
    black_point: Option<u32>,
    browser_mode: String,
    http_port: u16,
    node_bin: String,
    controller_js: PathBuf,
    font_size: u32,
    settings_path: PathBuf,
}

fn usage() -> String {
    format!(
        "用法：weread-zectrix-bridge [选项]\n\
         \x20 --ip <addr>            极趣设备 IPv4 地址（省略则暂不连接设备）\n\
         \x20 --format <fmt>         mono|gray4|gray16，默认取设置文件（gray4）\n\
         \x20 --delay-ms <n>         翻页等待毫秒数，默认取设置文件（200）\n\
         \x20 --black-point <n>      灰度拉伸黑点 0~249，默认取设置文件（200）\n\
         \x20 --browser <mode>       auto|chrome|chromium，默认 auto（系统 Chrome 优先）\n\
         \x20 --http-port <n>        管理页端口，默认 8765\n\
         \x20 --node-bin <path>      node 可执行文件，默认取 PATH 中的 node\n\
         \x20 --controller <path>    controller.js 路径，默认随本工程分发\n\
         \x20 --settings <path>      settings.json 路径\n\
         \x20 --font-size <px>       渲染页强制字号（0 为不覆盖）\n\
         \x20 --prefetch/--no-prefetch  兼容旧脚本：统一默认关闭预取，两者均为空操作\n\
         示例：CMAKE_POLICY_VERSION_MINIMUM=3.5 cargo run -- --ip 192.168.1.100"
    )
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        ip: None,
        format: None,
        delay_ms: None,
        black_point: None,
        browser_mode: "auto".to_string(),
        http_port: 8765,
        node_bin: "node".to_string(),
        controller_js: PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/browser-controller/controller.js"
        )),
        font_size: 0,
        settings_path: PathBuf::from(DEFAULT_SETTINGS),
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let mut value = || -> Result<String, String> {
            i += 1;
            argv.get(i)
                .cloned()
                .ok_or_else(|| format!("缺少 {arg} 的参数值\n{}", usage()))
        };
        match arg {
            "--ip" => args.ip = Some(value()?),
            "--format" => {
                let v = value()?;
                args.format = Some(
                    OutputFormat::parse(&v)
                        .ok_or(format!("不支持的图像格式 {v}（可选 mono|gray4|gray16）"))?,
                );
            }
            "--delay-ms" => {
                args.delay_ms = Some(
                    value()?
                        .parse()
                        .map_err(|_| format!("--delay-ms 需为非负整数\n{}", usage()))?,
                );
            }
            "--black-point" => {
                let n: u32 = value()?
                    .parse()
                    .map_err(|_| format!("--black-point 需为 0~249\n{}", usage()))?;
                if n >= 250 {
                    return Err("--black-point 需为 0~249".into());
                }
                args.black_point = Some(n);
            }
            "--browser" => {
                let v = value()?;
                if !["auto", "chrome", "chromium"].contains(&v.as_str()) {
                    return Err("--browser 只能是 auto|chrome|chromium".into());
                }
                args.browser_mode = v;
            }
            "--http-port" => {
                args.http_port = value()?
                    .parse()
                    .map_err(|_| format!("--http-port 需为端口号\n{}", usage()))?;
            }
            "--node-bin" => args.node_bin = value()?,
            "--controller" => args.controller_js = PathBuf::from(value()?),
            "--settings" => args.settings_path = PathBuf::from(value()?),
            "--font-size" => {
                args.font_size = value()?
                    .parse()
                    .map_err(|_| format!("--font-size 需为非负整数\n{}", usage()))?;
            }
            // 兼容旧脚本：统一默认关闭预取。
            "--prefetch" | "--no-prefetch" => {}
            // 兼容旧版本：字号档位由微信读书自身设置管理。
            "--font-level" => {
                let _ = value()?;
            }
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("未知参数 {other}\n{}", usage())),
        }
        i += 1;
    }
    Ok(args)
}

/// 校验阅读 URL：HTTPS + 精确域名 weread.qq.com + /web/reader/<非空书籍ID>。
/// 返回规范化后的 URL；不使用字符串 includes 判断域名。
fn validate_reader_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 2048 {
        return None;
    }
    let rest = raw.strip_prefix("https://weread.qq.com")?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return None; // 形如 https://weread.qq.com.evil.com
    }
    let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
    let path = &rest[..path_end];
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    if segments.next()? != "web" {
        return None;
    }
    if segments.next()? != "reader" {
        return None;
    }
    let book_id = segments.next()?;
    if book_id.is_empty() || !book_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some(raw.to_string())
}

// ---------------------------------------------------------------- 设备连接

type DeviceSlot = Arc<std::sync::Mutex<Option<Arc<Device>>>>;

/// 连接设备；成功后启动事件泵（唯一 next_event 消费者）。
async fn connect_device(
    ip: String,
    slot: DeviceSlot,
    state: Arc<RwLock<App>>,
    work_tx: mpsc::Sender<Work>,
) {
    {
        let mut app = state.write().unwrap();
        app.device_error = None;
    }
    println!("[bridge] 正在连接设备 {ip} …");
    match Device::open(OpenOptions {
        target: Some(ip),
        client_name: Some("WeRead Bridge".into()),
    })
    .await
    {
        Ok(device) => {
            let device = Arc::new(device);
            let desc = format!(
                "{}（{}）@ {}",
                device.summary.name, device.summary.id, device.summary.address
            );
            println!("[bridge] 已连接极趣设备：{desc}");
            {
                let mut app = state.write().unwrap();
                app.device_connected = true;
                app.device_desc = Some(desc);
                app.device_error = None;
            }
            *slot.lock().unwrap() = Some(device.clone());
            tokio::spawn(device_event_pump(device, slot, state, work_tx));
        }
        Err(e) => {
            eprintln!("[bridge] 连接设备失败：{e}");
            let mut app = state.write().unwrap();
            app.device_connected = false;
            app.device_error = Some(e.to_string());
        }
    }
}

/// 设备事件泵：按键经合并窗口转发到命令队列；断开则清空设备槽位。
async fn device_event_pump(
    device: Arc<Device>,
    slot: DeviceSlot,
    state: Arc<RwLock<App>>,
    work_tx: mpsc::Sender<Work>,
) {
    loop {
        let event = match device.next_event().await {
            Ok(Some(event)) => event,
            Ok(None) => {
                report_disconnect(&device, &slot, &state, DisconnectReason::Closed);
                return;
            }
            Err(e) => {
                eprintln!("[bridge] 读取设备事件失败：{e}");
                report_disconnect(&device, &slot, &state, DisconnectReason::Failed);
                return;
            }
        };
        match event {
            Event::Key {
                key,
                action: KeyAction::Short,
            } => {
                // 合并窗口：快速连按只保留最后一次方向。
                let mut merged = key;
                loop {
                    match tokio::time::timeout(KEY_COALESCE, device.next_event()).await {
                        Ok(Ok(Some(Event::Key {
                            key: k,
                            action: KeyAction::Short,
                        }))) => merged = k,
                        Ok(Ok(Some(Event::Disconnected { reason }))) => {
                            report_disconnect(&device, &slot, &state, reason);
                            let _ = work_tx
                                .send(Work::Key {
                                    gen: 0,
                                    key: merged,
                                })
                                .await;
                            return;
                        }
                        Ok(_) => continue,
                        Err(_elapsed) => break, // 合并窗口结束
                    }
                }
                if work_tx
                    .send(Work::Key {
                        gen: 0,
                        key: merged,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Event::Key {
                key,
                action: KeyAction::Long,
            } => {
                println!("[bridge] 忽略长按事件：{key:?} Long");
            }
            Event::RecordingStarted => println!("[bridge] 设备开始录音（忽略）"),
            Event::RecordingFinished { audio, reason } => {
                let frames = audio.map(|c| c.samples.len()).unwrap_or(0);
                println!("[bridge] 设备录音结束：{reason:?}（{frames} 帧，忽略）");
            }
            Event::Disconnected { reason } => {
                report_disconnect(&device, &slot, &state, reason);
                return;
            }
        }
    }
}

/// 设备断开：仅当槽位仍是同一连接时清空，避免误清新连接。
fn report_disconnect(
    device: &Arc<Device>,
    slot: &DeviceSlot,
    state: &Arc<RwLock<App>>,
    reason: DisconnectReason,
) {
    println!("[bridge] 设备连接结束：{reason:?}");
    let mut guard = slot.lock().unwrap();
    let same = guard
        .as_ref()
        .map(|current| Arc::ptr_eq(current, device))
        .unwrap_or(false);
    if same {
        *guard = None;
    }
    drop(guard);
    let mut app = state.write().unwrap();
    app.device_connected = false;
    app.device_desc = None;
}

// ---------------------------------------------------------------- worker

/// 浏览器适配器启动参数（重启时复用）。
#[derive(Clone)]
struct AdapterConfig {
    node_bin: String,
    controller_js: PathBuf,
    browser_mode: String,
    font_size: u32,
}

/// 会话管理 worker：统一命令队列的唯一消费者。
/// 设备按键、管理页 API、适配器事件在此串行处理。
struct Worker {
    state: Arc<RwLock<App>>,
    device_slot: DeviceSlot,
    device_ip: Option<String>,
    config: AdapterConfig,
    work_tx: mpsc::Sender<Work>,
    shutdown_rx: watch::Receiver<bool>,
    browser: Option<Browser>,
    /// 当前适配器代数：重启递增，旧代事件/退出通知被忽略。
    generation: u64,
    /// 适配器就绪后自动恢复上次投送（每次代数更新重置一次机会）。
    auto_cast_attempted: bool,
}

impl Worker {
    async fn run(mut self, mut work_rx: mpsc::Receiver<Work>) {
        // 启动浏览器适配器；失败不终止服务，管理页可重启。
        if let Err(e) = self.spawn_browser().await {
            eprintln!("[bridge] {e}（可在管理页重启浏览器）");
        }
        loop {
            tokio::select! {
                work = work_rx.recv() => {
                    let Some(work) = work else { break };
                    self.handle_work(work, &mut work_rx).await;
                }
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        if let Some(browser) = self.browser.take() {
            browser.shutdown().await;
        }
        println!("[bridge] worker 已退出");
    }

    /// 处理一件工作；返回前可能丢弃长任务期间积压的按键。
    async fn handle_work(&mut self, work: Work, work_rx: &mut mpsc::Receiver<Work>) {
        match work {
            Work::Key { gen, key } => {
                if gen != 0 && gen != self.generation {
                    return; // 旧适配器代数的残留事件
                }
                let label = match key {
                    Key::Down => "下一页",
                    Key::Up => "上一页",
                    Key::Confirm => "当前页刷新",
                };
                let cmd = match key {
                    Key::Down => "NEXT_PAGE",
                    Key::Up => "PREV_PAGE",
                    Key::Confirm => "REFRESH",
                };
                if let Err(e) = self.navigate(cmd, label).await {
                    println!("[bridge] 按键处理失败：{e}");
                }
            }
            Work::Api { op, reply } => {
                let is_cast = matches!(op, ApiOp::Cast { .. });
                let result = self.handle_api(op).await;
                let _ = reply.send(result);
                if is_cast {
                    // 投送耗时较长：丢弃期间积压的设备按键，避免误翻新书。
                    while let Ok(Work::Key { .. }) = work_rx.try_recv() {}
                }
            }
            Work::Node { gen, event } => {
                if gen != self.generation {
                    return;
                }
                self.handle_node_event(event);
                self.maybe_auto_cast().await;
            }
            Work::AdapterExited { gen } => {
                if gen != self.generation {
                    return;
                }
                println!("[bridge] 浏览器适配器已退出（管理页可重启）");
                self.browser = None;
                let mut app = self.state.write().unwrap();
                app.browser_alive = false;
                // 会话随适配器失效；保留最后投送记录与预览供恢复。
                app.session = None;
            }
        }
    }

    /// 启动（或重启）浏览器适配器，并挂接事件转发。
    async fn spawn_browser(&mut self) -> Result<(), String> {
        if let Some(old) = self.browser.take() {
            old.shutdown().await;
        }
        self.generation += 1;
        let gen = self.generation;
        let (browser, node_rx) = Browser::spawn(
            &self.config.node_bin,
            self.config.controller_js.to_str().unwrap_or_default(),
            &[
                ("--browser", self.config.browser_mode.clone()),
                ("--font-size", self.config.font_size.to_string()),
            ],
        )?;
        self.browser = Some(browser);
        self.auto_cast_attempted = false;
        {
            let mut app = self.state.write().unwrap();
            app.browser_alive = true;
        }
        // 事件转发：Node 事件并入统一命令队列；流关闭视为适配器退出。
        let forward_tx = self.work_tx.clone();
        tokio::spawn(async move {
            let mut node_rx = node_rx;
            while let Some(event) = node_rx.recv().await {
                if forward_tx.send(Work::Node { gen, event }).await.is_err() {
                    return;
                }
            }
            let _ = forward_tx.send(Work::AdapterExited { gen }).await;
        });
        Ok(())
    }

    fn handle_node_event(&mut self, event: NodeEvent) {
        match event {
            NodeEvent::Ready { browser_mode } => {
                println!("[browser] 适配器就绪（浏览器模式：{browser_mode}）");
                let mut app = self.state.write().unwrap();
                app.browser_alive = true;
                app.browser_mode = browser_mode;
            }
            NodeEvent::AuthChanged { logged_in } => {
                let mut app = self.state.write().unwrap();
                if app.logged_in != Some(logged_in) {
                    println!(
                        "[browser] 登录状态：{}",
                        if logged_in { "已登录" } else { "未登录" }
                    );
                }
                app.logged_in = Some(logged_in);
            }
            NodeEvent::CandidatesChanged { candidates } => {
                let titles: Vec<&str> = candidates.iter().map(|c| c.title.as_str()).collect();
                println!(
                    "[browser] 候选阅读标签：{} 本{}",
                    candidates.len(),
                    if titles.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", titles.join("、"))
                    }
                );
                self.state.write().unwrap().candidates = candidates;
            }
            NodeEvent::RendererClosed => {
                println!("[browser] 渲染页已关闭，会话进入待恢复");
                self.state.write().unwrap().session_renderer_closed();
            }
            NodeEvent::Log(message) => println!("[browser] {message}"),
            NodeEvent::Closed { reason } => {
                println!("[browser] 适配器上报关闭（{reason}）");
            }
        }
    }

    /// 适配器就绪后自动恢复上次投送的书籍（一次性尝试）。
    async fn maybe_auto_cast(&mut self) {
        if self.auto_cast_attempted {
            return;
        }
        let (has_session, last_cast) = {
            let app = self.state.read().unwrap();
            (app.session.is_some(), app.last_cast.clone())
        };
        if has_session || last_cast.is_none() {
            self.auto_cast_attempted = true;
            return;
        }
        self.auto_cast_attempted = true;
        let cast = last_cast.unwrap();
        println!("[bridge] 自动恢复上次投送：{}", cast.title);
        if let Err(e) = self.cast(&cast.url, Some(cast.title.clone())).await {
            println!("[bridge] 自动恢复失败：{e}（可在管理页手动投送）");
        }
    }

    /// 会话内导航（设备按键与管理页按钮共用同一路径）。
    async fn navigate(&mut self, cmd: &str, label: &str) -> Result<(), String> {
        let (active, delay_ms, black_point) = {
            let app = self.state.read().unwrap();
            let active = app
                .session
                .as_ref()
                .map(|s| s.state == SessionState::Active)
                .unwrap_or(false);
            (active, app.settings.delay_ms, app.settings.black_point)
        };
        if !active {
            return Err("当前没有进行中的投送会话（先投送书籍或恢复会话）".into());
        }
        let Some(browser) = self.browser.as_ref() else {
            return Err("浏览器适配器未运行".into());
        };
        let payload = json!({ "delayMs": delay_ms, "blackPoint": black_point });
        match browser.request(cmd, payload).await {
            Ok(NodeReply::Frame(png)) => {
                let started = Instant::now();
                self.push_frame(png).await?;
                println!(
                    "[bridge] {label} 完成，本轮耗时 {}ms",
                    started.elapsed().as_millis()
                );
                Ok(())
            }
            Ok(NodeReply::Status(v)) => {
                println!("[bridge] {label} 返回状态响应：{v}");
                Ok(())
            }
            Ok(NodeReply::Ack) => Ok(()),
            Err(e) => Err(e.user_message()),
        }
    }

    /// 推送一帧并确认；失败时会话暂停等待人工恢复（不盲目再翻页）。
    async fn push_frame(&mut self, png: Vec<u8>) -> Result<(), String> {
        let format = self.state.read().unwrap().settings.format;
        let device = self.device_slot.lock().unwrap().clone();
        let Some(device) = device else {
            // 无设备：画面仅入预览，会话视为进行中。
            let mut app = self.state.write().unwrap();
            app.last_frame = Some(png);
            if let Some(session) = app.session.as_mut() {
                session.state = SessionState::Active;
                session.error = None;
            }
            println!("[bridge] 设备未连接，画面仅更新预览");
            return Ok(());
        };
        match push_image(&device, &png, format).await {
            Ok(()) => {
                let mut app = self.state.write().unwrap();
                app.last_frame = Some(png);
                if let Some(session) = app.session.as_mut() {
                    session.state = SessionState::Active;
                    session.error = None;
                }
                Ok(())
            }
            Err(e) => {
                let message = format!("推送失败：{e}");
                println!("[bridge] {message}，会话暂停（浏览器与设备画面可能不一致）");
                let mut app = self.state.write().unwrap();
                if let Some(session) = app.session.as_mut() {
                    session.state = SessionState::Paused;
                    session.error = Some(message.clone());
                }
                Err(message)
            }
        }
    }

    /// 投送书籍：新会话 → 渲染页加载 → 首帧确认 → 更新绑定。
    async fn cast(&mut self, url: &str, title_hint: Option<String>) -> Result<(), String> {
        let Some(url) = validate_reader_url(url) else {
            return Err("非法阅读 URL：必须是 https://weread.qq.com/web/reader/<书籍ID>".into());
        };
        let title = {
            let app = self.state.read().unwrap();
            app.candidates
                .iter()
                .find(|c| c.url == url)
                .map(|c| c.title.clone())
                .or_else(|| {
                    app.last_cast
                        .as_ref()
                        .filter(|c| c.url == url)
                        .map(|c| c.title.clone())
                })
                .or(title_hint)
                .unwrap_or_else(|| "未命名书籍".to_string())
        };
        let previous = self.state.read().unwrap().session.clone();
        {
            let mut app = self.state.write().unwrap();
            app.begin_session(&url, &title);
            println!("[bridge] 开始投送会话 #{}：{title}", app.session_seq);
        }
        let Some(browser) = self.browser.as_ref() else {
            let message = "浏览器适配器未运行".to_string();
            self.state
                .write()
                .unwrap()
                .restore_session(previous, message.clone());
            return Err(message);
        };
        match browser.request("LOAD_BOOK", json!({ "url": url })).await {
            Ok(NodeReply::Frame(png)) => {
                if let Err(e) = self.push_frame(png).await {
                    return Err(e);
                }
                let mut app = self.state.write().unwrap();
                app.last_cast = Some(LastCast {
                    url: url.clone(),
                    title: title.clone(),
                });
                app.persist();
                println!("[bridge] 已投送：{title}");
                Ok(())
            }
            Ok(_) => {
                let message = "适配器返回了非画面响应".to_string();
                self.state
                    .write()
                    .unwrap()
                    .restore_session(previous, message.clone());
                Err(message)
            }
            Err(e) => {
                let message = e.user_message();
                self.state
                    .write()
                    .unwrap()
                    .restore_session(previous, message.clone());
                Err(message)
            }
        }
    }

    /// 恢复会话：重新截取当前渲染页并推送（重同步浏览器与设备）。
    async fn resume(&mut self) -> Result<(), String> {
        {
            let app = self.state.read().unwrap();
            match app.session.as_ref() {
                None => return Err("没有可恢复的会话".into()),
                Some(s) if s.state == SessionState::Loading => return Err("会话正在加载中".into()),
                _ => {}
            }
        }
        // 渲染页可能已关闭：按最后投送书籍完整重投。
        let renderer_gone = {
            let app = self.state.read().unwrap();
            app.session
                .as_ref()
                .and_then(|s| s.error.as_ref())
                .map(|e| e.contains("渲染页"))
                .unwrap_or(false)
        };
        if renderer_gone {
            let last = self.state.read().unwrap().last_cast.clone();
            if let Some(cast) = last {
                return self.cast(&cast.url, Some(cast.title)).await;
            }
            return Err("渲染页已关闭且没有可恢复的投送记录".into());
        }
        let Some(browser) = self.browser.as_ref() else {
            return Err("浏览器适配器未运行".into());
        };
        let black_point = self.state.read().unwrap().settings.black_point;
        match browser
            .request("CAPTURE", json!({ "blackPoint": black_point }))
            .await
        {
            Ok(NodeReply::Frame(png)) => {
                self.push_frame(png).await?;
                println!("[bridge] 会话已恢复并重新同步");
                Ok(())
            }
            Ok(_) => Err("适配器返回了非画面响应".into()),
            Err(e) => Err(e.user_message()),
        }
    }

    async fn handle_api(&mut self, op: ApiOp) -> Result<Value, String> {
        match op {
            ApiOp::Library => {
                let Some(browser) = self.browser.as_ref() else {
                    return Err("浏览器适配器未运行".into());
                };
                match browser.request("OPEN_LIBRARY", json!({})).await {
                    Ok(_) => Ok(json!({"ok": true})),
                    Err(e) => Err(e.user_message()),
                }
            }
            ApiOp::Cast { url } => {
                self.cast(&url, None).await?;
                let (id, title) = {
                    let app = self.state.read().unwrap();
                    app.session
                        .as_ref()
                        .map(|s| (s.id, s.title.clone()))
                        .unwrap_or((0, String::new()))
                };
                Ok(json!({"ok": true, "sessionId": id, "title": title}))
            }
            ApiOp::Navigation { action } => {
                let (cmd, label) = match action.as_str() {
                    "next" => ("NEXT_PAGE", "下一页"),
                    "prev" => ("PREV_PAGE", "上一页"),
                    "refresh" => ("REFRESH", "当前页刷新"),
                    _ => return Err("action 必须是 next|prev|refresh".into()),
                };
                self.navigate(cmd, label).await?;
                Ok(json!({"ok": true}))
            }
            ApiOp::Pause => {
                let mut app = self.state.write().unwrap();
                if let Some(session) = app.session.as_mut() {
                    if session.state == SessionState::Active {
                        session.state = SessionState::Paused;
                        println!("[bridge] 会话已暂停（保留连接与画面）");
                    }
                }
                Ok(json!({"ok": true}))
            }
            ApiOp::Resume => {
                self.resume().await?;
                Ok(json!({"ok": true}))
            }
            ApiOp::Settings(patch) => {
                let mut app = self.state.write().unwrap();
                if let Some(format) = patch.get("format").and_then(Value::as_str) {
                    let Some(f) = OutputFormat::parse(format) else {
                        return Err("format 只能是 mono|gray4|gray16".into());
                    };
                    app.settings.format = f;
                }
                if let Some(ms) = patch.get("delayMs").and_then(Value::as_u64) {
                    if ms > 5000 {
                        return Err("delayMs 需在 0~5000".into());
                    }
                    app.settings.delay_ms = ms;
                }
                if let Some(bp) = patch.get("blackPoint").and_then(Value::as_u64) {
                    if bp >= 250 {
                        return Err("blackPoint 需在 0~249".into());
                    }
                    app.settings.black_point = bp as u32;
                }
                app.persist();
                Ok(json!({
                    "ok": true,
                    "settings": {
                        "format": app.settings.format.as_str(),
                        "delayMs": app.settings.delay_ms,
                        "blackPoint": app.settings.black_point,
                    }
                }))
            }
            ApiOp::ReconnectDevice => {
                let connected = self.state.read().unwrap().device_connected;
                if connected {
                    return Ok(json!({"ok": true, "message": "设备已连接"}));
                }
                let Some(ip) = self.device_ip.clone() else {
                    return Err("启动时未提供 --ip，无法连接设备".into());
                };
                let state = self.state.clone();
                let slot = self.device_slot.clone();
                let work = self.work_tx.clone();
                tokio::spawn(async move {
                    connect_device(ip, slot, state, work).await;
                });
                Ok(json!({"ok": true, "message": "正在重连设备"}))
            }
            ApiOp::RestartBrowser => {
                self.spawn_browser().await?;
                Ok(json!({"ok": true, "message": "浏览器适配器已重启"}))
            }
        }
    }
}

/// 推送 PNG 到设备：Busy 等 300ms 重试一次；ResultUnknown 仅告警不自动重发。
async fn push_image(
    device: &Arc<Device>,
    png: &[u8],
    format: OutputFormat,
) -> zectrix_sdk::Result<()> {
    match device.show_with_format(png, format.to_sdk()).await {
        Ok(()) => Ok(()),
        Err(zectrix_sdk::Error::Busy) => {
            println!("[bridge] 设备忙碌，{BUSY_RETRY_DELAY:?} 后重试一次");
            tokio::time::sleep(BUSY_RETRY_DELAY).await;
            device.show_with_format(png, format.to_sdk()).await
        }
        Err(zectrix_sdk::Error::ResultUnknown) => {
            println!("[bridge] 警告：无法确认设备显示结果（ResultUnknown），按约定不自动重发");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------- 主流程

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("[bridge] 发生错误：{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args = parse_args()?;

    // 设置加载与 CLI 覆盖。
    let Persisted {
        mut settings,
        last_cast,
    } = state::load_persisted(&args.settings_path);
    if let Some(format) = args.format {
        settings.format = format;
    }
    if let Some(delay) = args.delay_ms {
        settings.delay_ms = delay;
    }
    if let Some(bp) = args.black_point {
        settings.black_point = bp;
    }
    println!(
        "[bridge] weread-zectrix-bridge v2 启动：设备 {}，格式 {}，翻页等待 {}ms，黑点 {}，浏览器 {}",
        args.ip.as_deref().unwrap_or("（未配置，暂不连接）"),
        settings.format.as_str(),
        settings.delay_ms,
        settings.black_point,
        args.browser_mode,
    );

    let state = Arc::new(RwLock::new(App::new(
        settings,
        last_cast,
        Some(args.settings_path.clone()),
    )));
    let device_slot: DeviceSlot = Arc::new(std::sync::Mutex::new(None));
    let (work_tx, work_rx) = mpsc::channel::<Work>(256);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // 管理页 HTTP 服务（仅 127.0.0.1）。
    let token = http::generate_token();
    {
        let state = state.clone();
        let work_tx = work_tx.clone();
        let shutdown_signal = shutdown_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = http::serve(args.http_port, &token, state, work_tx, INDEX_HTML).await {
                eprintln!("[bridge] 管理服务异常：{e}");
                let _ = shutdown_signal.send(true);
            }
        });
    }

    // 设备连接（可选，与管理服务独立）。
    if let Some(ip) = args.ip.clone() {
        let state = state.clone();
        let slot = device_slot.clone();
        let work = work_tx.clone();
        tokio::spawn(async move {
            connect_device(ip, slot, state, work).await;
        });
    }

    // worker：浏览器适配器 + 统一命令队列的唯一所有者。
    let worker = Worker {
        state: state.clone(),
        device_slot: device_slot.clone(),
        device_ip: args.ip.clone(),
        config: AdapterConfig {
            node_bin: args.node_bin.clone(),
            controller_js: args.controller_js.clone(),
            browser_mode: args.browser_mode.clone(),
            font_size: args.font_size,
        },
        work_tx: work_tx.clone(),
        shutdown_rx: shutdown_rx.clone(),
        browser: None,
        generation: 0,
        auto_cast_attempted: false,
    };
    let worker_task = tokio::spawn(worker.run(work_rx));

    // Ctrl+C / 管理服务失败 → 优雅退出。
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    let mut shutdown_watch = shutdown_rx.clone();
    tokio::select! {
        _ = &mut ctrl_c => println!("[bridge] 收到 Ctrl+C"),
        _ = shutdown_watch.changed() => {
            if !*shutdown_watch.borrow() {
                println!("[bridge] 管理服务已停止");
            }
        }
    }
    println!("[bridge] 正在退出…");
    let _ = shutdown_tx.send(true);
    let _ = worker_task.await;
    // 最终释放：设备连接显式关闭。
    let device = device_slot.lock().unwrap().take();
    if let Some(device) = device {
        let _ = device.close().await;
        println!("[bridge] 已关闭设备连接，再见。");
    }
    Ok(())
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::validate_reader_url;

    #[test]
    fn accepts_valid_reader_urls() {
        assert_eq!(
            validate_reader_url("https://weread.qq.com/web/reader/abc123"),
            Some("https://weread.qq.com/web/reader/abc123".into())
        );
        assert!(validate_reader_url("https://weread.qq.com/web/reader/abc123/def456").is_some());
        assert!(validate_reader_url("https://weread.qq.com/web/reader/abc?k=1").is_some());
    }

    #[test]
    fn rejects_invalid_urls() {
        assert!(validate_reader_url("").is_none());
        assert!(validate_reader_url("http://weread.qq.com/web/reader/abc").is_none());
        assert!(validate_reader_url("https://evil.com/web/reader/abc").is_none());
        assert!(validate_reader_url("https://weread.qq.com.evil.com/web/reader/a").is_none());
        assert!(validate_reader_url("https://weread.qq.com/web/shelf").is_none());
        assert!(validate_reader_url("https://weread.qq.com/web/reader/").is_none());
        assert!(validate_reader_url("https://weread.qq.com/web/reader/违法id").is_none());
        assert!(validate_reader_url("https://weread.qq.com/web/reader/abc#frag").is_some());
    }
}
