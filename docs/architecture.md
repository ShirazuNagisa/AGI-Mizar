# 内核架构

本文说明 `crates/` 里每个部分为什么长这样，以及哪些地方是刻意留白的。
产品规划见工作区 `docs/agi-mizar-plan-2026-10-27.md`。

---

## 1. 一句话

**内核是一个守护进程，Tauri 只是它的窗口。**

```
┌─────────────────────────────────────────────────────────┐
│ ui/            React：只认本地 API，不 import 任何 Tauri 模块 │
├─────────────────────────────────────────────────────────┤
│ 本地 API       loopback HTTP + WS，Bearer token            │
├─────────────────────────────────────────────────────────┤
│ agi-core       状态聚合、命令分发、事件广播                  │
├─────────────────────────────────────────────────────────┤
│ agi-engine     Engine trait（三个引擎共用的接口）            │
├─────────────────────────────────────────────────────────┤
│ 适配层         DSH（链路服务端）│ Codex │ Claude Code       │
└─────────────────────────────────────────────────────────┘
```

关掉窗口不退出内核；退出应用才停内核。

---

## 2. 为什么不用 Tauri command

`#[tauri::command]` 会把前端绑死在壳上：不能在浏览器里打开、不能脱离壳测试、CLI 也调不到。
把内核做成守护进程 + 本地 HTTP API，换来三件事：

1. **前端可以 `vite dev` 直接对着真内核调试**，不用起 Tauri。
2. **同一套 API 供三种客户端**：窗口、`agi-daemon` 的 curl、未来的无头部署。
3. **契约只有一个**：`Command` 是一个 tagged enum，生成的 TS 是一个联合类型；未知 `op`
   是明确的错误，而不是可能被误读成"旧客户端"的 404。

Tauri 剩下的职责：起内核、开窗、把 `{port, token}` 注入 `window.__AGI_MIZAR__`。

---

## 3. 三层类型

| 层 | 位置 | 谁读 |
|---|---|---|
| 领域模型 `Session` / `Record` / `Interaction` | `agi-protocol::domain` | 内核、UI、测试 |
| 引擎接口 `Engine` + `EngineEvent` | `agi-engine` | 内核 |
| 两条线：DSH Link v1、本地 API | `agi-protocol::{link, api}` | 适配层 / UI |

领域模型刻意窄：只装一块对话界面会画的东西。引擎知道的、模型表达不了的，走各自的逃生通道
（DSH 的 `invoke`），不上公共面。

### 会话键

```
dsh:session-123
claude:9f2c…
codex:th_…
```

引擎自己的 id 原样保留，前缀只用来防碰撞——DSH 和 Claude Code 都会发 UUID 形状的 id，早晚撞。

---

## 4. 引擎接口

```rust
async fn sessions(&self) -> Result<Vec<Session>>;
async fn records(&self, session, limit, before) -> Result<Vec<Record>>;
async fn prompt(&self, session, text) -> Result<()>;
async fn cancel(&self, session) -> Result<()>;
async fn answer(&self, id, answer) -> Result<()>;
async fn set_model(&self, session, model) -> Result<()>;
async fn set_permission(&self, session, preset) -> Result<()>;
fn capabilities(&self) -> Capabilities;
fn status(&self) -> (bool, Option<String>);
fn subscribe(&self) -> broadcast::Receiver<EngineEvent>;
```

两个刻意的决定：

- **事件推送，不轮询。** 三个引擎的更新机制完全不同（WebSocket 火管、JSON-RPC 通知、文件追加），
  统一成一条 `EngineEvent` 流，UI 只有一个真相来源。
- **`capabilities` 驱动界面。** 不支持的能力按钮不画，而不是点了再报错。DSH 0.9.0 当前
  `attachments` / `fork` / `archive` / `file_preview` 都是 `false`——它们能通过 `invoke` 做，
  但还没接线，如实标成不支持比假装支持好。

---

## 5. DSH 链路：方向是反的

另外两个引擎是本应用 spawn 出来的；DSH 是**插件拨进来**。所以 `agi-engine-dsh` 实现的是
**服务端**一侧：握手、帧循环、请求关联。

```
插件 ──WS──▶ agi-core::server 的 /dsh 路由
                    │ 拆成两条 String 通道
                    ▼
             DshEngine::serve(incoming, outgoing)
```

`serve` 收的是两条**文本帧通道**而不是 socket。HTTP 服务器管 socket，引擎管协议——这是让
"握手 + 请求关联 + 历史分页"能在没有网络的情况下被测试的原因（`tests/link.rs` 就是两个 channel）。

### 一个必须踩过才知道的坑

`serve` 里读会话列表**不能 await**。列表的应答是作为帧回来的，而帧只有进入读循环才会被送达；
先 await 列表，就等于等一个只有停止等待才开始流动的回复。现在是 spawn 一个任务去读。

### 历史页怎么读

```
session/follow（流，只取第一帧）→ cursor
session/page（unary，throughSeq = cursor）→ records
```

只取 follow 的第一帧，然后取消。拉第二帧会唤醒冷会话的 agent——这是 0.8.x 实测出来的。

---

## 6. 状态聚合

`agi-core::Core` 订阅每个引擎的事件，折进一个 `Store`，再广播成 `ApiEvent`。

- **一个有序视图。** 会话按 `updated_at` 倒序，就是会话列表唯一的读法。
- **内存态，不落盘。** 会话日志是引擎自己的持久记录，第二份拷贝就是第二个真相。SQLite 等需要它的
  功能（搜索、设置、交付文件索引）再上。
- **慢订阅者不会拖住内核。** 广播缓冲 1024；落后的订阅者丢事件，下一次 `state` 是权威的。

### 锁的规则

`Store` 和引擎内部状态都用 `std::sync::Mutex/RwLock`，**持锁期间不 await**。所有跨 await 的动作
先 clone 出需要的东西。这条规则比锁的类型重要。

---

## 7. Codex：唯一由我们 spawn 的引擎

三个引擎里，Codex 是唯一一个**进程属于我们**的：`codex app-server` 由我们拉起，stdio 上跑
newline-delimited JSON-RPC，死了就重启。DSH 是插件拨进来，Claude Code 是每轮一个子进程。

### 三层的分法

```
rpc.rs         子进程、分帧、请求关联、服务端请求、重启
projection.rs  纯函数：thread + turns → 记录（含 seq 编号规则）
engine.rs      Engine 实现：请求、审批、看护循环
```

`rpc.rs` 不认识 thread，`projection.rs` 不做 I/O。所以 Codex 改协议是改一个文件，而投影规则
（哪条 item 是消息、哪条是工具）可以被单测钉住。

### 审批是"请求"，不是"通知"

Codex 停下来等权限时，是**给我们发一个带 `id` 的请求**并等回复。这让它成为三个引擎里最好应答的
一个：回复就是答案，没有 waterfall 要加入，也没有 broker 要抢。唯一的活是把 JSON-RPC 的 id 留到
操作者做决定。

回复的形状按方法不同：

| 方法 | 允许 | 拒绝 |
|---|---|---|
| `item/commandExecution/requestApproval` | `{decision:"accept"}` | `{decision:"decline"}` |
| `execCommandApproval`（旧） | `{decision:"approved"}` | `{decision:{denied:{rejection}}}` |
| `item/permissions/requestApproval` | `{permissions:<请求的>, scope:"turn"}` | `{permissions:{}, scope:"turn"}` |

拒绝时**不回显**请求的权限，否则一次拒绝反而扩大了权限。

### 序号是算出来的，不是等来的

Codex 的 turn 只有 `id` 和 `startedAt`（**秒**），粗到两个 turn 可以同秒。所以记录的 `seq`
由"第几个 turn × 1,000,000 + 第几个 item × 10"算出来：

```
turn 1  turn/start     1_000_000
        user message   1_000_010
        tool call      1_000_020
        turn/end       1_001_999
turn 2  turn/start     2_000_000
```

跨读取稳定、线程内唯一、顺序显然。这也是为什么实时通知**不**直接追加记录：`item/completed`
没有序号，编一个就会和历史读取的编号撞车。实时只把行标记为"脏"，转写仍来自
`thread/turns/list` 这一个来源。

### 子进程死了是"断线"，不是"报错"

app-server 退出时，所有缓存的 thread 视图都作废——新起的 server 加载的 thread 集合不同。所以
看护循环清空缓存、广播状态变化、退避后重启；UI 看到的是一个状态，不是一叠失败调用。

## 8. Claude Code：三条缝拼起来的引擎

三个引擎里最难接的一个：它**没有入站服务**，也不会让别人往一个开着的窗口里塞消息。所以"控制"
不是一个通道，而是三条互不相干的缝拼起来的：

| 缝 | 用来做什么 | 局限 |
|---|---|---|
| transcript（`~/.claude/projects/<slug>/<id>.jsonl`） | 读会话与历史 | 只读；文件追加不代表"在跑" |
| hooks（8 个事件） | 观察运行中的会话；`PreToolUse` 能应答 | 要写进 `~/.claude/settings.json` |
| `claude -p --resume <id>` | 发起一轮 | 一轮一个进程 |

### 一轮的边界是推出来的

文件里没有"第 3 轮"。轮次由**人类提问**界定：一条 `type:"user"`、内容有文本、不是 tool result、
不是 sidechain、不是 `isMeta` 的记录。后面三条都是实测出来的——真实 transcript 里，由 skill 启动的
会话第一条 user 记录是 900 字的 skill 正文，把它当标题会让所有这类会话看起来一样。

`<command-message>`、`<task-notification>` 这类包装也是真实的一轮（操作者确实输入了 `/命令`），
但**不是标题**：它们全都长一样，说不出这个会话在做什么。

### 序号也是字节偏移

一行可能同时承载轮次标记和消息，所以 `seq = 行首字节偏移 * 10 + 槽位`。行号会在重写时移动，时间戳
在毫秒级会撞、而且 `custom-title` 这类记录根本没有时间戳；字节偏移不会动。

### 会话"在跑"只能靠 hook 说

transcript 只能证明"写过什么"，不能证明"还在写"——被中断的会话和正在跑的会话在磁盘上长得一样。
所以 `UserPromptSubmit` 打开一个 active 标记、`Stop` 关掉它，`records()` 据此决定最后一轮要不要
画结束标记。

### hooks 是阻塞的，所以必须有人看着

`PreToolUse` 会一直等到我们回答（上限 55 秒，比 Claude Code 自己的 60 秒短）。如果没人看着就阻塞，
操作者自己的终端会为每次工具调用停一分钟——比不装 hook 更糟。所以内核数着打开的 UI 事件连接，
零个的时候 hook 立刻返回空，把决定权还给 Claude Code。

这也是唯一一个"内核知道 UI 在不在"的地方，见 `server.rs` 的 `Attendance`。

### 三个令牌

| 令牌 | 谁用 | 放哪 |
|---|---|---|
| plugin token | DSH 插件 | `endpoint.json` |
| api token | UI | 只打印 / 注入页面 |
| hook token | `agi-mizar-hook` | `hook.json` |

hook 每次工具调用都会跑，而且是被 Claude Code 调起来的——一个能应答审批的令牌不该同时能驱动窗口。

### 安装是显式的

`agi-daemon --install-claude-hooks` 才会改 `~/.claude/settings.json`：先备份、按命令里的标记识别
（幂等）、只增不删别人的 hook、卸载时只删自己的。为了快速看一眼而起的守护进程不该动用户的家目录。

## 9. Mizar 服务端：唯一一张对外的脸

桌面端有三张脸：本地 API 给自己的窗口、DSH 链路给插件、**Mizar v1 给手机**。只有最后一张要在操作者
控制不了的网络上工作，所以只有它有配对、令牌和限流。

### 三条路由，两条免鉴权

| 路由 | 鉴权 | 干什么 |
|---|---|---|
| `GET /mizar/v1/hello` | 无 | "这个地址能拨通吗，是谁" |
| `POST /mizar/v1/pair` | 无，一次性 | 用配对码换设备令牌 |
| `GET /mizar/v1/ws` | 第一帧里的设备令牌 | 其余全部 |

免鉴权的暴露面是刻意划死的：`hello` 只说主机 id 和名字；`pair` 需要一个存在 5 分钟、单次使用、
每分钟最多试 5 次的码。除此之外任何东西都要令牌。

### 配对码是给人读的

八位 Crockford base32，显示成 `K7QP-3M2X`。这个字母表没有 `I`、`L`、`O`、`U`：前三个是人从屏幕上
抄错的那三个，`U` 去掉是为了随机码不会拼出难听的字。大小写不敏感，规范化会把 `O`→`0`、`I`/`L`→`1`。

**单次使用是在发令牌之前就把码清掉**，所以两部手机抢同一个码只会有一个赢。过期码也会被清掉——
留着它会让"重新生成一个"之后旧码还能用。

### 令牌只存哈希

`~/.agi-mizar/devices.json`（`0600`）里只有 SHA-256。状态文件泄漏——备份、同步目录、别人拍的一张
目录截图——不等于泄漏访问权。明文令牌只在配对响应里出现一次。

吊销过的设备与从未存在的设备**回同一个错误**：否则一个被吊销的手机就能确认自己曾经是真的。

### 记录要翻译，不能转发

手机的对话界面解的是 DSH 的事件词表（`{type:"user/message", seq, time, data:{turn, content}}`）。
我们的共享模型更平，所以 `record_to_event()` 把它**拼回手机已经认识的那个形状**。这是刻意的：
否则每加一个引擎就要发一次手机版本，而手机不该知道 Codex 和 Claude Code 的存在。

工具调用与结果不翻译——手机的摘要视图本来就不画它们。

### 中继隧道：手机出了家门也能连

家用路由器不接受入站连接，所以电脑**拨出去**，中继把手机的请求顺着这条 socket 送回来：

```
phone ──https──▶ relay ──ws──▶ 这台电脑 ──http──▶ 127.0.0.1:19392
                   ▲                                    │
                   └────────────── res ─────────────────┘
```

隧道只搬四种活：一个 HTTP 请求、开一个 WebSocket、WebSocket 上的消息、关掉它。外加中继发来的
`ping`（半开的 socket 要被发现，而不是被相信）。

**为什么要真的重放一次 HTTP，而不是直接调 handler**：否则隧道和局域网两条路会走不同的代码，
以后每个 bug 都要修两遍。一次回环 HTTP 请求是几微秒，换来只有一条路径。

房间号由**中继**决定，电脑不自己发明。`host.info` 的 `relay: {url, room, baseURL}` 就是手机
出门后要用的地址；没配隧道时它是 `null`，手机读作"没有中继路线"。

### 这一版实现了什么

| 已实现 | 说明 |
|---|---|
| `host.info` | 含 `addresses` 与 `desktop:{os, client}` |
| `session.list` | 三个引擎的会话，`sessionId` 是 `engine:id` |
| `session.messages` | 从共享记录翻译成 DSH 事件形状；`context` 暂时 `null` |
| `session.prompt` / `session.cancel` | 直通引擎 |
| `interactions.pending` / `approval.answer` | 审批走内核的分发（它已经知道是哪个引擎问的） |
| `session.model.set` / `session.permission.set` | DSH 收 `provider/model`，另两个收裸模型名 |
| 事件 | `approval.request` / `approval.resolved` / `interactions.changed` |
| 中继隧道 | `welcome` / `req`→`res` / `ws-open` / `ws-msg` / `ws-close` / `ping`→`pong`；房间由中继分配 |

| 未实现（回 `NOT_IMPLEMENTED`） | 等什么 |
|---|---|
| `relay.set` | 在两个公共中继之间搬家（隧道本身已经能用） |
| `push.register`、`/push/v1/notify` | 通知策略：标题本地化、Live Activity、按设备排除 |
| `upload.*`、附件 prompt | 分片暂存与 content part 构造 |
| `file.stat` / `file.read` | 工作区围栏与预览缓存 |
| `workspace.list`、`session.activity`、`session.stats`、`session.create/rename/archive` | 各自的投影与注册表读 |
| `question.*` | 提问还没有进共享模型，`interactions.pending` 的 `questions` 恒为空 |

`NOT_IMPLEMENTED` 是协议自己的信号，意思是"这台主机比你旧"——比一个看起来成功的桩好得多。

## 10. 窗口：Tauri 只是壳

```rust
let booted = runtime.block_on(boot(config))?;   // 引擎、API、手机服务端、隧道
let url = booted.window_url();                  // http://127.0.0.1:<port>/?token=…
tauri::WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
```

### 窗口加载的是 URL，不是打包的前端资源

因为前端本来就通过回环 HTTP 跟内核说话，不走 Tauri IPC。加载
`http://127.0.0.1:<port>/?token=…` 意味着**同一份页面**在浏览器里开发、在窗口里运行——没有第二套
传输、没有第二个构建产物。M7 的 React 会从同一个路由发出来。

代价是窗口失去 `tauri://` 的 IPC，但那条路我们本来就不用。

### 启动序列只有一份

`agi-boot` 是 daemon 和窗口共用的。两个入口如果各写一遍启动顺序，就会出现"窗口里能复现、终端里
不能"的 bug，然后花一个下午。

顺序是有讲究的：

1. 先起引擎——内核才有东西可聚合；
2. 本地 API 绑定之后**才**写 endpoint 文件——插件拨一个没人应答的端口会退避重试，看起来像坏了；
3. 手机服务端单独绑、绑所有接口（手机要从网络进来）；
4. 隧道最后拨——它把请求回放到手机服务端，所以那个服务端必须先存在。

### 运行时是故意泄漏的

内核的服务器活在 Tokio 运行时上，而 `tauri::Builder::run` 会阻塞到窗口关闭。如果运行时由 `main`
的栈拥有，窗口一开它就被丢掉，所有服务器随之消失。`Box::leak` 是对"它和进程同寿"这件事最诚实的
表达。

### 打包

`cargo build -p agi-app` 只产出可执行文件。`.app` / `.dmg` / `.msi` 需要 Tauri 的 bundler，那在
`tauri-cli` 里（另一个安装）。`scripts/package.sh` 会先检查它，再编译 hook shim（安装器要往
`~/.claude/settings.json` 写它的绝对路径，所以它必须和 app 一起进包），然后调 `cargo tauri build`。

macOS 需要 Developer ID + 公证，Windows 需要代码签名证书——脚本末尾列了具体变量。

## 11. 前端：三栏、圆角、一个强调色

```
┌──────────────────────────────────────────────────────────┐
│ 工具栏 48：AGI Mizar │ 引擎分段控件 │ live │ Details      │
├──────────┬──────────────────────────────┬────────────────┤
│ 会话 268  │ 对话 + 输入框                 │ 详情 320（可收）│
│ 按时间倒序 │ 轮次分隔线 / 气泡 / 工具单行   │ 审批 / 字段     │
└──────────┴──────────────────────────────┴────────────────┘
```

### 为什么手写 CSS，不用组件库

成品主题库会把产品做成"又一个后台面板"。这里要的是**圆角组件**的克制感：三个半径（10/14/18）、
一条 1px 边线、一层几乎看不见的阴影，强调色只出现在选中行、发送按钮和运行中的圆点上。

具体的禁令：无渐变、无发光、无玻璃拟态、无大圆角卡片堆叠、无插画空态、无 emoji 图标。
深浅色跟随系统，用同一套变量。

### 记录怎么画

| kind | 画成 |
|---|---|
| `turn_start` | 一条带"Turn n"的细线——唯一的装饰，而且它承载信息 |
| `user_message` / `assistant_message` | 气泡，右/左对齐，圆角 14 |
| `tool_call` / `tool_result` | 单行虚线框，名字 + 参数首行，超长省略 |
| `turn_end` | 一行灰字 |

工具调用不展开：它是"跑过什么"的证据，不是要读的内容。

### 详情面板：过程流、用量、目录

会话列表回答"有哪些"，对话回答"说过什么"，剩下三件事在右侧：

| | 来源 | 为什么重要 |
|---|---|---|
| **过程流** | 记录缓存里最新一轮的 tool call/result | 没有它，一个安静了四分钟的会话和一个卡住的会话长得一样 |
| **用量** | `sessionStats` + `tokenUsage` | 46 步、302 秒模型、501 秒工具——这些数字决定要不要打断 |
| **交付文件** | `present` 工具调用 | 模型说"我做完了"时，做的是什么；点开就是文件内容 |
| **余额** | `account/getState` + `account/getBalance` | 跑一次任务花多少，值不值得再来一次 |

模型与权限是下拉框，目录来自 `session/modelCatalog`（45 个模型）与
`permissionPresets/catalog`（4 档）。新建、重命名、归档在同一个面板里——它们是同一类动作：
改变这台电脑上的会话，而不是改变对话。

### 文件预览的围栏

`read_file` 是**接口上的默认实现**，不是某个适配器的：三个产品都报一个工作目录，"这个文件在不在
会话目录里"对它们是完全一样的问题。

围栏做两次，顺序有讲究：

1. **词法**：先 `canonicalize` 根，再从根拼出候选路径，然后把 `..` 消掉比较前缀。这样"文件不存在"
   不会变成"围栏失效"——只在存在的文件上生效的围栏不是围栏。
2. **符号链接**：再 `canonicalize` 候选路径比较一次。目录里一个指向外面的链接会在这里被拦下。

（macOS 上 `/var` 是指向 `/private/var` 的链接，所以根必须先规范化再拼，否则同一个目录的两种写法
会让所有文件都被判成越界。）

### 数据流

页面只认 `/api/v1/state`、`/api/v1/command`、`/api/v1/events` 三个入口，token 从窗口 URL 里取。
选中会话时读一次 `messages`，之后靠 `record_appended` 增量追加；`session_updated` 重排列表，
`interaction_requested` 让审批立刻出现——一个被阻塞的 agent 每多等一秒都是真实的停滞。

### 产物怎么进窗口

`ui/dist` 由 `rust-embed` 在**编译期**嵌进 `agi-core`。没构建前端也能 `cargo build`：这时
`GET /` 回落到占位页，同一个 API、同一套数据。`ui/dist` 提交进仓库（和 `dsh/lib/` 一个道理），
所以克隆下来不需要 Node 就能编译。

## 12. 鉴权

四类凭据，各管一件事：

| 凭据 | 谁持有 | 放哪 | 能做什么 |
|---|---|---|---|
| plugin token | `dsh-mizar` 插件 | `~/.agi-mizar/endpoint.json`（`0600`） | 拨 `/dsh` |
| api token | 窗口 / CLI | 只打印到终端，或注入页面 | 读状态、发命令 |
| hook token | `agi-mizar-hook` | `~/.agi-mizar/hook.json`（`0600`） | 只有 `/hooks/claude/*` |
| device token | 手机 | 手机钥匙串 | 只走 `/mizar/v1/ws` |

分开的理由：地址文件任何同用户进程都能读，把 UI 的入口也放进去等于把 UI 的门钥匙放在门口垫子下。
同理，一个每次工具调用都会跑、而且被 Claude Code 调起来的 shim，不该拿着能驱动窗口的令牌；一台
经过网络才能到达的手机，更不该。

设备的令牌在主机侧只存 SHA-256，见 §9。

API 只绑 `127.0.0.1`，绑定通配地址在代码构造上就不可能（`TcpListener::bind(("127.0.0.1", 0))`）。
WS 的 token 走查询参数——浏览器无法给 WebSocket 设 header。

---

## 13. 测试策略

| 层 | 怎么测 | 为什么可信 |
|---|---|---|
| 协议 | 逐帧 JSON 往返 | 与插件侧 `frames.ts` 的形状是同一份文档 |
| 投影 | 纯函数，真实载荷形状 | 0.8.x 踩过的坑（注入上下文、`message.content` 少一层）都成了用例 |
| 链路（DSH） | 两个 channel 模拟插件 | 握手、错令牌、列表、事件、follow+page、掉线撤卡，全是真的代码路径 |
| 链路（Codex） | 真子进程 + Node 假 app-server | 握手、thread 列表、历史、prompt、**审批应答的取值**、模型与权限设置 |
| 链路（Claude） | 临时 transcript 目录 + 直接调 hook 入口 | 列表、历史、轮次边界、`isMeta`/sidechain/包装、审批允许与拒绝、无人在场不阻塞 |
| 安装器 | 临时 settings.json | 幂等、保留别人的 hook、卸载只删自己的、坏 JSON 不覆盖 |
| Mizar 服务端 | 真 HTTP + 真 WebSocket（无 mock） | 配对码单次、错误码、welcome 无 `id`、未握手、吊销、未知方法 |
| 中继隧道 | 真 WS 两头 + 真本地监听器 | `welcome` 的 room、GET/POST 重放（含请求体）、`ping`→`pong` |
| 窗口 | 手动启动 + `curl` 它加载的地址 | 内核起来、页面可取、`/api/v1/state` 返回真实会话 |
| 前端 | `tsc -b` 严格模式 + 构建 | 类型与内核契约逐字段对应；构建产物由内核嵌入并可取回 |
| 内核 | 端到端跑一次 | 起守护进程 → 假插件连入 → `curl` 查 state / messages |

没有用 mock 替代 WebSocket 来测链路：那样测的是 mock。

---

## 14. 还没做的（以及为什么现在不做）

| 项 | 等什么 |
|---|---|
| 推送、上传 | 见 §9 的未实现表；两者都只影响手机，不影响桌面 |
| 提问进共享模型 | 现在 `Interaction` 只带标题与详情，`questions` 恒为空 |
| Claude 的提问工具（`AskUserQuestion`） | 目前只有审批走 hook；提问要借"拒绝的理由"回传答案，属独立一项 |

| 持久化 | 等搜索 / 设置 / 交付文件索引 |
| Mizar 服务端（配对、中继、推送） | M5；手机协议与黄金帧已经在 `protocol/` 里 |
| 附件、分支、归档、文件预览 | DSH 侧是 `invoke` 直通；接线时把 `capabilities` 打开 |

| 安装包 | 需要 `tauri-cli` 与签名证书；脚本已备 |
