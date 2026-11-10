# 上传 GitHub 与发布源码包

当前目录是独立、已去除运行数据的源码副本，不影响原服务。包含本地 SDK 和它的原始许可证；依赖安装后编译运行，不是独立安装应用。

## 创建仓库

在 GitHub 创建空仓库，例如 `weread-zectrix-bridge`，不要自动添加 README。进入本目录：

```bash
git init -b main
git add .
git status --short
```

检查暂存文件不包含 `.env`、`.user-data`、截图、阅读地址和日志。不要使用 `git add -f` 强行加入它们。

```bash
git diff --cached --stat
git commit -m "Initial release: WeRead to ZECTRIX bridge"
git remote add origin https://github.com/YOUR_USERNAME/weread-zectrix-bridge.git
git push -u origin main
```

将 YOUR_USERNAME 替换为你的账号，使用 GitHub 支持的登录方式授权。以上命令需要自行执行；本次整理没有创建远程仓库或推送。

## 制作 Release

```bash
bash scripts/package.sh
```

输出 `dist/weread-zectrix-bridge-source.zip`，排除本机会话和构建依赖。随后在 GitHub Releases → Draft a new release，创建 `v0.1.0` 标签，上传 ZIP。GitHub 自动生成的 Source code 也可以使用。

建议说明：macOS 已验证；需要 Node.js、Rust、CMake 与下载 Chromium；400×300 Note 4；首次扫码登录；当前换书需关闭旧标签并刷新；自动进度冲突处理未实现。

每次发布前执行安装、语法检查和编译检查；在实机检查普通正文、简介、首末行与连续翻页。CI 检查不代表已验证硬件。
