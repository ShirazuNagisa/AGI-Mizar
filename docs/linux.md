# Linux：浏览器图形界面

Linux 版不单独维护一套前端。`agi-daemon` 内嵌的 React 应用就是 macOS / Windows 窗口里
那一套（同一份 `ui/dist`，同样的侧栏、首页、会话、审批、设置页），Linux 只是把它注册成
一个**本机 Web 应用**：

- `systemd --user` 服务在登录时启动 kernel，退出后自动拉起；
- 桌面菜单里出现 **AGI Mizar**，点开用默认浏览器打开 Web UI；
- `agi-mizar-web` 命令负责 `open / url / status / logs`，无头机器也能用；
- 浏览器用 `/launch` 一次性 token 换 **HttpOnly、SameSite=Strict** 的 cookie，之后地址栏、
  书签和 WebSocket 都不带 token。

它不安装 relay，不改 nginx，也不碰系统级服务；API 默认只监听 `127.0.0.1`。

## 公测包（beta）

`scripts/package-linux.sh` 会产出两个文件（同目录附 `SHA256SUMS` 和 `RELEASE-NOTES.md`）：

- `agi-mizar-<version>-beta.<n>-linux-x86_64.tar.gz`：解压后运行
  `./scripts/linux/install-webui.sh`，安装到当前用户；
- `agi-mizar_<version>~beta.<n>_amd64.deb`：`sudo apt install ./agi-mizar_*.deb`，
  装到 `/usr/lib/agi-mizar`，并注册 `/usr/bin/agi-mizar-web`、桌面菜单和
  `/usr/lib/systemd/user/agi-mizar-web.service`。

`.deb` 的 systemd 用户服务不会在系统级 `postinst` 里启动；**第一次点桌面菜单或运行
`agi-mizar-web open`** 时，helper 会创建 `~/.config/agi-mizar/web.env`（0600，生成稳定
token）、执行 `systemctl --user daemon-reload`、启用并启动服务，然后打开浏览器。便携包
的 `install-webui.sh` 则会直接完成同样的注册。

公测包未签名；运行前先 `sha256sum -c SHA256SUMS`。DSH 与 `dsh-mizar` 插件不打进包里，
按下方依赖单独安装。

## 依赖

- Linux + systemd（用户实例；`systemctl --user` 可用）
- `curl`、`xdg-open`（或 `gio open`）
- DSH CLI 和 `dsh-mizar` 插件（DSH 引擎需要；没有时 Web UI 能打开，但 DSH 显示未连接）
- 构建时需要 Rust 与 Node/npm

## 构建并安装

```sh
cd "AGI Mizar"
npm --prefix ui ci
npm --prefix ui run build
cargo build --release -p agi-daemon

scripts/linux/install-webui.sh --build
```

`--build` 会依次构建 `ui/dist`（必须在前，daemon 用 `rust-embed` 把它编译进二进制）和
`agi-daemon`；也可以先手动构建，再传 `--binary target/release/agi-daemon`。

公测包构建与冒烟测试：

```sh
scripts/package-linux.sh --version 0.1.2 --beta 1 --out ../../release/linux-beta-0.1.2
scripts/linux/smoke-test.sh --binary target/release/agi-daemon
```

冒烟测试不需要 systemd 或 root：临时 HOME/端口启动 kernel，检查 UI 页面、无认证 401、
`/launch` 的 HttpOnly/SameSite cookie、cookie 访问 200、开放重定向被挡、logout 204。

默认安装位置：

| 内容 | 路径 |
|---|---|
| kernel | `~/.local/lib/agi-mizar/bin/agi-daemon` |
| 控制命令 | `~/.local/bin/agi-mizar-web`（同时保留在安装目录） |
| 配置（0600） | `${XDG_CONFIG_HOME:-~/.config}/agi-mizar/web.env` |
| systemd user unit | `${XDG_CONFIG_HOME:-~/.config}/systemd/user/agi-mizar-web.service` |
| 桌面菜单项 | `${XDG_DATA_HOME:-~/.local/share}/applications/agi-mizar-web.desktop` |
| 数据 | `~/.agi-mizar`（不动；重装保留） |

安装完成后会自动 `systemctl --user enable --now agi-mizar-web.service`。如果当前 shell
没有 user bus（常见于 SSH 无 linger），脚本会打印需要手动执行的命令。

## 日常使用

```sh
agi-mizar-web open      # 打开浏览器；首次运行会创建配置并启动服务
agi-mizar-web url       # 打印带一次性 token 的 launch URL
agi-mizar-web status    # 服务 + /healthz
agi-mizar-web logs      # journalctl --user
agi-mizar-web restart
```

桌面菜单里的 **AGI Mizar** 等价于 `agi-mizar-web open`。

## 选项

```sh
scripts/linux/install-webui.sh --port 64222
scripts/linux/install-webui.sh --host 127.0.0.1     # 默认
scripts/linux/install-webui.sh --no-desktop         # 服务器
scripts/linux/install-webui.sh --no-start
scripts/linux/install-webui.sh --linger             # 开机即起，不等登录
scripts/linux/install-webui.sh --uninstall          # 保留数据与配置
```

- `--host 0.0.0.0` 会把带 token 的 API 暴露到网络；只有在你确实要用另一台机器直连时才用，
  更推荐 SSH 隧道。
- `--uninstall` 删除 unit / 桌面项 / 二进制，保留 `~/.agi-mizar` 和 `web.env`。

## 无头服务器

```sh
scripts/linux/install-webui.sh --no-desktop --linger
# 从自己的电脑：
ssh -N -L 64222:127.0.0.1:64222 user@server
agi-mizar-web url        # 在服务器上跑，拿到 URL；把 127.0.0.1:64222 直接粘到浏览器
```

如果 `--host` 不是回环地址，安装器会提示你确认暴露范围；token 仍然需要。

## 认证流程

1. `agi-mizar-web open` 读取 `web.env`，拼出
   `http://127.0.0.1:<port>/launch?token=<token>`；
2. `GET /launch` 用常量时间比较 token，成功后 `303` 到 `/`，并设置
   `agi_mizar_session=<token>; HttpOnly; SameSite=Strict; Path=/; Max-Age=30d`；
3. 页面和 `/api/v1/*`、`/api/v1/events` 都接受这个 cookie；`POST /api/v1/logout` 清掉它；
4. 直接跑开发前端或 Tauri 窗口时，旧的 `Authorization: Bearer` / `?token=` 仍然可用。

token 仍在 `web.env` 和 `agi-mizar-web url` 的输出里，所以该文件是 0600；不要把它贴到
聊天/工单。`/launch` URL 只应在打开浏览器时短暂出现。

## 与 macOS / Windows 的布局一致性

`ui/src` 是同一套代码；`ui/index.html` 在 Linux 上识别 `platform=linux`，保留系统标题栏，
不做 macOS 红绿灯/Windows 自绘按钮的留白。侧栏、首页统计、会话、审批、设置页均与桌面端
一致；Linux 只是没有 Tauri 外壳，所以没有托盘和原生窗口菜单。

## 故障排查

| 现象 | 处理 |
|---|---|
| 菜单里没有 AGI Mizar | 确认 `~/.local/share/applications/agi-mizar-web.desktop` 存在；注销重登或 `update-desktop-database` |
| 打开是空白/401 | 检查 `agi-mizar-web status`，并用 `agi-mizar-web url` 而不是手输裸地址 |
| DSH 显示未连接 | 安装 DSH 和 `dsh-mizar` 插件（plug 进 `web` profile），见工作区 headless 部署文档 |
| 端口冲突 | `--port 64223` 重装；安装器会保留 token |
| 服务起不来 | `agi-mizar-web logs`；看 `AGI_API_PORT`/`AGI_API_TOKEN` 是否被 `web.env` 覆盖 |
| 无头机器登录后服务停 | `loginctl enable-linger $USER` 或 `--linger` |
