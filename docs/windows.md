# Windows 构建与打包

本文是 `scripts/package.sh`（macOS）的 Windows 对等说明：Windows 上怎么准备资源、怎么打包、
哪些是 Windows 特有的坑，以及哪些能力**没有**做（签名、MSI 的实机验证）。

结论先说：Windows 用 `scripts/package.ps1` 产 NSIS 安装包，效果对齐 `package.sh` 的
hook shim + `llama.cpp` 运行时 + 安装包三件事；安装包默认**未签名**，首次运行会被 SmartScreen
拦一次；MSI 需要额外装 WiX 工具集，不是默认产物。

本文的 Windows 部分在 Windows 11 实机、`rustc 1.97.1`、Node 24、`@tauri-apps/cli 2.12.4`
（Tauri crate 2.12.5）上核对过；完整 `tauri build` 由打包验证那一步统一跑，本文只记录脚本级验证。

---

## 1. 前置

| 依赖 | 版本 / 要求 | 用途 |
|---|---|---|
| Windows | 10 1903+ 或 11，x64 | 只支持 x64；ARM64 Windows 没有产物 |
| WebView2 Runtime | Windows 11 自带；Windows 10 可能没有 | 安装包配置是 `downloadBootstrapper`，安装时联网补 |
| Visual Studio 2022 Build Tools | 勾「使用 C++ 的桌面开发」 | MSVC 链接器 + Windows SDK；签名时 `signtool` 也在 SDK 里 |
| Rust | stable（workspace `rust-version = 1.82`） | `rustup target` 不需要额外 target，默认就是 `x86_64-pc-windows-msvc` |
| Node.js | 20+ | Tauri CLI 走 `npx @tauri-apps/cli@2`，**不是** `cargo tauri` |
| PowerShell | 7 推荐，Windows 10 自带 5.1 也能跑 | 两个 `.ps1` 脚本 |

macOS 那边用 `cargo install tauri-cli`；Windows 干净的机器上没有 `cargo tauri` 这个子命令，
脚本统一用 `npx --yes @tauri-apps/cli@2 build ...`，版本由 npm 解析（本机缓存 2.12.4）。

---

## 2. 第一次构建：资源必须先存在

`crates/agi-app/tauri.windows.conf.json` 在 `bundle.resources` 里声明了两个路径：

```json
"resources": {
  "resources/agi-mizar-hook.exe": "agi-mizar-hook.exe",
  "resources/bin": "bin"
}
```

它们相对 `crates/agi-app/`，也就是 `crates/agi-app/resources/`。这个目录**不入库**（已 gitignore），
但 `tauri-build` 在编译 `agi-app` 的 build script 阶段就会遍历并**拷贝** `bundle.resources`
到 `target/<profile>/`（给运行时用），源路径不存在时直接让编译失败：

```
resource path `resources\agi-mizar-hook.exe` doesn't exist
resource path `resources\bin` doesn't exist
```

也就是说：**Windows 上不只是打包，连 `cargo build -p agi-app`、`cargo test --all`、`cargo clippy`
都需要这两个路径先存在**。三种准备方式，按用途选：

### A. 要真的跑模型 / 出安装包（推荐）

```powershell
scripts\fetch-llama.ps1        # 下载固定 bNNNNN 的 CPU x64 运行时到 crates/agi-app/resources/bin
cargo build -p agi-hook-shim   # 编译 hook shim
Copy-Item target\debug\agi-mizar-hook.exe crates\agi-app\resources\ -Force
cargo run -p agi-app
```

或者一步到位：`scripts\package.ps1 -FetchLlama`（它会做上面全部 + 出安装包，见 §4）。

### B. 只想编译 / 跑测试，不跑模型（CI 用这个）

```powershell
scripts\fetch-llama.ps1 -Stub
```

`-Stub` 不联网，只建 `crates/agi-app/resources/bin/` 和一个 0 字节的
`crates/agi-app/resources/agi-mizar-hook.exe` 占位。`cargo clippy` / `cargo test` 能通过；
**不要**用这棵树出安装包（安装包里会没有能跑的 llama.cpp）。

### C. 离线 / GitHub 慢

```powershell
scripts\fetch-llama.ps1 -Archive D:\downloads\llama-b12000-bin-win-cpu-x64.zip
scripts\fetch-llama.ps1 -Proxy https://gh-proxy.com/    # 走前缀镜像下载
```

`fetch-llama.ps1` 会：

1. 从 `scripts/llama-version.txt` 读固定的 tag（当前 `b12000`，和 macOS 共用同一个文件）；
2. 下载 `llama-<tag>-bin-win-cpu-x64.zip`，或复用 `-Archive` / `target\llama-cache\` 里的缓存；
3. 按脚本内置的 SHA256 表校验（`b12000` = `6fb512a7...d6e067220`，与 GitHub release 的
   `*.zip.sha256` 一致）；tag 不在表里会直接报错，要求先核对再补表；
4. **解压整个目录**到 `crates/agi-app/resources/bin/`——Windows 版 `llama-server.exe` 只是一个
   9 KB 的 loader，`llama-server-impl.dll` / `llama.dll` / `ggml-*.dll` 必须和它同目录，
   只拷 exe 会得到一个起不来的 sidecar；
5. 打印文件数、总大小，并跑一次 `llama-server.exe --version`。

固定 tag 的原因见 `scripts/fetch-llama.sh` 的注释：llama.cpp 的二进制挂在滚动的 `bNNNNN` tag 上，
`/releases/latest` 没有资产。CPU x64 是默认产物；本机 `.zwork/win-verify/llama/` 里也有 CUDA
版本，但默认不打进包（体积、驱动依赖）。**b12000 的 CPU zip 约 24 MB，安装后是 52 个文件 / 约
60.5 MiB**（`llama-server.exe` 只有 9 KB，其余是 DLL）。

---

## 3. 构建、运行、测试

```powershell
cargo test --all                                  # 全 workspace
cargo clippy --all-targets -- -D warnings
cargo run -p agi-daemon                           # 无窗口内核
cargo run -p agi-app                              # 窗口
cd ui; npm ci; npm run build                      # 改前端之后：ui/dist 在 Rust 编译期被嵌入
```

注意 `ui/dist` 是提交进仓库的，只有改前端才需要 Node。

---

## 4. 打包：`scripts/package.ps1`

```powershell
scripts\package.ps1 -FetchLlama              # 默认 NSIS 安装包
scripts\package.ps1 -Bundles nsis,msi        # 额外出 MSI（需要 WiX，见 §6）
scripts\package.ps1 -Archive D:\downloads\llama-b12000-bin-win-cpu-x64.zip
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content .\updater.key -Raw
scripts\package.ps1 -WithUpdater             # 要求 key 存在，产出 .sig + latest.json
```

| 参数 | 含义 |
|---|---|
| `-Bundles` | `nsis`（默认）/ `msi` / `nsis,msi`；其他值报错 |
| `-FetchLlama` | 打包前调用 `fetch-llama.ps1`（固定版本 + SHA256） |
| `-Archive` / `-Proxy` / `-Force` | 透传给 `fetch-llama.ps1` |
| `-WithUpdater` | 要求 `TAURI_SIGNING_PRIVATE_KEY` 或 `TAURI_SIGNING_PRIVATE_KEY_PATH`；没有就失败 |

### 与 `package.sh` 的逐步对应

| `package.sh` | `package.ps1` 对应动作 |
|---|---|
| 检查 `cargo tauri` 是否安装 | 检查 `cargo` + `npx`（Windows 上 Tauri CLI 走 npm） |
| `cargo build --release -p agi-hook-shim` | 同 |
| `cp target/release/agi-mizar-hook → resources/agi-mizar-hook` | `Copy-Item target\release\agi-mizar-hook.exe → resources\agi-mizar-hook.exe`（Windows 用 `.exe` 名字） |
| 检查 `APPLE_SIGNING_IDENTITY` / Apple ID | 无关，Windows 不适用；改为提示 Authenticode 证书未配置 |
| 检查 `TAURI_SIGNING_PRIVATE_KEY` | 同；没有 key 时**不是**只提示——见下面「updater」 |
| 检查 `resources/bin/llama-server` | 检查 `resources/bin\llama-server.exe`；目录缺失直接报错（编译会失败） |
| `cargo tauri build --config '{"bundle":{"resources":...}}'` | `npx --yes @tauri-apps/cli@2 build --bundles <...> [--config <override>]`；resources 映射在 `tauri.windows.conf.json` 里 |
| 对 `.app/Contents/Resources/bin` 里的 dylib 逐个 `codesign` + 重签 `.app` | 等价物是 Authenticode 签名整个安装包（`bundle.windows.certificateThumbprint`），未配置则跳过 |
| `xcrun stapler` 公证 `.dmg` | Windows 没有公证概念；见 §7 |
| 打印构建完成 + 缺失项 | 打印安装包路径 / 大小 / SHA256 / 构建时间，以及缺签名、缺 WiX 等提示 |

### 产物

```
target\release\bundle\nsis\AGI Mizar_0.1.1_x64-setup.exe
target\release\bundle\msi\AGI Mizar_0.1.1_x64_en-US.msi      # -Bundles msi 时
```

脚本最后对每个安装包打印 `sha256`；脚本的退出语义是**文件存在即成功**：Tauri CLI 非 0 退出但
安装包已经落盘时（典型原因是 updater 签名缺失），脚本只 warning 并继续；反过来 CLI 退出 0 但没有
安装包，脚本会失败。

### updater 的两条路

主配置里 `bundle.createUpdaterArtifacts = true` 且 `plugins.updater.pubkey` 已配置。这种组合下
**没有私钥时 Tauri CLI 会在生成安装包之后报错退出**（`A public key has been found, but no private
key`），不是「静默跳过」。所以：

- 本地构建（不带 `-WithUpdater`、无 key）：`package.ps1` 往临时目录写一份
  `{"bundle":{"createUpdaterArtifacts":false}}`，通过 `--config` 传进去，构建是成功的，只是没有
  `.sig` / `latest.json`；
- 发布构建（带 `-WithUpdater`）：必须有 key，产出 NSIS 安装包 + `.sig` + `latest.json`。

`--config` 用的是**临时文件路径**而不是内联 JSON 字符串：仓库路径里有空格（`AGI Mizar`），
内联 JSON 经 `npx` → `cmd.exe` 再解析容易出问题。

---

## 5. Tauri 2 的平台配置合并语义（已核对源码）

结论：`crates/agi-app/tauri.windows.conf.json` 会在 Windows 上**自动**和 `tauri.conf.json` 合并，
macOS 不受影响（macOS 仍然走 `package.sh` 的 `--config` 注入）。

依据（本机 cargo registry 源码，Tauri crate 2.12.5 依赖 `tauri-utils` 2.10.2）：

- `tauri-utils/src/config/parse.rs::read_from()`：先解析 `tauri.conf.json`，再
  `read_platform(target, root_dir)` 读平台文件，然后 `merge(&mut config, &platform_config)`；
- 平台文件名在 `ConfigFormat::into_platform_file_name()` 里固定：Windows 是
  `tauri.windows.conf.json`（macOS 是 `tauri.macos.conf.json`，Linux/Android/iOS 同理）；
- 合并方式是 **JSON Merge Patch（RFC 7396）**：对象逐层合并，数组整体替换，值写 `null` 删除键；
- `tauri build --config <file>` 再在这之上叠加，`--bundles` 优先级高于配置里的 `bundle.targets`。

补充一个合并细节：主配置的 `bundle.targets` 是 `["app","dmg","msi","nsis"]`（macOS 时代留下的）。
平台配置里把它覆盖成 `["nsis"]`，这样 Windows 上裸跑 `npx tauri build` 只会打 NSIS，不会去尝试
macOS 专属的 `app` / `dmg` target；要 MSI 时显式 `--bundles nsis,msi`。

`resources` 映射的语义是 `{ 源路径: 包内目标路径 }`，目录会递归展开。两个 target 的名字不是随便取的：

- `resources/agi-mizar-hook.exe` → `agi-mizar-hook.exe`：`agi-boot` 的 `shim_path()` 在 Windows 上
  找的是**主程序同目录**的 `agi-mizar-hook.exe`，而 NSIS 把 resources 写到 `$INSTDIR\<target>`，
  正好落在 `agi-mizar.exe` 旁边；
- `resources/bin` → `bin`：NSIS 写到 `$INSTDIR\bin\`，`llama-server.exe` 和它的一堆 DLL 同目录。

> macOS `.app` 里 resources 落在 `Contents/Resources/`，所以 `package.sh` 映射成
> `resources/agi-mizar-hook` / `resources/bin` 后，`shim_path()` 的第二个分支
> （`../Resources/<name>`）和 `agi-core` 的 `Contents/Resources/bin/llama-server` 都能找到。

---

## 6. 工具集缓存与网络（NSIS / WiX）

Tauri bundler 不打 NSIS/WiX 进来，首次打包时自己从 GitHub release 下载并校验，缓存在：

| 工具 | 缓存目录 | 下载物 |
|---|---|---|
| NSIS | `%LOCALAPPDATA%\tauri\NSIS` | `nsis-3.13.zip`（sha1 `DB14F8AF...F5E09B8E`，解压出 `makensis.exe` 等）+ `nsis_tauri_utils.dll`（v0.5.3，sha1 `75197FEE...9349B860`，放到 `Plugins\x86-unicode\additional\`） |
| WiX（仅 MSI） | `%LOCALAPPDATA%\tauri\WixTools314` | `wix314-binaries.zip`（sha256 `6ac824e1642d...`），需要 `candle.exe` / `light.exe` 等 |

**这是 bundler 自己的网络请求，不认 `scripts/*-llama` 的 `-Proxy`，也不会被 `npx` 的镜像配置影响。**
国内直连 GitHub release CDN 经常卡住或超时；卡住时不要反复重试，手工把工具集放进缓存目录即可：

```powershell
$proxy = 'https://gh-proxy.com/'

# NSIS：从镜像拿到 zip，解压到缓存目录（必须有 makensis.exe / Include / Stubs / Plugins）
$dst = "$env:LOCALAPPDATA\tauri\NSIS"
New-Item -ItemType Directory -Force $dst | Out-Null
Invoke-WebRequest "${proxy}https://github.com/tauri-apps/binary-releases/releases/download/nsis-3.13/nsis-3.13.zip" `
  -OutFile "$env:TEMP\nsis-3.13.zip"
Expand-Archive "$env:TEMP\nsis-3.13.zip" -DestinationPath "$env:TEMP\nsis" -Force
Copy-Item "$env:TEMP\nsis\nsis-3.13\*" $dst -Recurse -Force
# 插件 DLL（缺它 makensis 会报找不到 nsis_tauri_utils）
New-Item -ItemType Directory -Force "$dst\Plugins\x86-unicode\additional" | Out-Null
Invoke-WebRequest "${proxy}https://github.com/tauri-apps/nsis-tauri-utils/releases/download/nsis_tauri_utils-v0.5.3/nsis_tauri_utils.dll" `
  -OutFile "$dst\Plugins\x86-unicode\additional\nsis_tauri_utils.dll"

# WiX（只在 -Bundles msi 时需要）
$wix = "$env:LOCALAPPDATA\tauri\WixTools314"
New-Item -ItemType Directory -Force $wix | Out-Null
Invoke-WebRequest "${proxy}https://github.com/wixtoolset/wix3/releases/download/wix3141rtm/wix314-binaries.zip" `
  -OutFile "$env:TEMP\wix314-binaries.zip"
Expand-Archive "$env:TEMP\wix314-binaries.zip" -DestinationPath $wix -Force
# 确认 $wix\candle.exe 存在，否则下次 -Bundles msi 还会重新下载
```

Tauri 也支持 `bundle.useLocalToolsDir = true`，把工具缓存放进 `target/.tauri/`；CI/系统账户场景有用，
本仓库没开。

---

## 7. 签名、updater 与 SmartScreen

Windows 上「签名」是两个完全独立的东西，不要混：

1. **updater 的 minisign 签名**（`TAURI_SIGNING_PRIVATE_KEY`）：保证自动更新下载的安装包可信，
   产出 `*.sig` 和 `latest.json`。和 Authenticode 无关；`package.ps1 -WithUpdater` 走这条。
2. **Authenticode 代码签名**（Windows SmartScreen 认的那个）：可选项。Tauri bundler 用
   `bundle.windows.certificateThumbprint` 指定的证书，通过 `signtool` 签名安装包和主程序；
   也支持 `bundle.windows.signCommand` 自定义（Azure Trusted Signing 之类）。

   注意：**它不会像 macOS 的 `APPLE_CERTIFICATE` 那样从环境变量导入 PFX**。要用的话：
   - 先 `Import-PfxCertificate ... -CertStoreLocation Cert:\CurrentUser\My`；
   - 再把 SHA1 指纹写进一个 config 文件，例如 `signing.windows.json`：
     ```json
     { "bundle": { "windows": { "certificateThumbprint": "A1B2...", "digestAlgorithm": "sha256", "timestampUrl": "http://timestamp.digicert.com" } } }
     ```
   - `npx --yes @tauri-apps/cli@2 build --bundles nsis --config signing.windows.json`；
   - `signtool` 找不到时用 `TAURI_WINDOWS_SIGNTOOL_PATH` 指向它（默认去 Windows SDK 注册表里找）。

**没有证书时**：安装包照常能用，但首次运行会出现 SmartScreen「Windows 已保护你的电脑」，
需要点「更多信息 → 仍要运行」。企业机器可能连「仍要运行」都没有（策略禁止）；这是它的正常行为，
不是打包坏了。签名证书需要 OV/EV 代码签名证书 + 私钥，属于发布流程之外的现实成本。

MSI 是否需要单独签：是，同一套 `bundle.windows` 配置，`signtool` 同时签 MSI 和 NSIS 产物。

---

## 8. CI

`.github/workflows/ci.yml` 新增 `windows` job（`windows-latest`）：

1. `scripts/fetch-llama.ps1 -Stub` 造出编译所需的 resource 路径；
2. `cargo clippy --all-targets -- -D warnings`；
3. `cargo test --all`。

**这个 job 没有 `cargo fmt --check`**：当前仓库在 rustfmt 1.9 下存在跨平台/跨版本的格式漂移（macOS
上同样红），本轮范围是 Windows 行为对等，不顺手全仓 reformat；mac 的 `rust` job 保持原样。

`.github/workflows/release.yml` 新增 `windows` job（tag `v*`，`needs: macos`）：

- 版本一致性检查（tag `v0.1.1` ↔ workspace `0.1.1`，PowerShell 版）；
- `cargo build --release -p agi-hook-shim` 并复制成
  `crates/agi-app/resources/agi-mizar-hook.exe`；
- `scripts/fetch-llama.ps1` 装真实运行时；
- `tauri-apps/tauri-action@v0`，`args: --bundles nsis`，用现有 secrets
  （`TAURI_SIGNING_PRIVATE_KEY` / `_PASSWORD`）产出带 `.sig` 的 NSIS 和 `latest.json`，进 draft release。

`needs: macos` 是为了让两个 job 都调 `tauri-action` 时不抢着创建同一个 draft release；代价是
Windows 任务在 macOS 之后串行。Authenticode 的 CI 接入（`WINDOWS_CERTIFICATE` /
`WINDOWS_CERTIFICATE_PASSWORD` / 指纹）在 `release.yml` 里以注释说明，没有默认开启——
没有证书时 release 一样能出，只是未签名。

同一轮也补了 `macos` job 缺 hook shim 的既有缺口（对齐本地 `package.sh` 的行为）：先分别构建
`agi-hook-shim` 的 arm64 / x86_64 两片，`lipo -create` 成
`crates/agi-app/resources/agi-mizar-hook` 并 `chmod +x`，再通过 tauri-action 的
`args: --target universal-apple-darwin --config '{"bundle":{"resources":{"resources/agi-mizar-hook":"agi-mizar-hook"}}}'`
注入。要点：

- app bundle 是 universal，shim 也必须是 universal（单 arch 的嵌套二进制在另一种 Mac 上会
  `bad CPU type`）；
- **只注入 shim**：这个 job 不跑 `fetch-llama.sh`，写 `resources/bin` 会因源路径不存在让 bundler 失败；
- macOS 仍然走 `--config` 注入，不使用 `tauri.windows.conf.json`；原有签名/公证/universal/dmg
  逻辑没有改动。

---

## 9. 与 macOS 的差异

| 维度 | macOS（`package.sh`） | Windows（`package.ps1`） |
|---|---|---|
| 产物 | `.app` + `.dmg`（可 universal） | `.exe` NSIS 安装包（默认）；`.msi` 需 WiX |
| hook shim | `agi-mizar-hook`（无扩展名） | `agi-mizar-hook.exe` |
| shim 落点 | `AGI Mizar.app/Contents/Resources/`（也在 `Contents/MacOS` 旁边找） | 安装目录根：`%LOCALAPPDATA%\AGI Mizar\agi-mizar-hook.exe`，和主程序同级 |
| llama 运行时 | 双架构 lipo 成一份（arm64 Metal 文件保留） | 单架构 CPU x64（`llama-server.exe` + DLL 的整目录） |
| 资源目录 | `Contents/Resources/bin/` | `$INSTDIR\bin\` |
| 二进制签名 | `codesign` 逐个 dylib + 重签 `.app` | 不需要；Authenticode 签安装包（可选） |
| 公证 | `notarytool submit` + `stapler` | 没有对应概念；SmartScreen 靠签名与下载信誉 |
| updater 产物 | `.app.tar.gz` + `.sig` + `latest.json` | `*-setup.exe` + `.sig` + `latest.json` |
| 安装位置 | `/Applications` | 默认 per-user：`%LOCALAPPDATA%\AGI Mizar\`（NSIS `installMode` 默认 currentUser） |
| 自启 | Login Item API | 注册表/启动项（壳层代码，`win-shell` 在改；打包脚本不涉及） |
| 路径常量 | `~` / `~/Library/...` | `%USERPROFILE%` / `%APPDATA%`（内核侧在做对等，不在本文范围） |

---

## 10. 限制与未决

- **没有签名证书**：安装包未签名，SmartScreen 会警告；updater 的 minisign 只保护自动更新链路。
- **MSI 未在本机验证**：本机缓存只有 NSIS（`%LOCALAPPDATA%\tauri\NSIS`），没有 WiX；
  `-Bundles nsis,msi` 需要先按 §6 放好 `WixTools314`。安装包内容（hook / llama）两种格式共用同一份
  resources 配置，但 MSI 的目录布局和卸载行为没有实机核对过。
- **只支持 x64**：没有 Windows ARM64 产物；也没有把 CUDA 版 llama.cpp 打进包（默认 CPU）。
- **`resources/` 不入库**：release CI 每次都要 `fetch-llama.ps1` 联网（约 24 MB）+ 校验；
  离线打 release 包需要 `-Archive`。
- **`package.ps1` 的「文件存在即成功」**：CLI 非 0 但安装包落盘时只 warning，避免把
  「只缺 updater 私钥」误判成打包失败；发布流程仍应要求 `-WithUpdater` 并检查 `.sig` 存在。
- **macOS release job 的 llama 运行时**：hook shim 已修（见 §8，GitHub 的 macOS 包现在会带
  `agi-mizar-hook`，且是 universal 的）；但这个 job 仍不跑 `fetch-llama.sh`，所以 GitHub 的 macOS
  包**不含** `llama.cpp` 运行时（本地 `package.sh` 会带）。补齐需要按 `package.sh` 的流程 fetch、
  对 `Resources/bin` 里每个 dylib 先 `codesign` 再重签 `.app`，属于 macOS 发布链路的独立改动。

---

## 11. 本轮验证记录

- `crates/agi-app/tauri.windows.conf.json` 的合并语义：核对 `tauri-utils` 2.10.2 源码
  （`config/parse.rs`），确认 JSON Merge Patch + 平台文件名固定；`tauri build --help` 确认
  `-c/--config` 支持「JSON 字符串或 JSON 文件路径」，`-b/--bundles` 在 Windows 上可选值是
  `msi, nsis`。
- `tauri-build` 2.7.4 源码确认 resources 在 build script 阶段被校验/拷贝（`src/lib.rs`，
  `bundle.resources` → `copy_resources(ResourcePaths …)`），并对照
  `.zwork/win-verify/package/nsis-build-C-resources.log` 里的真实报错
  `resource path \`resources\bin\` doesn't exist`。
- `tauri-bundler` 2.10.4 源码确认：NSIS 模板用 `File /a "/oname=<target>"` 写到 `$INSTDIR\<target>`；
  默认 `installMode=currentUser` 时 `$INSTDIR = $LOCALAPPDATA\<productName>`；NSIS/WiX 的下载 URL、
  SHA1/SHA256 与缓存目录（见 §6）。
- 用 `.zwork/win-verify/llama/llama-b12000-bin-win-cpu-x64.zip` 实测
  `fetch-llama.ps1 -Archive`：SHA256 校验通过（`6FB512A7…6E067220`）、52 个文件 / 60.5 MiB 解压到
  `resources\bin`、`llama-server.exe --version` 输出
  `version: 0.8.0-dev (build 12000, commit 68d20b884)`；把 zip 换成随便一个错误文件时脚本以
  `sha256 mismatch` + exit 1 拒绝；`-Stub` 只建占位路径、不联网。
- `package.ps1` 用假 `cargo` / `npx`（放在 `.zwork/win-verify/package-dryrun/`）干跑过完整流程：
  从任意 cwd 调用都会 `Set-Location` 到仓库根；`--config` 传的是临时文件路径（避开仓库路径里的
  空格经 `npx` → `cmd.exe` 再解析）；`-Bundles nsis,msi` 会透传并提示 WiX 缓存缺失；`-Bundles foo`、
  `-WithUpdater` 无 key、CLI exit 0 但没有新安装包这三种情况都以非 0 退出；产物按本次构建时间过滤，
  不会把上一次的旧安装包当成结果。**这只是流程干跑，真实 `tauri build` 没有在本步执行。**
- `release.yml` 的 macos job 补 shim 之后：`npx js-yaml` 解析通过；用 tauri-action `v0` 实际依赖的
  `string-argv` 0.3.2 解析 `args`，确认 inline `--config` 的 JSON 被切成**一个**参数
  （`["--target","universal-apple-darwin","--config","{\"bundle\":...}"]`）；对照 tauri-action 源码
  `src/config.ts`，它的 `mergeUserConfig()` 先按 inline JSON 解析、失败才当文件路径，所以
  `getInfo()` 也不会被这个写法绊倒。macOS 的 lipo/签名段是 shell 步骤，本机（Windows）无法执行，
  只做了静态核对，没有实跑 macOS 构建。
- 完整 `npx tauri build --bundles nsis` 与安装/卸载实机验证由打包验证步骤统一执行，不在本文的
  脚本级验证范围内；`package.ps1` 的构建参数、`--config` 覆盖和产物查找逻辑按上面的源码事实编写。
