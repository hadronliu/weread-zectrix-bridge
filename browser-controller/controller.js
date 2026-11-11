#!/usr/bin/env node
'use strict';

/**
 * 微信读书浏览器适配器 v2（协议 v2）。
 *
 * 由 Rust 会话管理器（weread-zectrix-bridge）作为子进程启动，通过 stdin/stdout
 * 的 JSON Lines 协议通信。与 v1 的核心差异：
 *
 * - 浏览页与渲染页分离：浏览页（约 1100×800）供用户正常登录、选书、浏览，
 *   永不注入净化 CSS；渲染页（516×360、DPR=1）专供设备截图，只有它接受
 *   翻页/截屏指令——不再“扫描第一个阅读标签”猜测目标。
 * - 启动即上报 controller_ready（无需登录/选书）；登录状态与候选阅读标签
 *   变化通过事件主动上报。
 * - 响应为分类型结构（frame/status/ack），错误含 code/message/recoverable。
 * - 浏览器模式 auto/chrome/chromium：auto 优先系统 Chrome，避免下载 Chromium。
 *
 * 请求（Rust → Node）：{"id":1,"cmd":"LOAD_BOOK","payload":{"url":"..."}}
 * 响应（Node → Rust）：{"id":1,"ok":true,"result":{"kind":"frame","pngB64":"..."}}
 *                      {"id":1,"ok":false,"error":{"code":"no_renderer","message":"…","recoverable":true}}
 * 事件（Node → Rust）：controller_ready / auth_changed / candidates_changed /
 *                      renderer_closed / log / closed
 *
 * 指令集：OPEN_LIBRARY、LIST_CANDIDATES、LOAD_BOOK、CAPTURE、NEXT_PAGE、
 *         PREV_PAGE、REFRESH、CLOSE_RENDERER。
 */

const fs = require('fs');
const path = require('path');
const readline = require('readline');
const { chromium } = require('playwright');
const { enhanceInk } = require('./ink');

// ---------------------------------------------------------------- 命令行参数

function argValue(name, fallback) {
  const i = process.argv.indexOf(name);
  return i !== -1 && process.argv[i + 1] !== undefined ? process.argv[i + 1] : fallback;
}

/** 浏览器模式：auto（系统 Chrome 优先）/ chrome / chromium。 */
const BROWSER_MODE = ['auto', 'chrome', 'chromium'].includes(argValue('--browser', 'auto'))
  ? argValue('--browser', 'auto')
  : 'auto';
/** 翻页后等待重绘的默认毫秒数；单条指令可用 payload.delayMs 覆盖。 */
const DELAY_MS = Math.max(0, parseInt(argValue('--delay-ms', '200'), 10) || 200);
const START_URL = argValue('--url', 'https://weread.qq.com');
/** 服务专属持久化目录：保存登录 Cookie，不读日常 Chrome 用户目录。 */
const PROFILE_DIR = argValue('--user-data-dir', path.join(__dirname, '.user-data'));
/** 灰度拉伸黑点（0~249）；0 表示保留原始截图。单条指令可覆盖。 */
const BLACK_POINT = (() => {
  const n = Number(argValue('--black-point', process.env.WEREAD_INK_BLACK_POINT ?? '200'));
  if (!Number.isFinite(n) || n < 0 || n >= 250) {
    throw new Error('--black-point must be in [0, 250)');
  }
  return Math.floor(n);
})();
/** 正文强制字号（px）；0 表示不覆盖（推荐在微信读书设置里调字号）。 */
const READER_FONT_SIZE = Math.max(0, parseInt(argValue('--font-size', '0'), 10) || 0);
/** 诊断输出开关：WEREAD_DIAG=1 时刷新指令附带排版诊断日志。 */
const DIAG = process.env.WEREAD_DIAG === '1';

/**
 * 渲染页排版：微信读书按 516×360 分页后在中间裁出 400×300 设备画面，
 * 保留约 12px 侧边距；浏览页用桌面尺寸正常浏览。
 */
const BROWSE_VIEWPORT = { width: 1100, height: 800 };
const RENDERER_VIEWPORT = { width: 516, height: 360 };
const CAPTURE_CLIP = { x: 58, y: 30, width: 400, height: 300 };

// ---------------------------------------------------------------- 输出工具

function sendEvent(name, fields = {}) {
  process.stdout.write(JSON.stringify({ event: name, ...fields }) + '\n');
}

function sendLog(message) {
  sendEvent('log', { message });
}

function sendReply(id, ok, extra = {}) {
  process.stdout.write(JSON.stringify({ id, ok, ...extra }) + '\n');
}

function sendOk(id, result) {
  sendReply(id, true, { result });
}

function sendErr(id, code, message, recoverable = true) {
  sendReply(id, false, { error: { code, message, recoverable } });
}

/** 已进入主动关闭流程：后续异常不再按“启动失败”上报。 */
let shuttingDown = false;

// ---------------------------------------------------------------- 排版净化 CSS

function buildCleanupCss(fontSizePx) {
  const fontRule =
    fontSizePx > 0
      ? `
/* 6) 墨水屏字号：加大笔画厚度，细字在黑白量化下不再发虚 */
.readerChapterContent, .readerChapterContent span,
.readerTextStyle, .readerTextStyle span,
.readerChapterContent .readerRestore, .readerChapterContent .readerRestore * {
  font-size: ${fontSizePx}px !important;
}
`
      : '';
  return `
/* 1) 隐藏非正文界面元素 */
.readerTopBar, .readerControls, .readerControls_item,
.readerCatalog, .readerCatalog_panel,
.readerSettingsPanel, .readerFontPanel, .readerControls_fontSize,
.readerNotePanel, .readerNoteList, .readerReviewPanel, .readerBookmarkPanel,
.readerFooter, .readerFooterPanel, .readerChapterCard,
.readerCopyright, .readerBarcode, .readerTranslate,
.reading_background_jukebox, .jukebox, .readerSearchPanel,
[class*="readerControls"], [class*="readerTopBar"],
.renderTarget_pager { display: none !important; }

/* 2) 阅读容器铺满视口。注意：不要对新版阅读器的 wr_* 容器强制
      height:100% —— 会破坏其内部高度计算导致渲染容器塌陷（白屏）。 */
html, body { margin: 0 !important; padding: 0 !important; width: 100% !important; height: 100% !important; }
.readerContent, .readerContent .app_content,
.readerChapterContent, .readerRestore, .readerRestore_1,
.readerContainer, .readerChapterContent .readerRestore {
  width: 100% !important;
  min-width: 100% !important;
  max-width: 100% !important;
  height: 100% !important;
  max-height: 100% !important;
  margin: 0 !important;
  padding: 0 !important;
  top: 0 !important;
  left: 0 !important;
  right: 0 !important;
  bottom: 0 !important;
}

/* 封面/扉页与 canvas 阅读器的左右留白对齐到裁切安全区。 */
.horizontalReaderCoverPage_content,
.horizontalReaderCoverPage_content_right,
.horizontalReaderCoverPage_content_additional,
.reader_flyleaf_container .wr_flyleaf_page {
  padding-left: 70px !important;
  padding-right: 70px !important;
  box-sizing: border-box !important;
  max-width: 100% !important;
}
.horizontalReaderCoverPage_content_bookInfo_intro {
  overflow-wrap: anywhere !important;
}

/* 3) 墨水屏对比度：纯白背景 + 纯黑文字（微信读书会把正文调成浅灰）。 */
html, body,
.readerContent, .app_content, .readerChapterContent,
.readerRestore, .readerRestore_1, .readerContainer {
  background: #ffffff !important;
  color: #000000 !important;
}
body *:not(svg):not(path):not(img):not(video) {
  color: #000000 !important;
  -webkit-text-fill-color: #000000 !important;
  filter: none !important;
  opacity: 1 !important;
}

/* 4) 去掉滚动条与页面溢出 */
html, body { overflow: hidden !important; }
::-webkit-scrollbar { display: none !important; }

/* 5) 墨水屏清晰度：关闭字体抗锯齿与文字阴影。 */
*, *::before, *::after {
  -webkit-font-smoothing: none !important;
  font-smooth: never !important;
  text-shadow: none !important;
}
${fontRule}`;
}

const CLEANUP_CSS = buildCleanupCss(READER_FONT_SIZE);
const CSS_STYLE_ID = 'weread-zectrix-clean-css';

/** 注入净化 CSS（仅渲染页调用）。幂等；force=true 强制重写。 */
async function injectCss(page, force = false) {
  await page.evaluate(
    ([css, styleId, forced]) => {
      let el = document.getElementById(styleId);
      if (el && !forced) return;
      if (el) el.remove();
      el = document.createElement('style');
      el.id = styleId;
      el.textContent = css;
      document.documentElement.appendChild(el);
    },
    [CLEANUP_CSS, CSS_STYLE_ID, force]
  );
}

/** 等待两帧 rAF（后台标签 rAF 会被暂停，用 500ms 定时器兜底）。 */
async function settlePaint(page) {
  await page.evaluate(
    () =>
      new Promise((resolve) => {
        let done = false;
        const finish = () => {
          if (!done) {
            done = true;
            resolve();
          }
        };
        const timer = setTimeout(finish, 500);
        requestAnimationFrame(() =>
          requestAnimationFrame(() => {
            clearTimeout(timer);
            finish();
          })
        );
      })
  );
}

/** 阅读页 URL 判定：严格解析，不使用字符串 includes。 */
function parseReaderUrl(raw) {
  if (typeof raw !== 'string' || raw.length > 2048) return null;
  let u;
  try {
    u = new URL(raw);
  } catch {
    return null;
  }
  if (u.protocol !== 'https:' || u.hostname !== 'weread.qq.com') return null;
  // 路径形如 /web/reader/<bookId>[/...]，bookId 必须非空。
  const parts = u.pathname.split('/').filter(Boolean);
  if (parts.length < 2 || parts[0] !== 'web' || parts[1] !== 'reader') return null;
  const bookId = parts[2];
  if (!bookId || /[?#]/.test(bookId)) return null;
  return { url: u.origin + u.pathname + u.search, bookId };
}

/**
 * 等待阅读内容真实渲染：渲染容器高度非零且含可见文本。
 * 重渲染期间容器会暂时塌陷为 0，此时截图会得到白屏。
 */
async function waitForContent(page, timeoutMs = 10000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const ok = await page
      .evaluate(() => {
        const el =
          document.querySelector('.renderTargetContainer') ||
          document.querySelector('.readerChapterContent');
        if (!el) return false;
        const r = el.getBoundingClientRect();
        return r.height > 50 && (el.textContent || '').trim().length > 0;
      })
      .catch(() => false);
    if (ok) return true;
    await new Promise((r) => setTimeout(r, 300));
  }
  return false;
}

/** 最近一次成功截屏存档，供人工比对浏览器端原始画面。 */
const LAST_CAPTURE_FILE = path.join(__dirname, '.last-capture.png');
function archiveCapture(buf) {
  try {
    fs.writeFileSync(LAST_CAPTURE_FILE, buf);
  } catch {
    /* 存档失败不影响主流程 */
  }
}

// ---------------------------------------------------------------- 主流程

async function launchContext() {
  const common = {
    headless: false,
    // context 级默认视口（浏览页尺寸）与 DPR=1；渲染页用 setViewportSize
    // 独立覆盖为 516×360——Playwright 不允许 viewport:null 时携带 deviceScaleFactor。
    viewport: { width: BROWSE_VIEWPORT.width, height: BROWSE_VIEWPORT.height },
    deviceScaleFactor: 1,
    args: [
      '--window-size=1180,900',
      '--disable-blink-features=AutomationControlled',
    ],
  };
  // executablePath() 是同步 API：浏览器未下载时抛错。
  let chromiumInstalled = true;
  try {
    chromium.executablePath();
  } catch {
    chromiumInstalled = false;
  }

  if (BROWSER_MODE === 'chromium') {
    if (!chromiumInstalled) {
      throw new Error(
        'Playwright Chromium 未安装：请在 browser-controller 下运行 npx playwright install chromium，或使用 --browser auto/chrome'
      );
    }
    return chromium.launchPersistentContext(PROFILE_DIR, common);
  }
  if (BROWSER_MODE === 'chrome') {
    return chromium.launchPersistentContext(PROFILE_DIR, { ...common, channel: 'chrome' });
  }
  // auto：优先系统 Chrome（无需下载 Chromium），缺失时回退。
  try {
    const ctx = await chromium.launchPersistentContext(PROFILE_DIR, {
      ...common,
      channel: 'chrome',
    });
    sendLog('使用系统 Chrome（专属会话目录，不影响日常浏览器数据）');
    return ctx;
  } catch (e) {
    if (!chromiumInstalled) {
      throw new Error(
        '系统 Chrome 不可用且 Playwright Chromium 未安装：请安装 Google Chrome，或在 browser-controller 下运行 npx playwright install chromium。原始错误：' +
          (e && e.message ? e.message : e)
      );
    }
    sendLog('系统 Chrome 不可用，回退 Playwright Chromium：' + (e && e.message ? e.message : e));
    return chromium.launchPersistentContext(PROFILE_DIR, common);
  }
}

async function main() {
  fs.mkdirSync(PROFILE_DIR, { recursive: true });

  const context = await launchContext();

  let exiting = false;
  let pollTimer = null;
  async function shutdown(reason) {
    if (exiting) return;
    exiting = true;
    shuttingDown = true;
    if (pollTimer) clearInterval(pollTimer);
    sendEvent('closed', reason ? { reason } : {});
    try {
      await context.close();
    } catch {
      /* 忽略关闭时的异常 */
    }
    process.exit(0);
  }
  process.on('SIGTERM', () => shutdown('SIGTERM'));
  process.on('SIGINT', () => shutdown('SIGINT'));
  context.on('close', () => shutdown('browser-window-closed'));

  // ---------------- 页面管理：浏览页 + 专用渲染页 ----------------

  /** 浏览页：正常桌面视口，保留站点导航，永不注入净化 CSS。 */
  let browsePage = context.pages().find((p) => !p.isClosed()) || (await context.newPage());
  await browsePage.setViewportSize(BROWSE_VIEWPORT).catch(() => {});
  if (!/weread\.qq\.com/.test(browsePage.url())) {
    sendLog(`浏览页正在打开 ${START_URL} …`);
    await browsePage
      .goto(START_URL, { waitUntil: 'domcontentloaded', timeout: 60_000 })
      .catch((e) => sendLog(`打开书架失败：${e && e.message ? e.message : e}`));
  }

  /** 渲染页：专用设备渲染目标；只有它接受翻页/截屏指令。 */
  let rendererPage = null;

  function ensureRenderer() {
    return rendererPage && !rendererPage.isClosed() ? rendererPage : null;
  }

  async function createRenderer() {
    const p = await context.newPage();
    await p.setViewportSize(RENDERER_VIEWPORT).catch(() => {});
    p.on('close', () => {
      if (rendererPage === p) {
        rendererPage = null;
        sendEvent('renderer_closed', {});
      }
    });
    return p;
  }

  /** 候选阅读标签：浏览上下文里除渲染页外的 /web/reader/ 页面。 */
  async function listCandidates() {
    const out = [];
    for (const p of context.pages()) {
      if (p === rendererPage || p.isClosed()) continue;
      const parsed = parseReaderUrl(p.url());
      if (!parsed) continue;
      const title = (await p.title().catch(() => '')) || parsed.bookId;
      out.push({ title, url: parsed.url, bookId: parsed.bookId });
    }
    return out;
  }

  // ---------------- 周期上报：登录态与候选标签 ----------------

  let lastAuth = null;
  let lastCandidates = '';
  async function pollState() {
    if (exiting) return;
    try {
      const cookies = await context.cookies('https://weread.qq.com');
      const loggedIn = cookies.some((c) => c.name === 'wr_vid' && c.value);
      if (loggedIn !== lastAuth) {
        lastAuth = loggedIn;
        sendEvent('auth_changed', { loggedIn });
      }
    } catch {
      /* context 关闭中 */
    }
    try {
      const candidates = await listCandidates();
      const key = JSON.stringify(candidates);
      if (key !== lastCandidates) {
        lastCandidates = key;
        sendEvent('candidates_changed', { candidates });
      }
    } catch {
      /* 忽略 */
    }
  }
  pollTimer = setInterval(pollState, 2000);
  pollState().catch(() => {});

  // ---------------- 指令处理 ----------------

  /** 截取渲染页 400×300 设备画面（净化 CSS + 字体就绪 + 灰度拉伸）。 */
  async function captureRenderer(blackPoint) {
    const target = rendererPage;
    if (!target || target.isClosed()) {
      throw Object.assign(new Error('渲染页不存在或已关闭'), { code: 'no_renderer' });
    }
    await injectCss(target);
    await settlePaint(target);
    await target.evaluate(() => document.fonts.ready).catch(() => {});
    const raw = await target.screenshot({ type: 'png', clip: CAPTURE_CLIP });
    if (!blackPoint) return raw;
    const enhanced = await target.evaluate(enhanceInk, {
      pngB64: raw.toString('base64'),
      blackPoint,
      whitePoint: 250,
    });
    return Buffer.from(enhanced, 'base64');
  }

  async function pressAndCapture(key, delayMs, blackPoint) {
    const target = rendererPage;
    if (!target || target.isClosed()) {
      throw Object.assign(new Error('渲染页不存在或已关闭'), { code: 'no_renderer' });
    }
    await target.keyboard.press(key);
    await target.waitForTimeout(delayMs);
    return captureRenderer(blackPoint);
  }

  const handlers = {
    /** 打开/聚焦书架浏览页。 */
    async OPEN_LIBRARY() {
      if (!browsePage || browsePage.isClosed()) {
        browsePage = await context.newPage();
        await browsePage.setViewportSize(BROWSE_VIEWPORT).catch(() => {});
      }
      await browsePage.bringToFront().catch(() => {});
      if (!/weread\.qq\.com/.test(browsePage.url())) {
        await browsePage
          .goto(START_URL, { waitUntil: 'domcontentloaded', timeout: 60_000 })
          .catch(() => {});
      }
      return { kind: 'status', url: browsePage.url() };
    },

    /** 列出候选阅读标签（含书名与 URL）。 */
    async LIST_CANDIDATES() {
      return { kind: 'status', candidates: await listCandidates() };
    },

    /**
     * 在渲染页加载指定书籍并返回首帧。失败时尽量恢复旧渲染页，
     * 不把旧书画面当作新书首帧返回。
     */
    async LOAD_BOOK(payload) {
      const parsed = parseReaderUrl(payload && payload.url);
      if (!parsed) {
        throw Object.assign(new Error('非法阅读 URL：必须是 https://weread.qq.com/web/reader/<书籍ID>'), {
          code: 'invalid_url',
          recoverable: false,
        });
      }
      const previousUrl =
        rendererPage && !rendererPage.isClosed() ? rendererPage.url() : null;
      const target = ensureRenderer() || (rendererPage = await createRenderer());
      try {
        await target.goto(parsed.url, { waitUntil: 'domcontentloaded', timeout: 45_000 });
      } catch (e) {
        // 导航失败：恢复旧书（若有），新书未加载成功。
        if (previousUrl && parseReaderUrl(previousUrl)) {
          await target.goto(previousUrl, { waitUntil: 'domcontentloaded', timeout: 30_000 }).catch(() => {});
        }
        throw Object.assign(new Error('打开书籍失败：' + (e && e.message ? e.message : e)), {
          code: 'navigation_failed',
        });
      }
      await injectCss(target, true);
      const ready = await waitForContent(target, 15000);
      if (!ready) {
        if (previousUrl && parseReaderUrl(previousUrl)) {
          sendLog('新书内容渲染超时，恢复上一本书');
          await target.goto(previousUrl, { waitUntil: 'domcontentloaded', timeout: 30_000 }).catch(() => {});
        }
        throw Object.assign(new Error('书籍内容渲染超时'), { code: 'content_timeout' });
      }
      const buf = await captureRenderer(BLACK_POINT);
      archiveCapture(buf);
      return { kind: 'frame', pngB64: buf.toString('base64') };
    },

    async CAPTURE(payload) {
      const buf = await captureRenderer(numberOr(payload && payload.blackPoint, BLACK_POINT));
      archiveCapture(buf);
      return { kind: 'frame', pngB64: buf.toString('base64') };
    },

    async NEXT_PAGE(payload) {
      const buf = await pressAndCapture(
        'ArrowRight',
        numberOr(payload && payload.delayMs, DELAY_MS),
        numberOr(payload && payload.blackPoint, BLACK_POINT)
      );
      archiveCapture(buf);
      return { kind: 'frame', pngB64: buf.toString('base64') };
    },

    async PREV_PAGE(payload) {
      const buf = await pressAndCapture(
        'ArrowLeft',
        numberOr(payload && payload.delayMs, DELAY_MS),
        numberOr(payload && payload.blackPoint, BLACK_POINT)
      );
      archiveCapture(buf);
      return { kind: 'frame', pngB64: buf.toString('base64') };
    },

    /** 确认键：强制重写净化 CSS 并刷新当前渲染页画面。 */
    async REFRESH(payload) {
      const target = rendererPage;
      if (!target || target.isClosed()) {
        throw Object.assign(new Error('渲染页不存在或已关闭'), { code: 'no_renderer' });
      }
      await injectCss(target, true);
      await waitForContent(target, 5000);
      if (DIAG) {
        sendLog('刷新诊断：viewport=' + RENDERER_VIEWPORT.width + 'x' + RENDERER_VIEWPORT.height);
      }
      const buf = await captureRenderer(numberOr(payload && payload.blackPoint, BLACK_POINT));
      archiveCapture(buf);
      return { kind: 'frame', pngB64: buf.toString('base64') };
    },

    /** 关闭渲染页（浏览页保留，服务不退出）。 */
    async CLOSE_RENDERER() {
      if (rendererPage && !rendererPage.isClosed()) {
        await rendererPage.close().catch(() => {});
      }
      rendererPage = null;
      return { kind: 'ack' };
    },
  };

  function numberOr(v, fallback) {
    return typeof v === 'number' && Number.isFinite(v) && v >= 0 ? v : fallback;
  }

  // 指令串行化：同一时刻只执行一条页面操作。
  let chain = Promise.resolve();
  function enqueue(job) {
    const p = chain.then(job);
    chain = p.catch(() => {});
    return p;
  }

  const DEFAULT_TIMEOUT_MS = 30000;
  const TIMEOUTS_MS = { LOAD_BOOK: 60000 };

  const rl = readline.createInterface({ input: process.stdin });
  rl.on('line', (line) => {
    line = line.trim();
    if (!line) return;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      sendLog(`忽略非法指令行：${line.slice(0, 80)}`);
      return;
    }
    if (!msg || typeof msg.id !== 'number' || typeof msg.cmd !== 'string') return;
    enqueue(async () => {
      // 看门狗：超时返回结构化错误；完成时清理定时器，不泄漏。
      let watchdogTimer = null;
      const watchdog = new Promise((_, reject) => {
        watchdogTimer = setTimeout(
          () =>
            reject(
              Object.assign(new Error(`指令 ${msg.cmd} 执行超时，已中止`), {
                code: 'command_timeout',
              })
            ),
          TIMEOUTS_MS[msg.cmd] || DEFAULT_TIMEOUT_MS
        );
      });
      try {
        const handler = handlers[msg.cmd];
        if (!handler) {
          throw Object.assign(new Error(`未知指令：${msg.cmd}`), {
            code: 'unknown_command',
            recoverable: false,
          });
        }
        const result = await Promise.race([handler(msg.payload || {}), watchdog]);
        sendOk(msg.id, result);
      } catch (e) {
        clearTimeout(watchdogTimer);
        sendErr(
          msg.id,
          e.code || 'internal',
          e && e.message ? e.message : String(e),
          e.recoverable !== false
        );
        return;
      }
      clearTimeout(watchdogTimer);
    });
  });

  // Rust 端退出（stdin 关闭）→ 优雅关闭浏览器。
  rl.on('close', () => shutdown('stdin-closed'));

  // 启动即就绪：登录、选书均由管理页/浏览页流程完成，不作为就绪条件。
  sendEvent('controller_ready', {
    browserMode: BROWSER_MODE,
    profile: PROFILE_DIR,
    rendererViewport: RENDERER_VIEWPORT,
  });
}

main().catch((e) => {
  if (shuttingDown) {
    process.exit(0);
  }
  sendLog(`控制器启动失败：${e && e.message ? e.message : e}`);
  sendEvent('closed', { reason: 'startup-failed' });
  process.exit(1);
});
