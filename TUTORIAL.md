# 从零开始：微信读书投屏到极趣

## 1. 准备

准备 Mac、ZECTRIX Note 4、微信账号，并让设备和电脑处于同一局域网。在极趣中开启网络同步与待机 Wi-Fi，记录设备 IP；不要同时运行另一个极趣推屏服务。

打开终端，安装命令行工具（已安装可跳过）：

```bash
xcode-select --install
```

安装 Node.js LTS、Rust stable 与 CMake：分别参阅 https://nodejs.org/ 、https://rustup.rs/ 、https://cmake.org/download/ 。安装后重新打开终端，检查：

```bash
node --version
npm --version
cargo --version
cmake --version
```

## 2. 获取与安装

在 GitHub 页面选择 Code → Download ZIP 并解压，或使用 git clone。终端切换到含 `Cargo.toml` 的目录。目录可含空格，路径请加引号。

```bash
cd "/你的路径/weread-zectrix-bridge"
bash scripts/setup.sh
```

脚本使用 `npm ci` 安装锁定的 Node 依赖、下载 Chromium，并编译 release 程序。SDK 已放入 vendor，不需另外下载。首次编译可能较慢。CMake 兼容参数已包含在脚本中。

## 3. 配置与启动

```bash
cp .env.example .env
open -e .env
```

将 `ZECTRIX_IP` 改为设备实际地址。`192.168.1.100` 只是示例。保存后运行：

```bash
bash scripts/start.sh
```

`.env` 由 Bash 读取，只使用自己的可信配置。可选项：

| 配置 | 默认 | 含义 |
| --- | --- | --- |
| ZECTRIX_IP | 必填 | 设备 IPv4 地址 |
| ZECTRIX_FORMAT | mono | mono、gray4、gray16 |
| WEREAD_INK_BLACK_POINT | 200 | 灰度黑点，0 关闭，范围 0 ≤ 值 < 250 |
| ZECTRIX_PREFETCH | 0 | 1 开启下一页预取，可能使云端记录领先设备一页 |

## 4. 登录、选书与阅读

浏览器首次启动后，在地址栏按 ⌘L 输入 `https://weread.qq.com`，在首页使用微信扫码登录，再打开自己的书架和书籍。登录已有效时可直接选书。二维码只在本机使用，不要上传 Cookie。

服务可能自动打开上次记录的书，所以浏览器后退键不可用；阅读导航被隐藏时，仍可用地址栏回首页。若新书新开了标签页，请关闭旧书的阅读标签，只保留当前一本。

日志出现“初始画面已推送到设备”表示已完成首次发送。设备下键下一页，上键上一页，确认键刷新当前页面。切换书籍后按确认键；若仍显示旧缓存，退出并重新启动服务。当前书籍地址文件只在启动就绪时保存，不保证每次换书后自动更新。

手机端阅读后的最新进度由微信读书网页自身同步。先在网页确认进度，再刷新设备；本服务没有独立的云端进度接口。需要时在网页刷新并选择继续阅读。不要把关闭预取理解为跨端进度一致性保证。

## 5. 停止与调整

在服务终端按 Ctrl+C 或关闭服务浏览器窗口，等待退出并释放设备连接。再次启动运行 `bash scripts/start.sh`。不要同时启动多个副本。

```bash
# 页面更新慢时，增加翻页后等待时间
bash scripts/start.sh --delay-ms 400
```

正文发灰：保持黑点 200；插图太黑：尝试 160，或 0 禁用增强，再重启。

本机 `browser-controller/.last-capture-raw.png` 是原始截图，`.last-capture.png` 是处理后截图，用于排查设备前后的问题，包含书籍内容，不要提交到仓库。

## 6. 可选：登录 Mac 后自动启动

有头浏览器需要图形登录会话，因此应使用用户 LaunchAgent，而非系统启动守护进程。请先完成一次手动启动和登录，再停止手动服务。

创建 `~/Library/LaunchAgents/com.local.weread-zectrix.plist`，将下面的 `/ABSOLUTE/PROJECT` 全部改为项目绝对路径。路径中的 `&` 需写成 `&amp;`。把 PATH 改为终端 `command -v node` 所在目录加上系统路径（Apple Silicon Homebrew 常为 `/opt/homebrew/bin`）。

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.local.weread-zectrix</string>
  <key>ProgramArguments</key><array>
    <string>/bin/bash</string><string>/ABSOLUTE/PROJECT/scripts/start.sh</string>
  </array>
  <key>WorkingDirectory</key><string>/ABSOLUTE/PROJECT</string>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>/ABSOLUTE/PROJECT/service.log</string>
  <key>StandardErrorPath</key><string>/ABSOLUTE/PROJECT/service-error.log</string>
</dict></plist>
```

```bash
plutil -lint ~/Library/LaunchAgents/com.local.weread-zectrix.plist
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.local.weread-zectrix.plist
# 停止并卸载；保留文件会在以后登录时再加载，永久关闭需移除该 plist
launchctl bootout "gui/$(id -u)" ~/Library/LaunchAgents/com.local.weread-zectrix.plist
```

未启用 KeepAlive，避免用户关窗后反复弹出。若项目放在 Documents 等受保护目录，系统可能要求允许文件访问。移动项目或 Node 路径后要更新配置。该配置为手动模板，尚未在本机安装或验证自动登录启动。

## 7. 常见问题

| 现象 | 处理 |
| --- | --- |
| 找不到 node / cargo / cmake | 安装依赖并重新打开终端，检查 PATH |
| Chromium 不存在 | 在 browser-controller 内运行 `npx playwright install chromium` |
| 无法连接极趣 | 核实 IP、Wi-Fi、网络同步和同一设备是否被其他服务占用 |
| 登录等待超时 | 首次需 15 分钟内进入阅读页，超时重新启动 |
| 找不到书架或返回键 | ⌘L 打开 https://weread.qq.com |
| 换书后仍显示旧书 | 关闭旧阅读标签，按确认键；仍异常则重启 |
| 页面两侧裁切 | 特殊页面结构可能不匹配当前 12px 适配，请提供脱敏截图反馈 |
| 翻页白屏 / 内容旧 | 将 delay-ms 增至 400，网页确认加载后刷新 |
| Busy | 服务会等待后重试一次 |
| ResultUnknown | 查看实机状态，服务不会自动重复发送 |

当前无断网自动重连保证，设备断开后可停止并重新启动。程序不升级固件、不扫描其他 IP。
