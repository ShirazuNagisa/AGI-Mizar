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

## 7. 鉴权

两个令牌，不是一个：

| 令牌 | 谁持有 | 放哪 |
|---|---|---|
| plugin token | `dsh-mizar` 插件 | `~/.agi-mizar/endpoint.json`（`0600`） |
| api token | 窗口 / CLI | 只打印到终端，或注入页面 |

分开的理由：地址文件任何同用户进程都能读，把 UI 的入口也放进去等于把 UI 的门钥匙放在门口垫子下。

API 只绑 `127.0.0.1`，绑定通配地址在代码构造上就不可能（`TcpListener::bind(("127.0.0.1", 0))`）。
WS 的 token 走查询参数——浏览器无法给 WebSocket 设 header。

---

## 8. 测试策略

| 层 | 怎么测 | 为什么可信 |
|---|---|---|
| 协议 | 逐帧 JSON 往返 | 与插件侧 `frames.ts` 的形状是同一份文档 |
| 投影 | 纯函数，真实载荷形状 | 0.8.x 踩过的坑（注入上下文、`message.content` 少一层）都成了用例 |
| 链路 | 两个 channel 模拟插件 | 握手、错令牌、列表、事件、follow+page、掉线撤卡，全是真的代码路径 |
| 内核 | 端到端跑一次 | 起守护进程 → 假插件连入 → `curl` 查 state / messages |

没有用 mock 替代 WebSocket 来测链路：那样测的是 mock。

---

## 9. 还没做的（以及为什么现在不做）

| 项 | 等什么 |
|---|---|
| Codex / Claude Code 适配 | M2 / M3；接口已经定死，加的是适配层 |
| 持久化 | 等搜索 / 设置 / 交付文件索引 |
| Mizar 服务端（配对、中继、推送） | M5；手机协议与黄金帧已经在 `protocol/` 里 |
| 附件、分支、归档、文件预览 | DSH 侧是 `invoke` 直通；接线时把 `capabilities` 打开 |
| 前端 | M7；API 已经可以对着真内核开发 |
