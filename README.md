# 微信读书 × 极趣墨水屏

![微信读书在极趣墨水屏上的实机阅读效果](docs/images/zectrix-reading.jpg)

在 Mac 上运行微信读书网页版，将阅读画面投送到局域网中的 ZECTRIX Note 4（400×300），用设备上、下键翻页；电脑端正常浏览选书，通过本地管理页投送到设备。

这是正文阅读桥接工具，使用浏览器扫码登录，不需要微信读书 API Key。仅在 macOS 上验证；不是手机 App，也不是官方客户端。

## 已有功能

- **浏览页与设备渲染页分离**：电脑用约 1100×800 的浏览页正常登录、选书、翻目录；设备画面由专用渲染页（516×360）生成，互不干扰。
- **本地管理页**（`http://127.0.0.1:8765`）：状态总览、候选书籍一键投送、400×300 预览、远程翻页/暂停/恢复、设置即时生效；仅绑定本机回环地址。
- 显式投送会话：换书生成新会话，旧会话迟到帧不会覆盖新书；投送失败自动恢复旧书绑定。
- 设备下键下一页、上键上一页、确认键刷新；预取默认关闭，不提前推进云端进度。
- 浅灰正文对比度增强，默认四级灰阶 gray4（2bpp），也支持 mono / gray16。
- 浏览器模式 auto / chrome / chromium：系统装有 Google Chrome 时无需下载 Chromium。
- 设置（格式/黑点/翻页等待）与最后投送书籍持久化，重启自动恢复同书；设备断开可在管理页手动重连。
- 源码自带 MIT SDK，安装、启动和源码打包脚本。

## 快速开始

需要 Node.js ≥18、npm、Rust stable、CMake 和 Xcode Command Line Tools；电脑和极趣设备在同一局域网。

```bash
bash scripts/setup.sh
cp .env.example .env
# 编辑 .env，将 ZECTRIX_IP 改为设备实际 IP（也可省略，之后在管理页重连）
bash scripts/start.sh
```

启动后打开终端里打印的管理页地址（默认 `http://127.0.0.1:8765`）：

1. 点「打开书架 / 登录」，在弹出的浏览页中微信扫码；
2. 打开一本书的正文，候选列表自动出现；
3. 点「投送此书」，首帧确认后设备开始显示，之后用设备上下键阅读。

完整步骤见 [使用教程](TUTORIAL.md)，发布到 GitHub 见 [发布指南](docs/PUBLISHING.md)，架构设计见 [docs/DESIGN-HUB.md](docs/DESIGN-HUB.md)。

## 当前限制

- 微信读书负责账号进度同步，本服务不承诺跨端进度实时一致；两端同时阅读可能互相覆盖位置。
- 投送只保证“同一本书”，不保证与电脑端段落一致（云端同步由微信读书管理）。
- macOS 开机自启尚未默认安装，按教程可设置登录后启动。
- 网页结构变化可能影响净化、裁切和按键。书籍特殊排版可能需要进一步适配；字号应在微信读书中设置。
- 当前源码包不是免安装的 `.app`，首次安装需要网络下载依赖（有系统 Chrome 时无需下载 Chromium）。

## 目录

```text
src/                  Rust 会话管理器：命令队列、HTTP 管理服务、设备桥接
browser-controller/   Node 浏览器适配器：浏览页/渲染页、截图、灰度增强
web/                  管理页静态资源（由 Rust 进程内置提供）
vendor/               当前使用的设备 SDK 与原始许可
scripts/              安装、启动、打包
TUTORIAL.md            使用教程
docs/                  设计文档与发布指南
```

## 隐私与许可

登录 Cookie 保存在本机 `browser-controller/.user-data/`，阅读截图、设置与上次书籍地址也会留在本机。这些数据已在忽略和打包规则中排除，请勿上传。日志可能包含书籍地址。

MIT，第三方声明见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。文档结构参考 [hadronliu/weread-ink](https://github.com/hadronliu/weread-ink)，两者用途不同，本项目未复制其业务代码。
