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

脚本使用 `npm ci` 安装锁定的 Node 依赖并编译 release 程序。浏览器选择：默认 auto——Mac 已装 Google Chrome 时直接使用，无需下载 Chromium；未装时下载 Playwright Chromium。需要强制 Chromium 时用 `WEREAD_BROWSER=chromium bash scripts/setup.sh`。SDK 已放入 vendor，不需另外下载。首次编译可能较慢。CMake 兼容参数已包含在脚本中。

## 3. 配置与启动

```bash
cp .env.example .env
open -e .env
```

将 `ZECTRIX_IP` 改为设备实际地址（`192.168.1.100` 只是示例；省略亦可启动，之后在管理页重连）。保存后运行：

```bash
bash scripts/start.sh
```

终端会打印管理页地址（默认 `http://127.0.0.1:8765`），在浏览器打开它。`.env` 由 Bash 读取，只使用自己的可信配置。可选项：

| 配置 | 默认 | 含义 |
| --- | --- | --- |
| ZECTRIX_IP | 可选 | 设备 IPv4 地址；省略则服务启动但不连设备 |
| ZECTRIX_FORMAT | gray4 | mono、gray4、gray16 |
| WEREAD_BROWSER | auto | auto、chrome、chromium |
| WEREAD_HTTP_PORT | 8765 | 管理页端口（仅 127.0.0.1） |
| WEREAD_INK_BLACK_POINT | 200 | 灰度黑点，0 关闭，范围 0 ≤ 值 < 250 |

## 4. 登录、选书与投送（管理页）

1. 管理页顶部可以看到浏览器、设备、登录三个状态灯。点「打开书架 / 登录」，弹出桌面尺寸的**浏览页**。
2. 在浏览页用微信扫码登录，然后像平时一样逛书架、搜索、打开一本书的**正文页**（此时不要管设备）。
3. 回到管理页：候选列表会自动出现这本书，点「**投送此书**」。程序会打开专用渲染页加载该书，首帧经设备确认后，会话进入“阅读中”，预览区显示最新确认画面。
4. 之后用极趣设备**下键下一页、上键上一页、确认键刷新**；管理页的上一页/下一页/刷新按钮与物理按键等效。
5. 换书：在浏览页打开另一本书 → 管理页候选列表点「投送此书」。设备换绑新书，旧书会话自动结束；失败时自动恢复旧书绑定。

预览是“最近一次设备已确认”的画面，不是设备屏幕实时回读。渲染页被误关时，管理页会显示待恢复，点「恢复」即可（自动按最后投送书籍重投）。浏览器整个被关闭时，点「重启浏览器」。

手机端阅读后的最新进度由微信读书云端同步。两端同时阅读可能互相覆盖位置；本服务不承诺跨端进度一致，投送只保证同一本书，不保证与电脑端段落相同。预取默认关闭，不会提前推进云端进度。

## 5. 停止与调整

在服务终端按 Ctrl+C，等待退出并释放设备连接（关闭浏览器窗口不会关闭服务，服务会转为“浏览器未运行”，可在管理页重启）。再次启动运行 `bash scripts/start.sh`。不要同时启动多个副本。

设置区可即时调整输出格式（gray4 / mono / gray16）、黑点（插图太黑尝试 160，正文发灰保持 200，0 禁用增强）与翻页等待（页面更新慢时提高到 400），保存后持久化到 `settings.json`，重启自动恢复；最后投送的书籍也会自动恢复。

本机 `browser-controller/.last-capture.png` 是最近一次发往设备的截图，用于排查设备前后的问题，包含书籍内容，不要提交到仓库。

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
| 系统 Chrome 不存在 | 安装 Google Chrome，或 `WEREAD_BROWSER=chromium` 重跑 setup 下载 Chromium |
| 无法连接极趣 | 核实 IP、Wi-Fi、网络同步和同一设备是否被其他服务占用；管理页点「重连设备」 |
| 登录失效 | 管理页「打开书架 / 登录」重新扫码；登录态由本机会话目录持久保存 |
| 候选列表为空 | 在浏览页确实进入某本书的正文页后点「刷新候选」 |
| 投送后设备无画面 | 查看管理页反馈与终端日志；设备未连接时画面只更新预览 |
| 换书后仍显示旧书 | 投送以“首帧确认”为准；若失败会自动恢复旧书，重试投送即可 |
| 页面两侧裁切 | 特殊页面结构可能不匹配当前 12px 适配，请提供脱敏截图反馈 |
| 翻页白屏 / 内容旧 | 设置区把翻页等待增至 400ms；网页确认加载后刷新 |
| Busy | 服务会等待后重试一次 |
| ResultUnknown | 查看实机状态，服务不会自动重复发送 |

设备断开后可在管理页「重连设备」；程序不升级固件、不扫描其他 IP。
