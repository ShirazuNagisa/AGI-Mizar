# AGI Mizar

One desktop application that hosts three coding agents — **DSH**, **Claude Code** and **Codex** —
and lets a phone drive all of them through one connection.

This repository currently contains the **backend kernel**. There is no window yet: the kernel runs,
serves its API, accepts the DSH plugin, drives a Codex app-server, and reads and answers Claude Code
through transcripts and hooks, and serves the shipped phone app over Mizar v1 — pairing, device
tokens, and the conversation methods. The React front end and the Tauri shell come next.

```
手机 App ──▶ AGI Mizar ──┬──▶ DSH         （helper plugin 拨回环 WS 进来）
                          ├──▶ Claude Code （hooks + claude -p）
                          └──▶ Codex       （codex app-server）
```

## Status

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M0 | 仓库、workspace、契约 | ✅ |
| M1 | 内核：引擎抽象、DSH 链路、状态聚合、本地 API、守护进程 | ✅ |
| M2 | Codex 引擎（app-server JSON-RPC、审批、历史投影） | ✅ |
| M3 | Claude Code 引擎（transcript 解析、hooks、`claude -p`） | ✅ |
| M4 | `dsh-mizar` 0.9.0 联调 | 🔶 插件侧已完成，等真机联调 |
| M5 | Mizar 服务端：**局域网配对 + 核心方法 + 中继隧道** ✅；推送 / 上传 / 文件预览 ⬜ |
| M6 | Tauri 壳 ✅；打包签名脚本已备，需装 `tauri-cli` |
| M7 | 前端 | ⬜ |

## 仓库结构

```
crates/
├─ agi-protocol/     共享类型：领域模型、DSH Link v1 帧、本地 API 契约
├─ agi-engine/       Engine trait、引擎事件、错误
├─ agi-engine-dsh/   DSH 适配：链路服务端 + 原始事件投影
├─ agi-engine-codex/ Codex 适配：app-server JSON-RPC + 轮次投影
├─ agi-engine-claude/Claude Code 适配：transcript 解析 + hooks + `claude -p`
├─ agi-hook-shim/    Claude Code hooks 调用的无 GUI 小二进制
├─ agi-mizar/        手机侧服务端：配对、设备、Mizar v1 WebSocket
├─ agi-core/         内核：状态聚合、本地 API、端点文件
└─ agi-daemon/       无窗口运行内核（开发 / 诊断 / 无头部署）
docs/                架构与协议
protocol/            Mizar v1 黄金帧（手机协议，未来服务端用）
```

## 跑起来

```sh
cargo test                     # 100 个测试
cargo run -p agi-daemon        # 内核，无窗口
cargo run -p agi-app           # 窗口（会打印它加载的地址）
```

窗口加载的是内核自己的页面 `http://127.0.0.1:<port>/?token=…`，不是 Tauri 打包的前端资源——
所以同一份页面在浏览器里开发、在窗口里运行，只有一条路。现在的页面是占位，M7 换成 React。

内核启动后会打印：

```
AGI Mizar kernel 0.1.0
  api    http://127.0.0.1:64222/api/v1/state
  events ws://127.0.0.1:64222/api/v1/events?token=…
  dsh    ws://127.0.0.1:64222/dsh
  file   ~/.agi-mizar/endpoint.json
  token  …
```

`~/.agi-mizar/endpoint.json` 是 `dsh-mizar` 0.9.0 读取的地址文件。桌面端（或这个守护进程）
一启动，装了 0.9.0 插件的 DSH 就会连上来。

```sh
TOKEN=…                        # 上面打印的那个
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:64222/api/v1/state
curl -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"op":"messages","session":"dsh:<id>","limit":50}' \
     http://127.0.0.1:64222/api/v1/command
```

## 设计上的三条硬规则

1. **前端不走 Tauri IPC。** 内核是一个守护进程，本地 API 是 HTTP + WebSocket。前端可以在浏览器里
   跑，也可以被 CLI 用，Tauri 只负责开窗和把端口/令牌交给页面。
2. **DSH 插件拨进来，不是我们拨出去。** 桌面端监听 `/dsh`，插件是客户端。桌面端不开任何对外端口，
   插件也不开。
3. **引擎只暴露人能做的事。** 读会话、读对话、发消息、打断、应答、切模型、切权限。产品的其余
   能力走各自的逃生通道，不往 trait 上加方法。

细节见 [`docs/architecture.md`](docs/architecture.md)。

## 打包

```sh
cargo install tauri-cli --version '^2' --locked   # 一次性
scripts/package.sh            # 当前平台
scripts/package.sh --universal # macOS：arm64 + x86_64
```

`cargo build -p agi-app` 只产出可执行文件，不产出 `.app` / `.dmg` / `.msi`——那些要 CLI。
签名与公证的环境变量见脚本末尾的说明。

## 许可

GPL-3.0-only。见 [LICENSE](LICENSE)。
