# 微信读书 × 极趣墨水屏

![微信读书在极趣墨水屏上的实机阅读效果](docs/images/zectrix-reading.jpg)

在 Mac 上运行微信读书网页版，将阅读画面投送到局域网中的 ZECTRIX Note 4（400×300），用设备上、下键翻页。

这是正文阅读桥接工具，使用浏览器扫码登录，不需要微信读书 API Key。仅在 macOS 上验证；不是手机 App，也不是官方客户端。

## 已有功能

- Playwright 浏览器持久登录，重启尝试打开上次记录的阅读页。
- 设备下键下一页、上键上一页、确认键刷新；关闭浏览器或 Ctrl+C 退出。
- 浅灰正文对比度增强，默认四级灰阶 gray4（2bpp），也支持 mono / gray16。
- 浏览器 516×360 排版，截取 400×300，当前正文左右约 12px 留白；简介页单独适配。
- 源码自带 MIT SDK，安装、启动和源码打包脚本。

## 快速开始

需要 Node.js ≥18、npm、Rust stable、CMake 和 Xcode Command Line Tools；电脑和极趣设备在同一局域网。

```bash
bash scripts/setup.sh
cp .env.example .env
# 编辑 .env，将 ZECTRIX_IP 改为设备实际 IP
bash scripts/start.sh
```

首次在弹出的浏览器中扫码并打开书籍。需要选书或登录时，在同一个浏览器按 ⌘L，输入 `https://weread.qq.com`。关闭旧书标签页，新书打开后按设备确认键刷新。

完整步骤见 [使用教程](TUTORIAL.md)，发布到 GitHub 见 [发布指南](docs/PUBLISHING.md)。

## 当前限制

- 电脑端阅读导航被净化 CSS 隐藏，暂无专用“返回书架”按钮，使用地址栏回首页。
- 自动换书和云端进度冲突处理尚未实现；优先选择第一个阅读标签页。
- 微信读书负责账号进度同步，本服务不承诺跨端进度实时一致。
- 启动脚本默认关闭预取，避免浏览器提前翻页；直接运行 Rust 二进制默认仍开启预取。
- macOS 开机自启尚未默认安装，按教程可设置登录后启动。
- 网页结构变化可能影响净化、裁切和按键。书籍特殊排版可能需要进一步适配；字号档位覆盖并不可靠，应在微信读书中设置。
- 当前源码包不是免安装的 `.app`，首次安装需要网络下载依赖和 Chromium。

## 目录

```text
src/                  Rust 设备连接与按键桥接
browser-controller/   浏览器控制、截图、灰度增强
vendor/               当前使用的设备 SDK 与原始许可
scripts/              安装、启动、打包
TUTORIAL.md            使用教程
 docs/PUBLISHING.md    GitHub 发布教程
```

## 隐私与许可

登录 Cookie 保存在本机 `browser-controller/.user-data/`，阅读截图和上次书籍地址也会留在本机。这些数据已在忽略和打包规则中排除，请勿上传。日志可能包含书籍地址。

MIT，第三方声明见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。文档结构参考 [hadronliu/weread-ink](https://github.com/hadronliu/weread-ink)，两者用途不同，本项目未复制其业务代码。
