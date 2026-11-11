//! weread-zectrix-bridge：微信读书网页版 ↔ ZECTRIX Note 4 极趣墨水屏局域网联动阅读器。
//!
//! 架构（Node.js + Rust 协同）：
//! - 模块 A（子进程 `browser-controller/controller.js`，Playwright）：打开有头
//!   Chromium 访问网页版微信读书，注入 400×300 墨水屏净化 CSS，接收 JSON Lines
//!   指令（CAPTURE / NEXT_PAGE / PREV_PAGE / REFRESH）并返回视口 PNG（Base64）。
//! - 模块 B（本进程，zectrix-sdk）：连接极趣设备推送画面；把物理按键映射为
//!   翻页指令：下键→下一页，上键→上一页，确认键→重注入 CSS 并刷新当前页。
//!   设备退出事件 / 断开 / Ctrl+C 均优雅退出并调用 `device.close().await`。
//!
//! 性能：默认启用“预渲染缓存”——设备显示第 N 页时，浏览器预先进到第 N+1 页并
//! 缓存其 PNG，按下下键可立即推送，几乎无等待。可用 --no-prefetch 关闭。

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use zectrix_sdk::{Device, DisconnectReason, Event, ImageFormat, Key, KeyAction, OpenOptions};

/// controller.js 默认随本工程分发，编译期定位，避免依赖运行时工作目录。
const DEFAULT_CONTROLLER_JS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/browser-controller/controller.js"
);
/// 单条控制器指令（翻页+截屏+Base64）的超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 首次运行等待扫码登录并进入正文页的最长时间。
const READY_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// 设备 Busy 时按任务约定的重试间隔。
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(300);
/// 关闭浏览器子进程的宽限时间，超时后强杀。
const CHILD_GRACE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------- 参数

struct Args {
    ip: String,
    format: ImageFormat,
    delay_ms: u64,
    font_size: u32,
    font_level: u32,
    prefetch: bool,
    node_bin: String,
    controller_js: PathBuf,
}

fn usage() -> String {
    format!(
        "用法：weread-zectrix-bridge --ip <设备 IP> [选项]\n\
         \x20 --ip <addr>            极趣设备 IPv4 地址（必填，不扫描其他设备）\n\
         \x20 --format <fmt>         图像格式 mono|gray4|gray16，默认 gray4\n\
         \x20 --delay-ms <n>         翻页后等待重绘的毫秒数，默认 200\n\
         \x20 --font-level <n>       微信读书字号档位（1~6），默认 4；0 为沿用账号设置\n\
         \x20 --font-size <px>       正文强制像素字号（覆盖档位，慎用），默认 0 不覆盖\n\
         \x20 --no-prefetch          关闭下一页预渲染缓存\n\
         \x20 --node-bin <path>      node 可执行文件路径，默认取 PATH 中的 node\n\
         \x20 --controller <path>    controller.js 路径，默认随本工程分发\n\
         示例：CMAKE_POLICY_VERSION_MINIMUM=3.5 cargo run -- --ip 192.168.1.100 --format gray4"
    )
}

fn parse_args() -> Result<Args, String> {
    let mut ip: Option<String> = None;
    let mut format = ImageFormat::Gray4;
    let mut delay_ms: u64 = 200;
    let mut font_size: u32 = 0;
    let mut font_level: u32 = 4;
    let mut prefetch = true;
    let mut node_bin = "node".to_string();
    let mut controller_js = PathBuf::from(DEFAULT_CONTROLLER_JS);

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
            "--ip" => ip = Some(value()?),
            "--format" => {
                format = match value()?.as_str() {
                    "mono" => ImageFormat::Mono,
                    "gray4" => ImageFormat::Gray4,
                    "gray16" => ImageFormat::Gray16,
                    other => {
                        return Err(format!(
                            "不支持的图像格式 {other}（可选 mono|gray4|gray16）"
                        ))
                    }
                };
            }
            "--delay-ms" => {
                delay_ms = value()?
                    .parse()
                    .map_err(|_| format!("--delay-ms 需为非负整数\n{}", usage()))?;
            }
            "--font-size" => {
                font_size = value()?
                    .parse()
                    .map_err(|_| format!("--font-size 需为非负整数\n{}", usage()))?;
            }
            "--font-level" => {
                font_level = value()?
                    .parse()
                    .map_err(|_| format!("--font-level 需为非负整数\n{}", usage()))?;
            }
            "--no-prefetch" => prefetch = false,
            "--node-bin" => node_bin = value()?,
            "--controller" => controller_js = PathBuf::from(value()?),
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("未知参数 {other}\n{}", usage())),
        }
        i += 1;
    }

    Ok(Args {
        ip: ip.ok_or_else(|| format!("必须通过 --ip 指定极趣设备地址\n{}", usage()))?,
        format,
        delay_ms,
        font_size,
        font_level,
        prefetch,
        node_bin,
        controller_js,
    })
}

// ---------------------------------------------------------------- 浏览器控制器

/// 发往 Node 控制器的指令。
#[derive(Clone, Copy, Debug)]
enum Cmd {
    Capture,
    NextPage,
    PrevPage,
    Refresh,
}

impl Cmd {
    fn as_str(self) -> &'static str {
        match self {
            Cmd::Capture => "CAPTURE",
            Cmd::NextPage => "NEXT_PAGE",
            Cmd::PrevPage => "PREV_PAGE",
            Cmd::Refresh => "REFRESH",
        }
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Vec<u8>, String>>>>>;

/// 通过 stdin/stdout JSON Lines 驱动的 Playwright 控制器客户端。
struct Browser {
    child: Child,
    stdin: ChildStdin,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    ready_rx: watch::Receiver<bool>,
    exited_rx: watch::Receiver<bool>,
}

impl Browser {
    fn spawn(args: &Args) -> Result<Browser, String> {
        if !args.controller_js.exists() {
            return Err(format!(
                "找不到控制器脚本：{}",
                args.controller_js.display()
            ));
        }
        let mut child = Command::new(&args.node_bin)
            .arg(&args.controller_js)
            .arg("--delay-ms")
            .arg(args.delay_ms.to_string())
            .arg("--font-size")
            .arg(args.font_size.to_string())
            .arg("--font-level")
            .arg(args.font_level.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Playwright 的提示信息直接透传给终端。
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("启动 Node 控制器失败（{}）：{e}", args.node_bin))?;

        let stdin = child.stdin.take().ok_or("无法获取控制器 stdin")?;
        let stdout = child.stdout.take().ok_or("无法获取控制器 stdout")?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (ready_tx, ready_rx) = watch::channel(false);
        let (exited_tx, exited_rx) = watch::channel(false);

        // 单一读取任务：分发指令响应与事件，检测子进程退出（stdout EOF）。
        tokio::spawn({
            let pending = pending.clone();
            async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            if Browser::handle_event(&v, &ready_tx) {
                                continue;
                            }
                            Browser::deliver_reply(&v, &pending).await;
                        }
                        _ => break,
                    }
                }
                // 子进程退出：唤醒所有等待者并广播退出状态。
                pending.lock().await.clear();
                let _ = exited_tx.send(true);
            }
        });

        Ok(Browser {
            child,
            stdin,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            ready_rx,
            exited_rx,
        })
    }

    /// 处理 Node 侧事件；返回 true 表示该行是事件而非指令响应。
    fn handle_event(v: &Value, ready_tx: &watch::Sender<bool>) -> bool {
        let Some(event) = v.get("event").and_then(Value::as_str) else {
            return false;
        };
        match event {
            "ready" => {
                let url = v.get("url").and_then(Value::as_str).unwrap_or("");
                println!("[bridge] 浏览器控制器就绪：{url}");
                let _ = ready_tx.send(true);
            }
            "log" => {
                if let Some(message) = v.get("message").and_then(Value::as_str) {
                    println!("[browser] {message}");
                }
            }
            "closed" => {
                let reason = v.get("reason").and_then(Value::as_str).unwrap_or("");
                println!("[browser] 控制器已退出（{reason}）");
            }
            other => println!("[browser] 未知事件：{other}"),
        }
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
            v.get("pngB64")
                .and_then(Value::as_str)
                .map(|s| {
                    B64.decode(s)
                        .map_err(|e| format!("PNG Base64 解码失败：{e}"))
                })
                .unwrap_or_else(|| Err("控制器未返回图像数据".into()))
        } else {
            Err(v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("未知错误")
                .to_string())
        };
        let _ = tx.send(reply);
    }

    /// 等待用户完成扫码登录并进入正文页（ready 事件）。
    async fn wait_ready(&mut self) -> Result<(), String> {
        tokio::select! {
            _ = tokio::time::sleep(READY_TIMEOUT) => {
                Err("等待浏览器进入微信读书正文页超时，请重新运行".into())
            }
            _ = self.ready_rx.changed() => {
                if *self.ready_rx.borrow() { Ok(()) } else { Err("ready 状态异常".into()) }
            }
            _ = self.exited_rx.changed() => {
                Err("浏览器控制器进程已退出".into())
            }
            _ = tokio::signal::ctrl_c() => {
                Err("收到 Ctrl+C".into())
            }
        }
    }

    /// 发送指令并等待 PNG 字节。
    async fn request(&mut self, cmd: Cmd) -> Result<Vec<u8>, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        // JSON Lines 协议：必须以换行符结尾，否则 Node 侧 readline 不会触发 line 事件。
        let mut line = json!({ "id": id, "cmd": cmd.as_str() }).to_string();
        line.push('\n');
        let written = async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.flush().await
        }
        .await;
        if let Err(e) = written {
            self.pending.lock().await.remove(&id);
            return Err(format!("向控制器写入失败：{e}"));
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("浏览器控制器已退出".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("控制器响应超时（{}s）", REQUEST_TIMEOUT.as_secs()))
            }
        }
    }

    /// 优雅关闭：关闭 stdin 触发 Node 自行收尾，宽限期后强杀。
    async fn shutdown(mut self) {
        let _ = self.stdin.shutdown().await;
        match tokio::time::timeout(CHILD_GRACE, self.child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
            }
        }
    }
}

// ---------------------------------------------------------------- 设备推送

/// 推送 PNG：Busy 等 300ms 重试一次；ResultUnknown 仅告警不自动重发。
async fn push_image(
    device: &Arc<Device>,
    png: &[u8],
    format: ImageFormat,
) -> zectrix_sdk::Result<()> {
    match device.show_with_format(png, format).await {
        Ok(()) => Ok(()),
        Err(zectrix_sdk::Error::Busy) => {
            println!("[bridge] 设备忙碌，{BUSY_RETRY_DELAY:?} 后重试一次");
            tokio::time::sleep(BUSY_RETRY_DELAY).await;
            device.show_with_format(png, format).await
        }
        Err(zectrix_sdk::Error::ResultUnknown) => {
            println!("[bridge] 警告：无法确认设备显示结果（ResultUnknown），按约定不自动重发");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// ------------------------------------------------------------- 翻页动作状态机

/// 预渲染缓存状态：浏览器是否领先设备一页、领先页 PNG，以及已推送画面历史。
#[derive(Default)]
struct PageCache {
    ahead: bool,
    png: Option<Vec<u8>>,
    /// 最近推送的画面（最多两页）：history.last() = 设备当前页，history[0] = 上一页，
    /// 供上键即时回推（与下键的预取缓存对称）。
    history: Vec<Vec<u8>>,
}

impl PageCache {
    fn invalidate(&mut self) {
        self.ahead = false;
        self.png = None;
    }

    /// 记录一次新页面的成功推送，保留最近两页。
    fn note_pushed(&mut self, png: &[u8]) {
        self.history.push(png.to_vec());
        if self.history.len() > 2 {
            self.history.remove(0);
        }
    }

    /// 同页刷新（确认键重绘）只替换当前页画面，不顶掉“上一页”历史。
    fn replace_last(&mut self, png: &[u8]) {
        if let Some(last) = self.history.last_mut() {
            *last = png.to_vec();
        } else {
            self.note_pushed(png);
        }
    }
}

/// 预取：浏览器前进一页并缓存其 PNG，使设备显示第 N 页时浏览器停在第 N+1 页。
async fn prefetch_next(browser: &mut Browser, cache: &mut PageCache) {
    match browser.request(Cmd::NextPage).await {
        Ok(png) => {
            cache.png = Some(png);
            cache.ahead = true;
        }
        Err(e) => {
            println!("[bridge] 预取下一页失败（{e}），回退为按键后同步翻页");
            cache.invalidate();
        }
    }
}

/// 推送并记录耗时与历史；same_page_refresh 为真时只替换当前页历史（确认键重绘）。
async fn push_page(
    device: &Arc<Device>,
    cache: &mut PageCache,
    png: &[u8],
    format: ImageFormat,
    label: &str,
    started: Instant,
    same_page_refresh: bool,
) -> zectrix_sdk::Result<()> {
    let result = push_image(device, png, format).await;
    println!(
        "[bridge] {label} 推送完成，本轮耗时 {}ms",
        started.elapsed().as_millis()
    );
    if result.is_ok() {
        if same_page_refresh {
            cache.replace_last(png);
        } else {
            cache.note_pushed(png);
        }
    }
    result
}

/// 下键：优先推送预取缓存；随后继续保持“浏览器领先一页”。
async fn action_next(
    browser: &mut Browser,
    device: &Arc<Device>,
    args: &Args,
    cache: &mut PageCache,
) -> zectrix_sdk::Result<()> {
    let started = Instant::now();
    if cache.ahead {
        if let Some(png) = cache.png.take() {
            push_page(
                device,
                cache,
                &png,
                args.format,
                "下一页（预取缓存）",
                started,
                false,
            )
            .await?;
            prefetch_next(browser, cache).await;
            return Ok(());
        }
        cache.invalidate();
    }
    match browser.request(Cmd::NextPage).await {
        Ok(png) => {
            push_page(device, cache, &png, args.format, "下一页", started, false).await?;
            if args.prefetch {
                prefetch_next(browser, cache).await;
            }
            Ok(())
        }
        Err(e) => {
            println!("[bridge] 翻下一页失败：{e}");
            cache.invalidate();
            Ok(())
        }
    }
}

/// 上键：优先用推送历史秒回上一页；否则翻页截屏推送。
async fn action_prev(
    browser: &mut Browser,
    device: &Arc<Device>,
    args: &Args,
    cache: &mut PageCache,
) -> zectrix_sdk::Result<()> {
    let started = Instant::now();
    let was_ahead = cache.ahead;

    // 快路径：历史里存有上一页画面，立即推送；随后校正浏览器状态。
    // 浏览器原本领先一页（停在 N+2，设备显示 N+1）：推送上一页（N）后，
    // 一次 PREV 让浏览器停到 N+1 —— 恰好仍是“领先设备一页”，其返回图像即新预取缓存。
    // 未领先时（浏览器=旧设备页 N+1）：一次 PREV 让浏览器与设备同页（N），丢弃返回图像。
    if cache.history.len() == 2 {
        let prev_png = cache.history[0].clone();
        push_page(
            device,
            cache,
            &prev_png,
            args.format,
            "上一页（历史缓存）",
            started,
            false,
        )
        .await?;
        match browser.request(Cmd::PrevPage).await {
            Ok(png) => {
                if was_ahead && args.prefetch {
                    cache.png = Some(png);
                    cache.ahead = true;
                }
            }
            Err(e) => {
                println!("[bridge] 上一页浏览器状态校正失败：{e}");
                cache.invalidate();
            }
        }
        return Ok(());
    }

    // 慢路径：无历史。浏览器领先一页时需两次 PREV——第一次回到设备当前页（丢弃图像），
    // 第二次才到达真正的上一页。
    if was_ahead {
        if let Err(e) = browser.request(Cmd::PrevPage).await {
            println!("[bridge] 翻上一页失败（回退当前页）：{e}");
            cache.invalidate();
            return Ok(());
        }
    }
    match browser.request(Cmd::PrevPage).await {
        Ok(png) => {
            push_page(device, cache, &png, args.format, "上一页", started, false).await?;
            if args.prefetch {
                prefetch_next(browser, cache).await;
            }
            Ok(())
        }
        Err(e) => {
            println!("[bridge] 翻上一页失败：{e}");
            cache.invalidate();
            Ok(())
        }
    }
}

/// 确认键：重注入净化 CSS 并强制刷新当前页画面。
async fn action_confirm(
    browser: &mut Browser,
    device: &Arc<Device>,
    args: &Args,
    cache: &mut PageCache,
) -> zectrix_sdk::Result<()> {
    let started = Instant::now();
    if cache.ahead {
        // 先退回设备当前页，丢弃该次返回的图像，再重注入 CSS 截屏。
        match browser.request(Cmd::PrevPage).await {
            Ok(_) => {}
            Err(e) => {
                println!("[bridge] 回退当前页失败：{e}");
                cache.invalidate();
            }
        }
    }
    match browser.request(Cmd::Refresh).await {
        Ok(png) => {
            push_page(
                device,
                cache,
                &png,
                args.format,
                "当前页刷新",
                started,
                true,
            )
            .await?;
            if args.prefetch {
                prefetch_next(browser, cache).await;
            }
            Ok(())
        }
        Err(e) => {
            println!("[bridge] 刷新当前页失败：{e}");
            cache.invalidate();
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- 事件合并

/// 防抖合并：推送处理期间积压的连按合并为最后一次方向；遇到断开事件返回 None。
fn merge_pending(rx: &mut mpsc::Receiver<Event>, mut key: Key) -> Option<Key> {
    loop {
        match rx.try_recv() {
            Ok(Event::Key {
                key: k,
                action: KeyAction::Short,
                ..
            }) => key = k,
            Ok(Event::Disconnected { .. }) => return None,
            Ok(_) => {} // 长按与录音事件不参与合并
            Err(_) => return Some(key),
        }
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
    println!(
        "[bridge] weread-zectrix-bridge 启动：设备 {}，格式 {:?}，翻页等待 {}ms，预取 {}",
        args.ip,
        args.format,
        args.delay_ms,
        if args.prefetch { "开" } else { "关" }
    );

    // 1. 启动浏览器控制器并等待进入正文页（首次运行需扫码登录）。
    let mut browser = Browser::spawn(&args)?;
    if let Err(e) = browser.wait_ready().await {
        // 等待登录期间退出（Ctrl+C/超时/控制器退出）：先收子进程再返回。
        browser.shutdown().await;
        return Err(e);
    }

    // 2. 连接极趣设备（单一长连接，客户端名 WeRead Bridge）。
    let device = Arc::new(
        Device::open(OpenOptions {
            target: Some(args.ip.clone()),
            client_name: Some("WeRead Bridge".into()),
        })
        .await
        .map_err(|e| format!("连接设备失败：{e}"))?,
    );
    println!(
        "[bridge] 已连接极趣设备：{}（{}）@ {}",
        device.summary.name, device.summary.id, device.summary.address
    );

    // 3. 初始画面：CAPTURE 当前阅读页并推送；失败时显式释放设备连接再退出。
    let mut cache = PageCache::default();
    let initial = match browser.request(Cmd::Capture).await {
        Ok(png) => png,
        Err(e) => {
            let msg = format!("获取初始画面失败：{e}");
            println!("[bridge] {msg}，退出前释放设备连接…");
            browser.shutdown().await;
            let _ = device.close().await;
            return Err(msg);
        }
    };
    if let Err(e) = push_page(
        &device,
        &mut cache,
        &initial,
        args.format,
        "初始画面",
        Instant::now(),
        false,
    )
    .await
    {
        let msg = format!("推送初始画面失败：{e}");
        println!("[bridge] {msg}，退出前释放设备连接…");
        browser.shutdown().await;
        let _ = device.close().await;
        return Err(msg);
    }
    println!("[bridge] 初始画面已推送到设备");
    if args.prefetch {
        prefetch_next(&mut browser, &mut cache).await;
    }

    // 4. 事件泵：唯一 next_event 消费任务，转发到主循环通道。
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(64);
    let pump = tokio::spawn({
        let device = device.clone();
        async move {
            loop {
                match device.next_event().await {
                    Ok(Some(event)) => {
                        if event_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = event_tx
                            .send(Event::Disconnected {
                                reason: DisconnectReason::Closed,
                            })
                            .await;
                        break;
                    }
                    Err(e) => {
                        eprintln!("[bridge] 读取设备事件失败：{e}");
                        let _ = event_tx
                            .send(Event::Disconnected {
                                reason: DisconnectReason::Failed,
                            })
                            .await;
                        break;
                    }
                }
            }
        }
    });

    println!("[bridge] 就绪。极趣设备按键：下键=下一页，上键=上一页，确认键=重注入 CSS 并刷新；Ctrl+C 退出。");

    // 5. 主循环：设备事件 / Ctrl+C / 控制器退出 任一触发结束。
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    let mut node_exited = browser.exited_rx.clone();
    // 所有 break 路径都会先赋值，此处无需初始值。
    let exit_reason;
    loop {
        let event = tokio::select! {
            event = event_rx.recv() => match event {
                Some(event) => event,
                None => { exit_reason = "事件通道关闭"; break; }
            },
            _ = &mut ctrl_c => { exit_reason = "收到 Ctrl+C"; break; }
            _ = node_exited.changed() => {
                if *node_exited.borrow() {
                    exit_reason = "浏览器控制器已退出";
                    break;
                }
                continue;
            }
        };
        match event {
            Event::Key {
                key,
                action: KeyAction::Short,
                ..
            } => {
                // 防抖：合并推送期间积压的连按，只执行最后一次按键。
                let Some(key) = merge_pending(&mut event_rx, key) else {
                    exit_reason = "设备连接结束（事件合并期间收到断开）";
                    break;
                };
                let result = match key {
                    Key::Down => action_next(&mut browser, &device, &args, &mut cache).await,
                    Key::Up => action_prev(&mut browser, &device, &args, &mut cache).await,
                    Key::Confirm => action_confirm(&mut browser, &device, &args, &mut cache).await,
                };
                if let Err(e) = result {
                    println!("[bridge] 按键处理失败，准备退出：{e}");
                    exit_reason = "设备推送失败";
                    break;
                }
            }
            // 上下长按与录音流程不属于本阅读场景，仅记录。
            Event::Key {
                key,
                action: KeyAction::Long,
                ..
            } => {
                println!("[bridge] 忽略长按事件：{key:?} Long");
            }
            Event::RecordingStarted => println!("[bridge] 设备开始录音（忽略）"),
            Event::RecordingFinished { audio, reason } => {
                let frames = audio.map(|c| c.samples.len()).unwrap_or(0);
                println!("[bridge] 设备录音结束：{reason:?}（{frames} 帧，忽略）");
            }
            Event::Disconnected { reason } => {
                exit_reason = "设备连接结束";
                println!("[bridge] 设备断开：{reason:?}");
                break;
            }
        }
    }

    // 6. 优雅退出：先收子进程，再释放设备连接。
    println!("[bridge] 正在退出（{exit_reason}）…");
    pump.abort();
    browser.shutdown().await;
    device
        .close()
        .await
        .map_err(|e| format!("关闭设备连接失败：{e}"))?;
    println!("[bridge] 已关闭设备连接，再见。");
    Ok(())
}
