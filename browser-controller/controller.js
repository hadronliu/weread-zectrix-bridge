#!/usr/bin/env node
'use strict';

/**
 * 微信读书（weread.qq.com）浏览器渲染与截屏控制器。
 *
 * 由 Rust 桥接服务（weread-zectrix-bridge）作为子进程启动，通过 stdin/stdout 的
 * JSON Lines 协议通信：
 *
 *   请求（Rust → Node）：{"id":1,"cmd":"CAPTURE"|"NEXT_PAGE"|"PREV_PAGE"|"REFRESH"}
 *   响应（Node → Rust）：{"id":1,"ok":true,"pngB64":"<400×300 PNG 的 Base64>"}
 *                        {"id":1,"ok":false,"error":"..."}
 *   事件（Node → Rust）：{"event":"ready","url":"..."}
 *                        {"event":"log","message":"..."}
 *                        {"event":"closed","reason":"..."}
 *
 * - 使用持久化 userDataDir 保存微信读书登录状态，首次运行请在弹出的浏览器窗口中
 *   扫码登录并进入书籍正文页，检测到阅读页后自动注入净化 CSS 并上报 ready。
 * - 视口固定 400×300、deviceScaleFactor=1，与 ZECTRIX Note 4 墨水屏 1:1 像素对齐。
 * - 截图做可调灰度拉伸，不做二值化/抖色，最终量化交由 SDK 处理。
 */

const fs = require('fs');
const path = require('path');
const readline = require('readline');
const { chromium } = require('playwright');
const { enhanceInk } = require('./ink');
// Set WEREAD_INK_BLACK_POINT=0 to retain the original screenshot.
const INK_BLACK_POINT = Number(process.env.WEREAD_INK_BLACK_POINT ?? 200);
if (!Number.isFinite(INK_BLACK_POINT) || INK_BLACK_POINT < 0 || INK_BLACK_POINT >= 250) {
  throw new Error('WEREAD_INK_BLACK_POINT must be in [0, 250)');
}

// ---------------------------------------------------------------- 命令行参数

function argValue(name, fallback) {
  const i = process.argv.indexOf(name);
  return i !== -1 && process.argv[i + 1] !== undefined ? process.argv[i + 1] : fallback;
}

/** 翻页后等待 DOM/Canvas 重绘稳定的毫秒数，与 Rust 端 --delay-ms 对应。 */
const DELAY_MS = Math.max(0, parseInt(argValue('--delay-ms', '200'), 10) || 200);
const START_URL = argValue('--url', 'https://weread.qq.com');
/** 持久化会话目录：保存登录 Cookie 与浏览器状态。 */
const PROFILE_DIR = argValue('--user-data-dir', path.join(__dirname, '.user-data'));
/** 等待用户扫码登录并进入正文页的最长时间（首次运行较久）。 */
const READY_TIMEOUT_MS = Math.max(
  60_000,
  parseInt(argValue('--ready-timeout-ms', String(15 * 60 * 1000)), 10) || 15 * 60 * 1000
);

/** ZECTRIX Note 4 物理分辨率：400×300，1:1 像素对齐。 */
const VIEWPORT = { width: 400, height: 300 };
// WeRead reserves 70px on each side even on small screens. Let its own
// paginator lay out a wider page, then remove only the outer whitespace.
// Keep 12px side margins and the chapter header; never resize canvas pixels.
const READER_VIEWPORT = { width: 516, height: 360 };
const CAPTURE_CLIP = { x: 58, y: 30, width: 400, height: 300 };

/** 正文强制字号（px）；0 表示不覆盖（推荐用字号档位而非 px 覆盖）。 */
const READER_FONT_SIZE = Math.max(0, parseInt(argValue('--font-size', '0'), 10) || 0);

/**
 * 微信读书新版阅读器（wr_horizontalReader）的字号档位：body 上的
 * wr_reader_font_size_level_N class。0 表示沿用账号当前档位。
 */
const READER_FONT_LEVEL = Math.max(0, parseInt(argValue('--font-level', '4'), 10) || 0);

/** 触发微信读书重新分页后等待重排完成的毫秒数。 */
const REPAGINATE_SETTLE_MS = Math.max(
  300,
  parseInt(argValue('--repaginate-ms', '900'), 10) || 900
);

/** 记住上次的阅读页 URL，重启后自动续读。 */
const LAST_READER_FILE = path.join(__dirname, '.last-reader-url');

// ---------------------------------------------------------------- 输出工具

function sendEvent(name, fields = {}) {
  process.stdout.write(JSON.stringify({ event: name, ...fields }) + '\n');
}

function sendLog(message) {
  sendEvent('log', { message });
}

function sendReply(id, ok, fields = {}) {
  process.stdout.write(JSON.stringify({ id, ok, ...fields }) + '\n');
}

/** 已进入主动关闭流程：后续异常不再按“启动失败”上报。 */
let shuttingDown = false;

// -------------------------------------------- 微信读书墨水屏排版净化 CSS

/**
 * 目标：隐藏顶部导航栏、左右悬浮工具栏、目录/笔记/设置等弹层与页脚，
 * 让正文铺满 400×300 视口，并强制纯白背景 / 纯黑文字以增强墨水屏对比度。
 * 微信读书分页由其 JS 按容器尺寸计算，注入后需触发 resize 让它重新分页。
 */
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
/* 新版阅读器“上一页/下一页”提示条：物理按键已承担翻页，腾出正文空间 */
.renderTarget_pager { display: none !important; }

/* 2) 阅读容器全幅铺满 400×300 视口：清零桌面版预留的边栏与上下留白。
      注意：不要对新版阅读器的 wr_* 容器强制 height:100%——会破坏其内部
      高度计算导致渲染容器塌陷（曾引发整页白屏）。 */
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

/* Cover/intro pages use 50px padding, unlike the canvas reader's 70px.
   Align both to the same capture-safe area (58px crop + 12px margin).
   Keep automatic height so long introductions can still paginate. */
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

/* 3) 墨水屏对比度优化：纯白背景 + 纯黑文字。
      微信读书会用低不透明度把正文调成浅灰（黑字×opacity≈灰字），
      灰字经黑白量化+抖色后“发虚”——必须强制 opacity 归一。 */
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

/* 5) 墨水屏清晰度：关闭字体抗锯齿与文字阴影，笔画边缘纯黑白，
      mono 量化后不发虚（macOS Chromium 生效）。 */
*, *::before, *::after {
  -webkit-font-smoothing: none !important;
  font-smooth: never !important;
  text-shadow: none !important;
}
${fontRule}`;
}

const CLEANUP_CSS = buildCleanupCss(READER_FONT_SIZE);

const CSS_STYLE_ID = 'weread-zectrix-clean-css';

/**
 * 注入净化 CSS。幂等：默认仅在不存在的时机注入；force=true 时强制重写
 * （供确认键“重新注入美化 CSS”使用）。
 */
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

/**
 * 等待两帧 requestAnimationFrame，确保截图前页面已完成重绘。
 * 有界等待：后台/被遮挡标签的 rAF 会被 Chromium 暂停，用 500ms 定时器兜底，
 * 避免永久挂起（曾导致截屏指令 30s 超时）。
 */
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

function isReaderUrl(url) {
  return typeof url === 'string' && url.includes('weread.qq.com') && url.includes('/web/reader');
}

/**
 * 等待阅读内容真正渲染：渲染容器高度非零且含可见文本。
 * 新版阅读器（renderTargetContainer）重渲染期间容器高度会暂时塌陷为 0，
 * 此时截图会得到白屏——就绪与刷新前必须等待。
 */
async function waitForContent(page, timeoutMs = 8000) {
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
  sendLog('等待阅读内容渲染超时，继续执行（可能截到未完成的画面）');
  return false;
}

/**
 * 触发微信读书重新分页：派发 resize 事件并等待重排完成。
 * 注意：仅在确认键刷新时使用；启动时的自动触发曾导致渲染容器塌陷（白屏）。
 * 字号档位（wr_reader_font_size_level_N）是账号云端状态，重渲染会被同步
 * 覆盖，无法通过 DOM 修改——需在微信读书自己的设置面板中调整。
 */
async function triggerRepagination(page) {
  await page.evaluate(() => {
    window.dispatchEvent(new Event('resize'));
  });
  await new Promise((r) => setTimeout(r, REPAGINATE_SETTLE_MS));
}

/**
 * 排版诊断：追踪正文文本节点的祖先链（transform/zoom/字号/盒尺寸/内边距），
 * 统计小字号元素，把结果打到日志，供精确校准 CSS。
 */
async function reportDiagnostics(page) {
  try {
    const info = await page.evaluate(() => {
      const out = {
        viewport: [window.innerWidth, window.innerHeight],
        chain: [],
        smallFonts: [],
        chapterRect: null,
      };
      const chap = document.querySelector('.readerChapterContent');
      if (chap) {
        const r = chap.getBoundingClientRect();
        out.chapterRect = [Math.round(r.width), Math.round(r.height)];
      }
      // 找可见文本最多的元素作为正文代表
      let best = null;
      let bestLen = 0;
      const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
      let n;
      while ((n = walker.nextNode())) {
        const t = (n.textContent || '').trim();
        if (t.length > bestLen && n.parentElement) {
          const cs = getComputedStyle(n.parentElement);
          if (cs.display !== 'none' && cs.visibility !== 'hidden') {
            best = n.parentElement;
            bestLen = t.length;
          }
        }
      }
      if (best) {
        let el = best;
        for (let depth = 0; el && depth < 14; depth++) {
          const cs = getComputedStyle(el);
          const cls = (el.getAttribute('class') || '').toString().slice(0, 70);
          out.chain.push({
            tag: el.tagName,
            cls,
            fontSize: cs.fontSize,
            transform: cs.transform !== 'none' ? cs.transform.slice(0, 70) : 'none',
            zoom: cs.zoom || '1',
            rect: [
              Math.round(el.getBoundingClientRect().width),
              Math.round(el.getBoundingClientRect().height),
            ],
            padding: cs.padding,
          });
          el = el.parentElement;
        }
      }
      // 小字号元素普查（≤15px 且含可见文本），新版阅读器根节点优先
      const census = new Map();
      const roots = document.querySelectorAll('.wr_horizontalReader, .readerContent');
      roots.forEach((root) =>
        root.querySelectorAll('*').forEach((el) => {
          const cs = getComputedStyle(el);
          if (
            parseFloat(cs.fontSize) <= 15 &&
            cs.display !== 'none' &&
            (el.textContent || '').trim()
          ) {
            const key = `${el.tagName}.${(el.getAttribute('class') || '').split(' ')[0]}@${cs.fontSize}`;
            census.set(key, (census.get(key) || 0) + 1);
          }
        })
      );
      out.smallFonts = [...census.entries()]
        .sort((a, b) => b[1] - a[1])
        .slice(0, 8);
      // 当前字号档位与翻页提示元素
      out.fontLevel = (document.body.className.match(/wr_reader_font_size_level_\d+/) || ['none'])[0];
      out.pageHints = [];
      document.querySelectorAll('button, span, div, a').forEach((el) => {
        const t = (el.textContent || '').trim();
        if ((t === '上一页' || t === '下一页') && el.children.length <= 2) {
          out.pageHints.push(`${el.tagName}.${(el.getAttribute('class') || '').slice(0, 50)}`);
        }
      });
      // 渲染树转储：renderTargetContainer 下 3 层结构（定位页面盒留白的来源）
      out.renderTree = [];
      const rt = document.querySelector('.renderTargetContainer');
      if (rt) {
        const walk = (el, depth) => {
          if (depth > 3 || out.renderTree.length > 24) return;
          const cs = getComputedStyle(el);
          const r = el.getBoundingClientRect();
          out.renderTree.push(
            `${'  '.repeat(depth)}${el.tagName}.${(el.getAttribute('class') || '').split(' ')[0].slice(0, 40)} ${Math.round(r.width)}x${Math.round(r.height)} pad:${cs.padding}`
          );
          for (const child of Array.from(el.children).slice(0, 6)) walk(child, depth + 1);
        };
        walk(rt, 0);
      }
      return out;
    });
    sendLog('排版诊断 ' + JSON.stringify(info));
  } catch (e) {
    sendLog('排版诊断失败：' + (e && e.message ? e.message : e));
  }
}

/** 最近一次成功截屏的存档路径，供人工比对浏览器端原始画面。 */
const LAST_CAPTURE_FILE = path.join(__dirname, '.last-capture.png');
function archiveCapture(buf) {
  try {
    fs.writeFileSync(LAST_CAPTURE_FILE, buf);
  } catch {
    /* 存档失败不影响主流程 */
  }
}

// ---------------------------------------------------------------- 主流程

async function main() {
  fs.mkdirSync(PROFILE_DIR, { recursive: true });

  sendLog(`正在启动 Chromium（会话目录：${PROFILE_DIR}）…`);
  const context = await chromium.launchPersistentContext(PROFILE_DIR, {
    headless: false,
    // 严格 400×300、1:1 像素，避免缩放导致文字边缘发虚。
    viewport: READER_VIEWPORT,
    deviceScaleFactor: 1,
    args: ['--window-size=536,420', '--disable-blink-features=AutomationControlled'],
  });

  let exiting = false;
  let cssTimer = null;
  async function shutdown(reason) {
    if (exiting) return;
    exiting = true;
    shuttingDown = true;
    if (cssTimer) clearInterval(cssTimer);
    sendEvent('closed', reason ? { reason } : {});
    try {
      await context.close();
    } catch {
      /* 忽略关闭时的异常 */
    }
    process.exit(0);
  }

  // 用户直接关闭浏览器窗口 → 通知 Rust 端退出。
  context.on('close', () => shutdown('browser-window-closed'));

  /** 阅读页可能在任意标签页打开（微信读书会新开标签）：扫描全部标签。 */
  function findReaderPage() {
    return context.pages().find((p) => !p.isClosed() && isReaderUrl(p.url())) || null;
  }

  /** 指令目标标签：优先阅读页标签，否则退回主标签。 */
  function targetPage() {
    return findReaderPage() || page;
  }
  process.on('SIGTERM', () => shutdown('SIGTERM'));
  process.on('SIGINT', () => shutdown('SIGINT'));

  // 复用已打开的微信读书标签页，否则新开并导航。
  // 上次记录过阅读页则直接续读，免去手动找书打开。
  let savedReaderUrl = '';
  try {
    savedReaderUrl = fs.existsSync(LAST_READER_FILE)
      ? fs.readFileSync(LAST_READER_FILE, 'utf8').trim()
      : '';
  } catch {
    savedReaderUrl = '';
  }
  const initialTarget =
    savedReaderUrl && isReaderUrl(savedReaderUrl) ? savedReaderUrl : START_URL;

  let page = context.pages().find((p) => /weread\.qq\.com/.test(p.url()));
  if (!page) page = await context.newPage();
  if (!/weread\.qq\.com/.test(page.url())) {
    sendLog(`正在打开 ${initialTarget}${savedReaderUrl ? '（自动续读上次书籍）' : ''} …`);
    await page
      .goto(initialTarget, { waitUntil: 'domcontentloaded', timeout: 60_000 })
      .catch((e) => sendLog(`打开页面失败：${e && e.message ? e.message : e}`));
  }
  await page.bringToFront().catch(() => {});

  // SPA 路由切换 / 章节跳转后可能重建 DOM，周期性幂等补注 CSS。
  cssTimer = setInterval(() => {
    if (exiting) return;
    const reader = findReaderPage();
    if (reader) injectCss(reader).catch(() => {});
  }, 3000);

  // ---------------------------------------------------------------- 指令处理

  /** 校验并返回指令目标标签（阅读页标签）。 */
  function ensureReader() {
    const target = targetPage();
    if (target.isClosed()) throw new Error('阅读页已被关闭');
    if (!isReaderUrl(target.url())) throw new Error(`当前不在微信读书阅读页（${target.url()}）`);
    return target;
  }

  /** 截取目标标签视口 400×300 原始 PNG（截屏前保证净化 CSS 在场）。 */
  async function capturePng(target) {
    await injectCss(target);
    await settlePaint(target);
    await target.evaluate(() => document.fonts.ready);
    const raw = await target.screenshot({
      type: 'png',
      clip: CAPTURE_CLIP,
    });
    fs.writeFileSync(path.join(__dirname, '.last-capture-raw.png'), raw);
    if (INK_BLACK_POINT === 0) return raw;
    const enhanced = await target.evaluate(enhanceInk, {
      pngB64: raw.toString('base64'), blackPoint: INK_BLACK_POINT, whitePoint: 250,
    });
    return Buffer.from(enhanced, 'base64');
  }

  /** 发送翻页键 → 等待重绘稳定 → 截屏。 */
  async function pressAndCapture(key) {
    const target = ensureReader();
    await target.keyboard.press(key);
    await target.waitForTimeout(DELAY_MS);
    return capturePng(target);
  }

  const handlers = {
    CAPTURE: async () => capturePng(ensureReader()),
    NEXT_PAGE: () => pressAndCapture('ArrowRight'),
    PREV_PAGE: () => pressAndCapture('ArrowLeft'),
    /** 确认键：强制重写净化 CSS 后刷新当前页画面（不再自动触发 resize，避免白屏）。 */
    REFRESH: async () => {
      const target = ensureReader();
      await injectCss(target, true);
      await waitForContent(target, 3000);
      await reportDiagnostics(target);
      await settlePaint(target);
      return capturePng(target);
    },
  };

  // 指令串行化，避免并发截图互相干扰。
  let chain = Promise.resolve();
  function enqueue(job) {
    const p = chain.then(job);
    chain = p.catch(() => {});
    return p;
  }

  // 立即挂载 stdin 指令监听：ready 之前收到的指令会得到结构化错误响应，
  // 而不是被静默丢弃。
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
      // 看门狗：任何指令超过 25s 视为卡死，返回明确错误而非让 Rust 侧静默超时。
      const watchdog = new Promise((_, reject) => {
        setTimeout(() => reject(new Error(`指令 ${msg.cmd} 执行超过 25s，已中止（页面可能失去响应）`)), 25000);
      });
      try {
        const handler = handlers[msg.cmd];
        if (!handler) throw new Error(`未知指令：${msg.cmd}`);
        const buf = await Promise.race([handler(), watchdog]);
        archiveCapture(buf);
        sendReply(msg.id, true, { pngB64: buf.toString('base64') });
      } catch (e) {
        sendLog(`指令 ${msg.cmd} 失败：${e && e.message ? e.message : e}`);
        sendReply(msg.id, false, { error: e && e.message ? e.message : String(e) });
      }
    });
  });

  // Rust 端退出（stdin 关闭）→ 优雅关闭浏览器。
  rl.on('close', () => shutdown('stdin-closed'));

  // 等待用户进入正文阅读页（首次运行需扫码登录，耗时较长）。
  // 微信读书可能在新标签页打开正文：轮询扫描所有标签，命中后自动切换。
  const readyTimeoutMin = Math.round(READY_TIMEOUT_MS / 60000);
  sendLog('请在浏览器窗口中完成：1) 扫码登录微信读书；2) 打开任意书籍进入正文阅读页。等待中…');
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const deadline = Date.now() + READY_TIMEOUT_MS;
  let lastLoggedUrl = '';
  while (true) {
    const reader = findReaderPage();
    if (reader) {
      page = reader;
      break;
    }
    const current = !page.isClosed() ? page.url() : '（标签已关闭）';
    if (current !== lastLoggedUrl) {
      lastLoggedUrl = current;
      sendLog(`当前页面：${current}（仍在等待正文阅读页…）`);
    }
    if (Date.now() > deadline) {
      throw new Error(`等待进入微信读书正文页超时（${readyTimeoutMin} 分钟），请重新运行并完成扫码登录`);
    }
    await sleep(800);
  }
  await page.bringToFront().catch(() => {});
  await injectCss(page);
  // 等待阅读内容真实渲染完成，避免把白屏推给设备。
  sendLog('正在等待阅读内容渲染…');
  await waitForContent(page);
  await reportDiagnostics(page);

  // 记录本次阅读页，供下次启动自动续读。
  try {
    fs.writeFileSync(LAST_READER_FILE, page.url());
  } catch {
    /* 记录失败不影响运行 */
  }
  sendEvent('ready', { url: page.url() });
  sendLog('控制器已就绪：净化 CSS 已注入，开始接受翻页 / 截屏指令。');
}

main().catch((e) => {
  // 主动关闭（信号 / 关窗 / stdin 结束）期间抛出的异常不算启动失败。
  if (shuttingDown) {
    process.exit(0);
  }
  sendLog(`控制器启动失败：${e && e.message ? e.message : e}`);
  sendEvent('closed', { reason: 'startup-failed' });
  process.exit(1);
});
