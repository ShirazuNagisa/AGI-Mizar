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

### 两个安装包：NSIS 是 per-user，MSI 是 per-machine

这是两种格式在 Windows 上最大的行为差异，打包和测试都要按它对：

| | NSIS（`*-setup.exe`，默认产物） | MSI（`-Bundles msi`） |
|---|---|---|
| 安装范围 | **per-user**（Tauri NSIS `installMode` 默认 `currentUser`） | **per-machine**（Tauri Wix 模板写死 `InstallScope="perMachine"`） |
| 安装目录 | `%LOCALAPPDATA%\AGI Mizar\` | `C:\Program Files\AGI Mizar\` |
| 权限 | 普通用户即可，不弹 UAC | **需要管理员/elevated shell**，否则 UAC 弹窗（`/qn` 下无法确认，会挂起） |
| 快捷方式/卸载登记 | 当前用户开始菜单 + HKCU | 所有用户开始菜单 + HKLM |
| 适合 | 普通用户下载安装、自动更新载体 | 企业分发/系统级安装 |

两种格式的 payload 布局相同：`agi-mizar.exe`、`agi-mizar-hook.exe`（同目录）、`bin\llama-server.exe`
+ DLL。静默安装/卸载（在**管理员** PowerShell 里跑 MSI）：

```powershell
# NSIS（per-user，当前用户即可；/S 静默）
Start-Process -FilePath 'target\release\bundle\nsis\AGI Mizar_0.1.1_x64-setup.exe' -ArgumentList '/S' -Wait
Get-ChildItem "$env:LOCALAPPDATA\AGI Mizar"
& "$env:LOCALAPPDATA\AGI Mizar\uninstall.exe" /S     # 卸载

# MSI（per-machine，必须是管理员 shell）
Start-Process msiexec.exe -ArgumentList '/i', '"D:\path\AGI Mizar_0.1.1_x64_en-US.msi"', '/qn', '/norestart', '/L*v', 'C:\temp\agi-msi-install.log' -Wait
Get-ChildItem 'C:\Program Files\AGI Mizar'
Get-ChildItem 'C:\ProgramData\Microsoft\Windows\Start Menu\Programs' | Where-Object Name -like '*AGI Mizar*'
# 卸载（ProductCode 见 MSI 的 Property 表，或用“应用和功能”里的卸载项）
Start-Process msiexec.exe -ArgumentList '/x', '{PRODUCT-CODE}', '/qn', '/norestart', '/L*v', 'C:\temp\agi-msi-uninstall.log' -Wait
```

想在不提权的情况下检查 MSI 内容，用**administrative install**（只展开文件、不登记安装）：

```powershell
msiexec /a "AGI Mizar_0.1.1_x64_en-US.msi" /qn TARGETDIR="D:\temp\agi-msi-extract"
```

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
2. **Authenticode 代码签名**（Windows / SmartScreen 认的那个）：对 NSIS `.exe` 和 MSI 用
   `signtool` 签名。Tauri bundler 原生支持 `bundle.windows.certificateThumbprint`（安装包 + 主程序
   一起签）和 `bundle.windows.signCommand`（Azure Trusted Signing 之类）；`scripts/package.ps1`
   在 bundler 之外再对**产物安装包**做一次显式签名/校验。

### 本地用 `package.ps1` 签名

```powershell
# PFX 文件（发布方手里有 .pfx + 密码的常见路径）
scripts\package.ps1 -Bundles nsis,msi `
  -PfxPath C:\certs\agi-mizar.pfx -PfxPassword $env:PFX_PASSWORD `
  -TimestampUrl http://timestamp.digicert.com

# 证书已导入 CurrentUser\My（或 LocalMachine\My，自动加 /sm）
scripts\package.ps1 -Thumbprint 0123456789ABCDEF0123456789ABCDEF01234567 `
  -TimestampUrl http://timestamp.digicert.com
```

- 签名在**产物落盘之后**做：`signtool sign /fd SHA256 /f <pfx> /p <pwd> [/tr <url> /td SHA256]`
  （`-Thumbprint` 换成 `/sha1`）；不传 `-TimestampUrl` 只做无时间戳签名（证书过期后签名失效）。
- 随后 `signtool verify /pa /v` + `Get-AuthenticodeSignature`，输出 Status / Subject / Thumbprint /
  时间戳证书，并重新打印签名后的 SHA256。
- 校验门的判定与语言无关：`signtool verify /pa` 在链不受信时退出码是 1，而 `Get-AuthenticodeSignature`
  给出 `UnknownError`/`NotTrusted` 且有 SignerCertificate —— 这一对被视为「签名完整、仅信任链不满足」
  的预期结果（自签名场景）；只有 `NotSigned`/`HashMismatch`/无 Signer 才算失败。`signtool` 的提示文本会随
  Windows 显示语言本地化，不要靠匹配英文句子（2026-11-03 在中文系统上踩过：只匹配英文导致
  `-SelfSigned` 在签名后误报失败，已改为按 `Get-AuthenticodeSignature` 判定）。
- **失败语义是 fail-closed**：`-PfxPath` 不存在、密码错、`signtool` 执行失败、签名后没有 SignerCertificate
  或 `Get-AuthenticodeSignature` 返回 `NotSigned`/`HashMismatch`，脚本都返回非 0——不会静默产出一个未签名
  的安装包。`signtool` 从 PATH 或 Windows SDK 目录自动找（本机是
  `C:\Program Files (x86)\Windows Kits\10\bin\10.0.28000.0\x64\signtool.exe`），也可以用
  `TAURI_WINDOWS_SIGNTOOL_PATH` 指定。
- 不传任何签名参数时保持原行为：安装包产出、提示未签名。

### 没有证书时验证签名链路：`-SelfSigned`

```powershell
scripts\package.ps1 -Bundles nsis,msi -SelfSigned
```

它会创建临时自签名代码签名证书（`CN=AGI Mizar Test Signing`）→ 导出临时 PFX → 导入
`Cert:\CurrentUser\TrustedPeople` → 对 NSIS/MSI 签名 → `signtool verify /pa` +
`Get-AuthenticodeSignature` → **清理**（TrustedPeople、`CurrentUser\My`、临时 PFX 全部删除）。

实测边界（本机 Windows 11、非管理员、无证书）：

- 签名本身成功，`Get-AuthenticodeSignature` 找到签名且 Signer 证书指纹与临时证书一致；
- `Status` 是 **`UnknownError`**，`signtool verify /pa` 报
  `A certificate chain processed, but terminated in a root certificate which is not trusted`；
- 原因是自签名证书是链的根：只放进 `TrustedPeople` **不足以**让 Authenticode 信任链成立。要让
  Status 变成 `Valid`，必须把证书放进 `CurrentUser\Root`（受信任的根证书颁发机构）。
- 往 Root 写入会弹**交互式安全警告对话框**（`certutil -user -addstore Root` 和 .NET `X509Store` 都实测
  会阻塞等待确认），在 CI / 无人值守会话里不能自动化，强行做会挂住流水线。所以 `-SelfSigned` 的正确
  断言是「签名存在、完整、只有信任链这一项不满足」，不是「Status=Valid」。人工想看到 `Valid` 可以：

  ```powershell
  # 交互式会话里手动接受一次安全警告（仅测试机；测完务必删除）
  certutil -user -addstore Root C:\temp\agi-mizar-test.cer
  Get-AuthenticodeSignature 'target\release\bundle\nsis\AGI Mizar_0.1.1_x64-setup.exe' | Format-List Status, SignerCertificate
  certutil -user -delstore Root <thumbprint>
  ```

- 自签名**永远不能**消除 SmartScreen 警告，也**不要**把测试证书提交进仓库；`-SelfSigned` 只用于证明
  「签名 → 校验 → 清理」链路在无证书环境可用。

### CI（release.yml 的可选 Authenticode）

`release.yml` 的 windows job 支持两种可由发布方提供的 secrets，**仓库里没有、也不应该有证书**：

| secrets | 用途 |
|---|---|
| `WINDOWS_CERTIFICATE_BASE64` + `WINDOWS_CERTIFICATE_PASSWORD` | base64 编码的 `.pfx`；job 里导入 `CurrentUser\My`，用 `bundle.windows.certificateThumbprint` 让 tauri-bundler 调 `signtool` 签名主程序和 NSIS 安装包 |
| `WINDOWS_THUMBPRINT` | 证书已经预装在 runner 证书库时，直接用这个 SHA1 指纹（自定义 runner 镜像才用得上；base64 secret 优先） |

两者都不存在时 job 照常构建，只是安装包未签名。签名使用
`timestampUrl = http://timestamp.digicert.com`，避免证书到期后历史版本签名失效。注意 macOS 的
`APPLE_CERTIFICATE` 那套**不适用于 Windows**：tauri-bundler 不会从环境变量导入 PFX，必须先把证书
导进证书库再传指纹（workflow 里的 import 步骤做的就是这个）。

### SmartScreen 到底由什么决定

- 未签名 / 自签名：首次运行弹「Windows 已保护你的电脑」，需要「更多信息 → 仍要运行」；企业策略可能
  完全禁止。
- **受信 CA 签发的代码签名证书（OV/EV）+ 时间戳**：这是必要条件，但不是充分条件。SmartScreen 还看
  下载声誉：新证书/新文件仍可能警告，直到累计下载量建立声誉；EV 证书通常能更快获得信任。
- 证书私钥泄露等于发布渠道被接管；CI 里应使用 secrets + 仅发布 job 可用的环境，构建完清理导出的 PFX
  （workflow 的 import 步骤在 `finally` 里删除临时文件）。
- MSI 和 NSIS 需要各自签名；`bundle.windows` 这套配置对两种产物都生效。

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
Windows 任务在 macOS 之后串行。Authenticode 是**可选**的：只有发布方提供
`WINDOWS_CERTIFICATE_BASE64` + `WINDOWS_CERTIFICATE_PASSWORD`（或 `WINDOWS_THUMBPRINT`）时，job 才导入
证书并把 `bundle.windows.certificateThumbprint` 传给 tauri-bundler；没有 secrets 时构建照常、安装包
未签名。仓库里不保存任何证书/私钥，细节见 §7。

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

- **证书**：仓库没有代码签名证书，release 默认产出**未签名**安装包；`package.ps1` 已支持
  `-PfxPath/-PfxPassword`、`-Thumbprint` 和自签名测试（§7），但本机只有自签名链路可验证。
  自签名无法让 `Get-AuthenticodeSignature` 变成 `Valid`：把临时证书放进 `CurrentUser\Root` 需要
  交互式安全警告确认（`certutil -user -addstore Root` 和 .NET `X509Store` 都实测阻塞），无人值守
  会话不能做；没有 CA 证书 + 下载声誉，SmartScreen 仍会警告。真实证书由发布方通过
  `WINDOWS_CERTIFICATE_BASE64`/`WINDOWS_THUMBPRINT` secrets 提供，本机没有 secrets，**没有实测**
  release 的签名步骤。
- **MSI 安装未在本机实机验证（环境限制，不是产物缺陷）**：本机非管理员，Tauri MSI 是
  `InstallScope=perMachine`，`msiexec /i /qn` 会在 UAC 安全桌面挂起（实测 582.5s，超时被杀），
  连 `/a` administrative install 也需要提权（实测 300s 后被杀的同一现象）。本机改用
  **7-Zip 解包 + MSI File/Component/Directory 表**核对 payload（54 个文件 / 84.1 MiB，
  逐个大小与 File 表一致；`agi-mizar.exe` + `agi-mizar-hook.exe` 在安装根、50 个 llama.cpp
  文件在 `bin\`），并用解包出来的 `agi-mizar.exe` 启动验证（endpoint.json/hook.json 正常落盘、
  `/healthz` 200）。真实安装/卸载需在**管理员 PowerShell** 里执行（命令见 §4「两个安装包」）。
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
- 完整打包（2026-11-02 23:15–23:21，一次 cargo 构建）：`scripts\package.ps1 -Bundles nsis,msi -SelfSigned`
  成功，NSIS 20.4 MiB / MSI 33.2 MiB。自签名前后 hash、`Get-AuthenticodeSignature` Status、清理结果
  见 `.zwork/win-verify/fixes/signing-msi/build-sign-nsis-msi.log`：
  - NSIS：签名前 `1E7CC03B…AAEEF25`，签名后 `11E4CDE3…622FAE9`；
  - MSI：签名前 `D1E19621…F216066D`，签名后 `63629E8D…ABCD32A`；
  - 两个产物都是 `Status=UnknownError` + `CN=AGI Mizar Test Signing`，`signtool verify /pa` 报
    「链终止于不受信根」（自签名的预期结果，见 §7）；临时证书已从 `My`/`TrustedPeople` 删除，PFX
    已删（脚本 finally 清理）。
- WiX 预置：官方 `wix314-binaries.zip` 41,297,555 B，sha256 `6AC824E1…FF43D31` 与
  tauri-bundler 2.10.4 常量一致；解压到 `%LOCALAPPDATA%\tauri\WixTools314` 后 `candle.exe` 可直接
  运行（本机只有 .NET 4.8，没有 3.5，WiX 3.14 不需要旧框架），MSI 构建没有触发自动下载。
- 签名脚本的其余路径也实测过：不带签名参数时照旧产出未签名安装包并 exit 0；`-Thumbprint` 用临时
  证书（`CurrentUser\My`，无 PFX 文件）签名并通过校验（同样是 `UnknownError`/链不受信的预期结果），
  证书随后从证书库删除。证据在同一目录的 `pretest-signing.log` 与 `SUMMARY.md` 里。
- MSI 安装验证（环境限制）：非管理员下 `msiexec /i /qn` 挂起 582.5s（UAC 安全桌面，Approval 被禁用
  无法确认）后被杀；`ALLUSERS=2 MSIINSTALLPERUSER=1` 单次尝试同样挂起 150s 被杀；连
  `msiexec /a` administrative install 也需要提权，300s 后被杀。因此改用 7-Zip 解包 + MSI
  File/Component/Directory 表核对 payload：54 文件 / 84.1 MiB、逐个大小与 File 表一致、
  `agi-mizar.exe`+`agi-mizar-hook.exe` 在根、50 个 llama.cpp 文件在 `bin\`；解包产物
  `agi-mizar.exe --background` 可启动，`endpoint.json`/`hook.json` 落盘、`/healthz` 200
  （`/api/v1/state` 用 endpoint token 返回 401，属运行时/Auth 问题，不在本步结论内）。证据：
  `.zwork/win-verify/fixes/signing-msi/`（`msi-database.txt`、`msi-payload.log`、
  `msi-verification.log`、`pretest-signing.log` 等）。真实安装/卸载需管理员 PowerShell，命令见 §4。

### 2026-11-03 最终重建（判官/协议/信封 R-B1 修复后）

- 代码：`windows-support` 工作树，测试 376 passed / 0 failed / 1 ignored，clippy exit 0。
- `scripts\package.ps1 -Bundles "nsis,msi"` 一次构建出：
  - NSIS `AGI Mizar_0.1.1_x64-setup.exe` 21,383,101 B / sha256 `961C61AC8E20BFD4CCB269EE16F2549AD7FDE35D6CBC1A06EA57FB9FBDA7E7B8`（per-user，未签名）；
  - MSI `AGI Mizar_0.1.1_x64_en-US.msi` 34,807,808 B / sha256 `A40EE111D2A6A113974E948841DB98F95457E0DBC3CA1CA05B26E29F24BD74D4`（perMachine，未签名）。
- 再跑 `-Bundles nsis -SelfSigned`：签名后 NSIS 21,392,968 B / sha256 `A678E50C48BF60B4CECDD1E9BC198D6BF3C23BB7E29B66F1B26D0F8BF5B799A3`，
  `Status=UnknownError`、`CN=AGI Mizar Test Signing`、`signtool verify /pa` 报链不受信（预期）；临时证书/PFX 已清理。
- 最终 NSIS（未签名与自签名两份都）安装/启动/关窗/退出/卸载全 PASS：安装 2.2–2.5s、55 文件 / 88,328,878 B、
  hook 在安装根、`bin\` 52 文件；退出删除 endpoint/hook；卸载后目录/快捷方式/注册表全清。
- 证据：`docs/evidence/agi-mizar-windows-2026-11-02/fixed/limitations/packaging/`（工作区仓库）。

### 2026-11-07 全量验证（origin/main `e0ed908` + 本轮 Windows 修复）

本轮把 `e0ed908`（ags-lan/agi-peer/agi-remote、dataset/plugin/remote、Codex CDP、远程 Agent 设置页）
在 Windows 上从零编译到安装跑通；完整记录在工作区
`docs/session-notes/2026-11-07-agi-mizar-windows-full-verification.md`，证据在
`docs/evidence/agi-mizar-windows-2026-11-07/`。要点：

- 门禁：`cargo build --workspace` 通过 → `cargo test --workspace --no-fail-fast` **458 passed / 0 failed**
  → `cargo clippy --all-targets -- -D warnings` **0 warning** → `cd ui && npm run build` 通过。
  修掉的 Windows 编译阻断：`agi-engine-codex/src/gui_ipc.rs` 的 non-unix stub 签名、
  `agi-core/tests/remote_installer.rs` 的 unix import。
- 打包：`scripts\package.ps1 -Bundles nsis,msi` 一次出 NSIS **23,790,852 B** /
  sha256 `50CA0FA55FBD89C265D42E72A0334840AEE73AB9B34145E37C1A15890556B0BD`、MSI **38,506,496 B** /
  sha256 `9331DFF10851A6AC39184E07C0012D77DDB8817E66E696CE38BB8B34300DB677`。
  `package.ps1` 现在会 `cargo build --release -p agi-remote` 并 stage 到 `resources/bin`（安装包 `bin\agi-remote.exe`），
  SHA256 用 `.NET` 计算（本机从 Git Bash 起的 PowerShell 5.1 会丢 `Get-FileHash`；签名链路改用 `pwsh` 7 跑）。
- 安装/生命周期：NSIS `/S` 安装 → 启动（0.1.1）→ 托盘面板 → 关窗隐藏 → 单实例 → 后台 `--background`
  启动（无主窗）→ `app_quit` → `uninstall.exe /S` 全通过；安装版 `HKCU\...\Run` 写入
  `"<install>\agi-mizar.exe" --background`，关闭即删除。
- 判官/引擎：真模型 `live_judge` 2/2（chat completions → `Some(Done)` 7.58 s）；假手机租约后内核自动拉起
  sidecar（ready, port 55601）；Codex 真 turn PONG；Codex GUI 只读来源 16 条 `codex_desktop`。
- 平台缺口（非本轮回归）：Codex GUI 控制仅 unix；Claude Desktop GUI 来源仅 macOS 路径；上下文压缩没有
  桌面 UI/API 入口。需要账户/手机的 LAN 证书、WAN、生产 dataset 上传未验证（本机未登录）。
- 另：`../dsh-relay` 的两个 Windows 缺陷（直跑守卫、GNU tar `D:` 主机）已修，relay 334/334、dsh 74/74。

### 2026-11-07 自绘标题栏

Windows 不再显示系统标题栏：窗口 `decorations(false) + shadow(true)`，页面在右上角绘制
最小化 / 最大化 / 关闭（46×48，关闭键 hover 变 Windows 红 `#c42b1c`），`Toolbar` 在 Windows 下
`pr-[144px]` 让页面自己的右上角按钮左移。拖动靠 32px 拖动条 + 工具栏 `data-tauri-drag-region`
（双击切换最大化）；缩放靠 8 个贴边热区调用 `start_resize_dragging`（tao 只接管有阴影窗口的顶边）。
实现与验证记录见工作区 `docs/session-notes/2026-11-07-windows-custom-titlebar.md`；安装包
23,796,282 B / SHA256 `7D0C54D9…DB1C5`。限制：窗口宽度固定 1034（年份格子），最大化按钮没有
Windows 11 的 Snap Layouts 悬停面板（自绘按钮拿不到系统 caption 的贴靠面板）。

### 2026-11-10 适配性全量复验（origin/main `5e6cb82d`，本轮修复 rebase 后 `6e8fa2d3`）

macOS 侧又推进 29 个提交（remote_health 依赖检查+日志导出、remote intake/权限预设、relay 半开重连、
二进制隧道帧、五弧环图标、onboarding 与设置页重做）后，Windows 侧复验并修掉：

- 门禁：rustc 1.97 的 clippy 两处红灯（`bool::then` → `then_some`、测试回调 `result_large_err`）；
  修后 `cargo test --workspace --no-fail-fast` **489/0**、`clippy --all-targets -- -D warnings` **0**。
- `ui/dist` 在 **Windows 上重建与仓库提交逐字节一致**（Tailwind 显式 `@source` 后不再需要 Linux 构建）。
- 平台修复：所有控制台子进程带 `CREATE_NO_WINDOW`（依赖修复不再闪黑框）；依赖检查按 `DSH_PROFILE` 解析
  配置档、sidecar 缺失时 MCP 判定为 `unknown`、路径走 dunce（报告与 `cordis.patch.yml` 无 `\?\`）；
  Claude 桌面会话来源按平台取 `%APPDATA%\Claude{,-3p}`；设备列表同时按 roster 名与当前机器名过滤本机
  （boot 顺带刷新 roster 名）；窗口高度按工作区 clamp、顶部 banner 补 144px 让位；WebView2 的浏览器右键
  菜单与 F5/Ctrl+R 重载在页面层关掉（输入框保留原生菜单）；引导手机卡片补 Android APK 入口、
  偏好步文案与默认值一致、Codex GUI 面在 Windows 如实写「只读」。
- 打包/安装：`scripts/package.ps1 -Bundles nsis` → **22.8 MiB**，SHA256
  `9ef239c6330e12bdc9ad3458707b2d1cc68846e426d4e0afbcd84de1a6e04b42`；`/S` 覆盖安装 exit 0，
  安装版 `agi-mizar.exe` 30,215,168 B / SHA256 `042E6821…FCE56A`。
- 运行时：安装版 27 项矩阵 **27 PASS / 0 FAIL**（依赖完整性报告 8 项 ok、导出日志原生「另存为」可取消
  且不报错、F5/右键守卫、shell IPC 往返、更新检查…）；Win32 层：最大化 3862×2110（工作区 3840×2088，
  不盖任务栏）、最小化/恢复、关窗隐藏进程存活、单实例 0.1 s 退出并唤窗、`--background` 全窗口隐藏且
  `/healthz` 200、`HKCU\...\Run` 写入 `"<install>\agi-mizar.exe" --background`。
- 证据：`docs/evidence/agi-mizar-windows-2026-11-10/`；完整记录
  `docs/session-notes/2026-11-10-agi-mizar-windows-parity.md`。
- 仍存缺口：Codex GUI 桌面控制仍仅 macOS/Linux（Windows 无可驱动桌面端）；Snap Layouts 悬停缺失；
  宽度锁 1034 使 Win+←/→ 分屏失效；Authenticode 未签名；MSI 需提权。
### 2026-11-10 15:45 · 再更新到 `f6a1d928`（presence tunnel 重建安装）

上游 `560e9a5e fix(relay): keep a presence tunnel on the account instance`（`crates/agi-boot/src/lib.rs`
+60 行：签入时在账户实例 `:8443` 再挂一条同房间隧道，供 iOS `/desktops` 与旧配对使用）合入后，
本机从 `8d3a68a3` fast-forward 到 `f6a1d928`，重新跑门禁/打包/覆盖安装/运行时复验：

- 门禁 `cargo test --workspace --no-fail-fast` **489/0**、clippy **0**；
- NSIS **22.8 MiB**，SHA256 `9ef239c6…`；安装版 exe 30,215,168 B / SHA256 `042E6821…`；
- 运行时矩阵 27 PASS / 0 FAIL；新代码在 Windows 上的直接证据是进程同时保有
  `…:443` 与 `…:8443` 两条已建立连接（`Get-NetTCPConnection -OwningProcess <pid>`）；
- 安装版以 WMI 方式后台启动（pid 29800），窗口 1034×802、`/healthz` 200。
### 2026-11-10 22:15 · 远程 Agent 新功能验证（allow-all workspaces / 委派模型 / SKILL 重写）

上游 `7933c687`（`81e61c73` allow-all workspaces + 委派模型/思考强度 + 一轮式 intake；`51be8bc2` UI；
`f644b86e` 文档；`7933c687` rustfmt）后的 Windows 复验：

- 门禁 `cargo test --workspace --no-fail-fast` **490/0**、clippy **0**；Windows 重建 `ui/dist` 与仓库
  逐字节一致；NSIS SHA256 `796d151c…`，安装版 exe `440CBD4E…`。
- 实测（安装版）：`allowAllWorkspaces` 字段/开关/条件渲染/按钮文案/持久化全对；`remote_delegate` 的
  `provider`/`model`/`reasoning_effort` 能被解析（假 peer → `REMOTE_PEER_UNKNOWN`）；把 SKILL.md 写坏后
  修复能恢复成与仓库逐字节相同的 7380 B 且 `changed=[]`（幂等）；回归矩阵 27/27。
- **重要**：第一次覆盖安装时 `bin\agi-remote.exe` 没被更新——DSH 把安装版 sidecar 当 MCP 子进程常驻，
  文件被占用，NSIS 静默跳过（安装器仍 exit 0），于是「检查依赖完整性」用旧 sidecar 的旧 SKILL 写盘，
  skill 一直 `outdated`。更新前先 `Stop-Process -Name agi-remote` 再安装（DSH 约 1 s 后重启它，要抢窗口），
  装完确认 `bin\agi-remote.exe` 哈希 = `target/release/agi-remote.exe`，再点修复。
  详见工作区词条 `desktop.windows-sidecar-locked-update`。