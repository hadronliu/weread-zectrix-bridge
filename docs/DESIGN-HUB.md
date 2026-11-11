# 设计草案：独立选书与设备阅读管理

状态：A / B / C 三阶段已实现（系统 Chrome 与安装选择、显式 rendererPage、统一关闭预取、
超时与缓存修复、浏览页与渲染页分离、本地管理页、显式投送会话与首帧确认切换、
设置与最后投送书籍持久化、设备重连与浏览器重启）。D 阶段（精确单向位置同步、扩展入口、
Hub 对接）仍为设计。针对 Issue #1；基于本地提交 18297ec 的源码审查。

## 目标与边界

让电脑正常选书、浏览，设备通过专用页面阅读；使用系统 Chrome 减少安装体积。保留现有 Rust SDK 推图和 Playwright 网页渲染。首版为单账号、单设备、一本当前投送书籍，不提取正文、不绕过付费限制、不承诺跨端进度精确一致。不把普通阅读页面当作可随意修改的设备渲染页。

## 已确认的问题

- controller.js 的 findReaderPage() 取第一个阅读标签，目标会随标签顺序变化；周期 CSS 注入也使用这个目标。
- launchPersistentContext 全局设定 516×360，电脑选书也受设备尺寸限制；context 关闭会退出服务。
- Rust deliver_reply() 只接受 pngB64，不能表达登录、切书、状态等管理操作。
- PageCache::invalidate() 只清除 ahead 和 png，不清 history；切书、排版变化需要更强的 reset。
- action_confirm() 在 ahead 时先回退再刷新，若目标标签已变化，可能误翻新书。
- waitForContent() 超时仍继续；截图等待两帧不能证明正文绘制完成。
- Promise.race 的 watchdog 未清理定时器，超时不取消旧任务；不能直接释放串行队列运行下一条页面操作。
- ready 当前表示已打开阅读页，不能表达“服务已运行但尚未登录/未选书”。

## 推荐架构

本地管理页 → Rust 会话管理器 → JSON Lines → Node 浏览器适配器
设备实体按键 → Rust 同一会话队列 → 已绑定的专用渲染页 → PNG → SDK → 设备

Rust 是设备连接、会话版本与命令队列的唯一所有者；Node 负责登录窗口、浏览页、渲染页和截图。管理 HTTP 服务放在 Rust，避免 Node 直接写设备或形成第二条控制通道。UI 静态文件由本机提供，使用轻量 HTML/CSS/JS，不必引入完整前端框架。

浏览器模式：auto / chrome / chromium。auto 优先系统 Chrome，缺失时使用已安装的 Playwright Chromium；两者均缺失时给出安装说明，不在运行时静默下载。chrome 使用 Playwright channel: chrome，仍使用本项目专用 profile，不读取日常 Chrome 默认用户目录。安装脚本添加浏览器选择，Chrome 模式跳过 Chromium 下载。

首版浏览页与渲染页位于服务专属的同一持久化 context，共享已扫码登录状态；每个 page 分别 setViewportSize：浏览页约 1100×800，渲染页 516×360、DPR=1。必须在两种浏览器实测页面 viewport 独立生效；窗口尺寸可调不应改变截图排版。只有专用 rendererPage 引用可以注入 CSS、翻页与截图。不得继续扫描第一个阅读标签来确定设备目标。

若需要独立浏览器窗口，可通过服务管理的额外窗口实现，但首先验证生命周期与 profile 锁；不同时启动两个进程指向同一持久化目录。暂不采用复制 Cookie 到第二 profile 的方案，以免登录刷新与过期处理失控。

## 交互流程

1. 服务启动即展示管理页，不以进入正文作为管理服务就绪条件。设备连接可独立进行。
2. “打开书架 / 登录”打开完整的微信读书页面，保留网站导航。登录状态检测使用实际页面状态，不仅看 URL。
3. 用户在浏览页选书；管理页列出服务管理的阅读标签（标题和 URL），选中后点“投送此书”。可提供粘贴阅读链接入口，不强依赖私有书架 API。
4. Rust 暂停设备翻页命令，建立新 sessionId；Node 创建或导航专用渲染页，验证书籍身份与内容完成，再返回第一帧。
5. 设备推送确认成功后，Rust 更新当前投送会话并清空旧缓存；电脑继续选书不触发设备自动切书。
6. 设备上下键只操作 rendererPage；确认键只刷新设备当前书。管理页关闭不退出服务；渲染页关闭进入待恢复，浏览窗口关闭不关闭设备连接。
7. 新书加载失败时不显示旧缓存为新书；尽可能保持旧渲染页直到新书首帧成功，失败恢复旧目标。新页加载可能已改变云端位置，UI 明示此限制。

## 管理页首版内容

- 顶部：浏览器类型、设备连接状态、登录/待登录提示。
- 选书区：打开书架、候选阅读标签、当前候选与设备当前书分开展示、投送按钮。
- 设备区：400×300 最新推送预览、上下页、刷新、暂停、恢复。
- 设置：gray4 默认、mono/gray16 可选，黑点、翻页等待；左右留白先保持 12px。字号只在验证能触发可靠重排后开放，首版不展示当前未生效的 font-level 控件。
- 反馈：加载中、等待设备确认、成功、失败、登录失效。暂停保留连接和当前画面，设备翻页与管理页翻页均停止；浏览器自身仍可能发生云端同步。

预览是最新确认推送的帧，不宣称为设备屏幕实时回读。断线保留最后预览并明确标注。

## 会话与并发

状态：idle → loading → active ↔ paused；另有 auth_required、disconnected、error。设备连接状态和登录状态独立记录，避免一个枚举混合全部组合。

每次换书或排版变更递增 sessionId / layoutRevision；所有异步结果包含版本，旧帧不得推送。Rust 串行队列统一处理按键、HTTP 和渲染响应；长任务期间明确忽略或合并按键，不积压跨会话命令。

缓存键至少包含 sessionId、layoutRevision、页面序号（仅本地导航序号，不当作书籍稳定页码）。新增 reset_session() 清空 ahead、png、history。字体、灰阶、黑点变化分别使相应渲染/输出缓存失效，不能只替换最近图片。

禁用默认预取：Rust 直接运行和脚本默认保持一致。历史图像不得脱离 rendererPage 的实际位置单独改变设备页。推送失败后记录 browser/display 不一致，暂停后续翻页并允许重发当前帧或重新同步，不盲目再翻一页。

任务超时后不能只返回错误并让下一条任务执行；先恢复或重建渲染页、等待旧操作终止，再开放队列。清除 watchdog 定时器。内容等待超时返回结构化错误，不推白屏替换成功画面。

## 协议与接口草案

JSON Lines 增加协议版本和分类型响应：

- 请求：id、cmd、sessionId、payload。
- 响应：id、ok、sessionId、result（kind=status/frame/ack）；错误含 code、message、recoverable。
- 事件：controller_ready、auth_changed、candidates_changed、renderer_closed。
- 指令：LIST_CANDIDATES、OPEN_LIBRARY、LOAD_BOOK、CAPTURE、NEXT_PAGE、PREV_PAGE、REFRESH、CLOSE_RENDERER。

Node 事件必须转交 Rust 的事件通道，而非只记日志。保留旧 CLI 可用，协议升级同时修改两端，首版不承诺混用新旧版本。

HTTP：GET /api/state、GET /api/candidates、GET /api/preview；POST /api/library、/api/cast、/api/navigation、/api/pause、/api/resume、/api/settings。耗时操作返回 operationId，状态通过轮询查询，避免请求重试重复翻页。客户端请求附 requestId，服务执行幂等去重。

只绑定 127.0.0.1，校验 Host、Origin 和会话令牌，拒绝跨站写请求；不开放任意代理或任意 URL 导航。阅读 URL 使用 URL 解析器校验 HTTPS、hostname 精确等于 weread.qq.com、路径为 /web/reader/ 下有效非空书籍路径；禁止字符串 includes 判断域名。preview、状态和书籍信息也需访问控制。

## 阅读进度策略

“投送书籍”首版仅保证同书，不保证与电脑当前段落一致。阅读 URL 是否编码章节和位置需实测，不能当作既成事实。

“同步当前位置”先做能力验证：能获得稳定书籍ID、章节和段落锚点才开放；不能定位时降级为同书/章节并要求用户确认。电脑与设备页宽不同，绝不直接复用页码。首次实现只做单向电脑→设备，设备→电脑后续评估。

同一账号的云端位置可能被两个页面覆盖。先提供明确提示与手动同步，关闭预取；不拦截未知进度 API、不声称阅读进度隔离。验证期间观察刷新、后台放置、手机换书后的行为。若网页自动拉取云端位置使设备跳页，应提示用户并暂停，而非保证无法实现的完全独立阅读。

## 分阶段交付与验收

A：系统 Chrome、安装选择、显式 rendererPage、统一关闭默认预取、修复超时/缓存 reset。验收 Chrome 无需下载 Chromium 可登录；多个标签与书籍互不误选；退出释放连接。

B：浏览/设备页分离、本地管理页、显式选书投送、会话版本、首帧确认切换。验收电脑换书不改变设备绑定；旧帧迟到不能覆盖新书；关闭管理页可继续实体按键阅读。

C：登录失效、设备断连恢复、持久化设置和最后一次成功投送书籍。只保存书籍与配置，无法可靠定位时恢复同书并提示，不宣称恢复精确段落。

D：验证后增加精确单向位置同步、扩展入口、Hub 对接。接入 Hub 前需要确认官方扩展协议、设备连接所有权与按键转发接口，不根据名称猜测已有能力。

自动验证：模拟 Node 响应/设备事件，覆盖换书期间按键、旧会话迟到、重复 HTTP 请求、推送失败、超时后重建、非法 URL。浏览器本地 fixture 验证浏览页不注入 CSS、两页 viewport 独立、候选与渲染目标绑定。无需在 CI 登录真实账号。

人工验证：系统 Chrome 与 Chromium、首次登录和过期重登、至少两本书连续切换、目录/简介/正文/插图、首末行与分页衔接、跨端进度冲突、断网重连、关闭不同窗口；设备验证 gray4 与 12px 留白。没有实测前不把设计标为已支持。

## 参考

- Issue：https://github.com/hadronliu/weread-zectrix-bridge/issues/1
- Chrome channel：https://playwright.dev/docs/browsers#google-chrome--microsoft-edge
- Page 独立视口：https://playwright.dev/docs/api/class-page#page-set-viewport-size
