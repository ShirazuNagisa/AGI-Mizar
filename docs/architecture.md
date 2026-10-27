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

## 8. 鉴权

两个令牌，不是一个：

| 令牌 | 谁持有 | 放哪 |
|---|---|---|
| plugin token | `dsh-mizar` 插件 | `~/.agi-mizar/endpoint.json`（`0600`） |
| api token | 窗口 / CLI | 只打印到终端，或注入页面 |

分开的理由：地址文件任何同用户进程都能读，把 UI 的入口也放进去等于把 UI 的门钥匙放在门口垫子下。

API 只绑 `127.0.0.1`，绑定通配地址在代码构造上就不可能（`TcpListener::bind(("127.0.0.1", 0))`）。
WS 的 token 走查询参数——浏览器无法给 WebSocket 设 header。

---

## 9. 测试策略

| 层 | 怎么测 | 为什么可信 |
|---|---|---|
| 协议 | 逐帧 JSON 往返 | 与插件侧 `frames.ts` 的形状是同一份文档 |
| 投影 | 纯函数，真实载荷形状 | 0.8.x 踩过的坑（注入上下文、`message.content` 少一层）都成了用例 |
| 链路（DSH） | 两个 channel 模拟插件 | 握手、错令牌、列表、事件、follow+page、掉线撤卡，全是真的代码路径 |
| 链路（Codex） | 真子进程 + Node 假 app-server | 握手、thread 列表、历史、prompt、**审批应答的取值**、模型与权限设置 |
| 内核 | 端到端跑一次 | 起守护进程 → 假插件连入 → `curl` 查 state / messages |

没有用 mock 替代 WebSocket 来测链路：那样测的是 mock。

---

## 10. 还没做的（以及为什么现在不做）

| 项 | 等什么 |
|---|---|
| Claude Code 适配 | M3；接口已经定死，加的是适配层 |
| 持久化 | 等搜索 / 设置 / 交付文件索引 |
| Mizar 服务端（配对、中继、推送） | M5；手机协议与黄金帧已经在 `protocol/` 里 |
| 附件、分支、归档、文件预览 | DSH 侧是 `invoke` 直通；接线时把 `capabilities` 打开 |
| 前端 | M7；API 已经可以对着真内核开发 |
