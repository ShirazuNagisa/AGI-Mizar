# 接入说明：Mizarllm v1 → AGI Mizar 内核

> 本文只讲**怎么跑起来**。判据、四态的含义、训练过程见 `docs/mizarllm-v1-2026-10-28.md`。

## 1. 它做了什么

`turn/end` 到达时，内核判断这一轮属于四态之一，结果写进 `layouts` 并打一条 debug 日志。

```
turn/end
  ├─ reason = error / aborted          → stalled   （闸门，不问模型）
  ├─ 还有子代理 / 后台任务在跑          → waiting   （闸门，不问模型）
  └─ 结构已静止                        → 问模型：done / needs_user / stalled
```

**当前是影子模式**：结果只落内存和日志，不推给任何客户端。手机端行为一个字都没变。

## 2. 三步跑起来

### 2.1 GGUF 已经导出好了

```bash
# 仓库根目录
ls models/mizarllm-v1-q4km.gguf     # 1.0 GB
cat models/SHA256SUMS
```

导出管线（已经跑过，需要重导时用）：

```bash
cd crates/agi-layout/tools
python export_gguf.py --adapter ../../../../tools/replay/out/mizarllm-v2 --out ./mizarllm-v1
llama-quantize mizarllm-v1-f16.gguf mizarllm-v1-q4km.gguf Q4_K_M
```

**已验证**：把三条真实 holdout 样本（gold = done / needs_user / stalled）喂给量化后的 GGUF，
输出分别是 `{"state":"done"}` / `{"state":"needs_user"}` / `{"state":"stalled"}` —— 三条全对，
说明合并、转换、量化都没有破坏模型。

### 2.2 起 sidecar

模型常驻，单次判定是一次 HTTP 往返（几十毫秒），不是每次加载模型。

```bash
llama-server -m mizarllm-v1-q4.gguf --port 8791 --ctx-size 4096 -t 6
```

### 2.3 让内核连上

```bash
export MIZAR_LAYOUT_URL=http://127.0.0.1:8791
# 需要鉴权时再加
# export MIZAR_LAYOUT_KEY=...

cargo run -p agi-mizar
```

没有 `MIZAR_LAYOUT_URL` 时，内核行为和以前完全一致（`layout` 字段为 `None`，一次模型都不会调）。

## 3. 怎么确认它在工作

日志里出现：

```
layout judged  session=... turn=7 state=done source=Model elapsed_ms=48
```

- `source=Gate`：结构层直接定的，没问模型（正常，且是好事）
- `source=Model`：模型判的
- `source=Rule`：模型不可用 / 输出解析不了，退回规则 —— **这个比例就是模型的健康度**

## 4. 代码结构

| 位置 | 职责 |
|---|---|
| `crates/agi-layout/src/lib.rs` | 纯逻辑：`State` / `gate` / `render_prompt` / `parse_state` / `Judge` 三种实现 / `LayoutService` |
| `crates/agi-core/src/lib.rs` 尾部 `layout glue` | 把 `Record` 映射成 `Item`，拼 `Facts`，`turn/end` 时异步触发 |
| `tests/parity.rs` | **Rust 渲染的 prompt 必须与训练管线逐字节一致** |

那条 parity 测试是这套东西的安全带：以后任何一边改了模板（JS 或 Rust），测试会立刻红。改模板必须同时重训模型，不存在"只改一边"的合法情况。

## 5. 已知的保真度损失

| 项 | 现状 | 影响 |
|---|---|---|
| `ToolResult` 没有错误标志 | 用输出前缀猜（`Error` / `failed` / `exit code 1`） | 失败调用的统计会和训练数据略有出入 |
| 三个引擎的 `turn/end` 文本不统一 | `reason_of()` 归一到 completed/error/aborted | DSH 写 provider reason、Codex 写自身状态、Claude 不写 |
| 模型不输出置信度 | 只有 `{"state":...}` | 没法做阈值回退，`source=Rule` 是唯一降级路径 |

## 6. 从影子模式到上线

影子跑一周，用日志回答一个问题：**模型说 `done`、用户却在同一分钟内又发了消息的轮次有多少？** 这个比例就是真实误报率，比 holdout 上的 87.8% 更有说服力。

达到可接受水平后，再做两处改动（都不影响老客户端）：

1. `ApiEvent::RecordAppended` 里给 `TurnEnd` 的 `record` 附上 `layout` 字段；
2. 手机端读 `layout.v2` 能力位，读不到就回落现有规则路径。

**手机端的规则路径永远不删。**

---

# 第二部分：模型接缝与会话状态

## 7. 内核不认识 Mizarllm，只认识 manifest

`models/*.json` 描述一个模型：在哪、说什么方言、以及**它的标签对应哪个状态码**。

```json
{
  "id": "mizarllm-v1",
  "version": "1.0.0",
  "backend": { "kind": "http", "endpoint": "http://127.0.0.1:8791", "timeout_ms": 1500 },
  "labels": { "done": 9, "needs_user": 7, "stalled": 10, "waiting": 5 }
}
```

| 字段 | 作用 |
|---|---|
| `backend` | `http` 指向常驻 sidecar；`rule` 表示纯规则（没有模型也能跑） |
| `labels` | 模型的词 → 状态码。**换模型只改这里，内核二进制不动** |
| `version` | 进日志，用来回答"手机看到的那条状态是哪个模型判的" |

启动顺序：

```
MIZAR_MODEL=<id>          指定激活哪个
   ↓ 没指定
目录里只有一个 manifest    自动激活它
   ↓ 没有 manifest
MIZAR_LAYOUT_URL          开发时的临时逃生口
   ↓ 都没有
不判，内核行为与以前完全一致
```

运行中换模型：`Core::activate_model("mizarllm-v2")` —— 不用重启、不用重新编译、手机端零改动。
新模型多出一个标签？在 manifest 里指向已有的状态码即可；指向这个 build 不认识的状态码，manifest 会被**拒绝**而不是画错。

## 8. 十二状态：码走线，文案留在两端

唯一来源是 `protocol/states.json`。改完跑：

```bash
node tools/gen-states.mjs          # 生成
node tools/gen-states.mjs --check  # CI 用：生成物过期就红
```

生成物三份，**不要手改**：

| 文件 | 使用者 |
|---|---|
| `AGI Mizar/crates/agi-protocol/src/session_state.rs` | 桌面内核 |
| `iOS/MizarKit/Sources/MizarKit/SessionState.swift` | 手机 |
| `iOS/protocol/fixtures/session-state.json` | 两侧测试共用的对照表 |

线上的形态是一个字节：

```json
{ "st": 7, "at": 1761728400000, "turn": 12 }
```

手机 `SessionState(code: 7)` → 「等你回答」。未知码 → `.unknown`，不崩、不误显示。
正文、总结消息这类真文本照旧原样传——**只压缩"状态"这类枚举，绝不压缩内容**。

## 9. 状态是怎么定出来的

两层，结构先说话：

| 事实 | 状态 |
|---|---|
| `turn/end` 原因 = error | `failed` (11) |
| `turn/end` 原因 = aborted / interrupted | `paused` (8) |
| 还有子代理在跑 | `subagent` (4) |
| 还有后台任务在跑 | `background` (5) |
| 以上都不是 | 问模型 → `completed` (9) / `awaiting_user` (7) / `stalled` (10) |

运行中的状态（`thinking` / `writing` / `tool_running` / `idle`）来自记录流本身，不经过模型——
模型只在一轮结束时被问一次。

## 10. 现在到哪了

| 步骤 | 状态 |
|---|---|
| 十二状态表、两侧枚举、共享 fixture | ✅ 完成，两侧测试全绿 |
| manifest 驱动的模型接缝 + 运行中换模型 | ✅ 完成 |
| 内核按记录流推导状态并存下来 | ✅ 完成（`Core::session_state`） |
| 状态码进 `RecordAppended` 帧 | ⬜ 未做 |
| MizarKit v1 传输层带上 `st` 字段 | ⬜ 未做 |
| iOS 端渲染 | ⬜ 未做（枚举与解码已就位） |
