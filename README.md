# AGI Mizar

One desktop application that hosts three coding agents — **DSH**, **Claude Code** and **Codex** —
and lets a phone drive all of them through one connection.

This repository currently contains the **backend kernel**. There is no window yet: the kernel runs,
serves its API, accepts the DSH plugin, and is tested end to end. The React front end and the Tauri
shell come next.

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
| M2 | Codex 引擎 | ⬜ |
| M3 | Claude Code 引擎 | ⬜ |
| M4 | `dsh-mizar` 0.9.0 联调 | 🔶 插件侧已完成，等真机联调 |
| M5 | Mizar 服务端（配对 / 中继 / 推送） | ⬜ |
| M6 | Tauri 壳、打包签名 | ⬜ |
| M7 | 前端 | ⬜ |

## 仓库结构

```
crates/
├─ agi-protocol/     共享类型：领域模型、DSH Link v1 帧、本地 API 契约
├─ agi-engine/       Engine trait、引擎事件、错误
├─ agi-engine-dsh/   DSH 适配：链路服务端 + 原始事件投影
├─ agi-core/         内核：状态聚合、本地 API、端点文件
└─ agi-daemon/       无窗口运行内核（开发 / 诊断 / 无头部署）
docs/                架构与协议
protocol/            Mizar v1 黄金帧（手机协议，未来服务端用）
```

## 跑起来

```sh
cargo test                     # 29 个测试
cargo run -p agi-daemon        # 内核，无窗口
```

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

## 许可

GPL-3.0-only。见 [LICENSE](LICENSE)。
