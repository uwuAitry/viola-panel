# viola-panel 设计文档

> 状态：设计共识已达成（/grill-me 收敛，2026-10-02）。界面层于 2026-10-03 修订为 WebView，见第 9 节。**未开始实现**。
> 本文是设计与实现计划的唯一权威，改动先改本文。

## 0. 这是什么 / 不是什么

**是什么**：一个独立的 Windows 桌面程序（Rust + wry/tao 承载 WebView2），用于查看与调整 **orender（omniphony-renderer）的输出侧设置**，并作为 orender + ffplay 的启动器。

**不是什么**：
- 不是 viola-bridge 的一部分。独立仓库、独立 CI、独立版本。**不复制、不链接 viola-bridge 的代码**，只通过进程边界调用。
- 不改 Studio One 的 ASIO 设置。S1 的 ASIO 设备恒为 `viola-bridge ASIO`，面板不碰。
- 不是 ASIO 驱动自身的控制面板宿主。它只是能**代你打开**当前输出驱动的控制面板。
- 本轮**不做**「在 S1 的音频设备设置里打开本面板」。该路径 2026-10-03 已在 viola-bridge 侧实现并装机验证（见 D1）。

## 1. 背景与硬事实（均已核实，带证据）

### 1.1 当前出声链路
```
S1 --ASIO--> viola_asio.dll --写--> \\.\pipe\orender.input --> orender --file 后端--> stdout --> ffplay --> Windows 默认输出端点
```
这是唯一在本机实测能出声的路线（`D:\viola-bridge\scripts\live-playout.ps1`，commit 725fd29）。

### 1.2 orender 输出后端现状（本机实测，证据在 `D:\ASRW-development\ab\`）

| 后端 / 设备 | 结果 | 证据 |
|---|---|---|
| `file` → stdout → ffplay | ✅ 可用、已长期在用 | `ab\asio-in-2ch-engine.log` |
| `asio` → viola-bridge ASIO | ✅ 建流成功（3/12/2ch 全过），**但自环自喂、听不到声** | `ab\asio-vb3ch.log:38,46,51` |
| `asio` → ASIO4ALL v2 | ❌ `The requested stream configuration is not supported by the device.` | `ab\asio-3ch.log:38,41` |
| `asio` → Voicemeeter Virtual ASIO | ❌ 3ch：`sample clock or rate cannot be determined`；12ch：`does not support 12 channels`（上限 8ch） | `ab\asio-vm3ch.log:41`、`ab\asio-vm12.log:39` |

**结论：ASIO 输出后端本身没坏**，坏在 `cpal 0.15.3 → 具体 ASIO 驱动`这一层；本机**当前没有能出声的 ASIO 输出设备**。

viola-bridge ASIO 的"自环"来自 `crates/viola_asio/src/driver.rs:33-37,1226-1244`：驱动把宿主写入的 **output** 缓冲半区 Tap 出来，压进 ring，由 `driver.rs:564 pipe_loop` 写回 `\\.\pipe\orender.input`（`lib.rs:89 PIPE_PATH`）——也就是 orender 正在读的那个输入管道。（此条为**强推测**：基于代码架构推理，日志未直接观测到反馈。）

### 1.3 orender 的关键约束

- `--output-backend` 在 Windows 下**只有 `asio` 与 `file`**（`omniphony-renderer/src/cli/command.rs:887-902`），**没有 WASAPI / 系统默认端点**。
- `--output-device` 按**精确名称**匹配（`audio_output/src/cpal_output.rs:193-213`），名称即注册表 `HKLM\SOFTWARE\ASIO` 的子键名。
- **面板拉起的 orender 必须带 `--continuous`**：否则写端一断，orender 读到 EOF 即退出（`src/cli/decode/decoder_thread.rs:506`），不会等新客户端。
- **必须显式 `--bridge-path` 指向 `viola_bridge.dll`**：orender 不自带 bridge，显式路径不存在时直接 bail（`orender_engine/src/bridge_loader.rs:115-124`）。本机 `orender.exe` 同目录**没有** `*_bridge.dll`。
- `--speaker-layout` 是 **YAML 文件路径**，不是通道数。
- `render.binaural.output_mode`（`binaural` / `speaker`）**没有 CLI flag**，只能经 YAML 或 OSC 设置。→ 面板的 `--config` 是设置它的唯一静态手段。
- **改输出设备/后端/采样率若走 OSC 可热改**，但本面板**已定不开 OSC**，因此一律靠**重启 orender**。

### 1.4 重启 orender 是安全的

- orender 是命名管道 **server**（`sys/src/input.rs:201` `CreateNamedPipeW`，`nMaxInstances=255`）。
- viola_asio 驱动是 **client**（`crates/viola_asio/src/pipe.rs:134`），`pipe_loop` 是**无条件 200ms 重连循环**（`pipe.rs:126-140`），无致命分支、无"放弃"标志、不需重调 `start()`。
- 进程退出（含硬杀）由 OS 释放管道句柄，连续启停**不会**出现"管道已存在"（`sys/src/input.rs:856-865` 只有 `CloseHandle`，无 `DisconnectNamedPipe`）。
- **代价**：重启窗口期内 S1 写管道会失败一次（驱动 200ms 后自动重连），表现为**一次断音**。

### 1.5 ASIO 设备枚举与控制面板（SDK 2.3.3，`D:\viola-bridge\third_party\asiosdk`）

- **枚举设备 = 只读注册表，不 LoadLibrary**：`host/pc/asiolist.cpp:8,125,128` 枚举 `HKLM\SOFTWARE\ASIO` 子键；`newDrvStruct`（`:69-95`）读 `clsid` 与 `Description`（显示名，缺省回退键名）。**零副作用**。
- 载入驱动的唯一入口是 `asioOpenDriver`（`asiolist.cpp:171-180`）→ `CoCreateInstance(..., CLSCTX_INPROC_SERVER, ...)`。
- `init(void* sysHandle)` 是 `IASIO` 第 0 个方法（`common/iasiodrv.h:11`）；sysHandle **可传 NULL**。
- 能力查询（`common/asio.h`）：`ASIOGetChannels:574`、`ASIOGetChannelInfo:767-775`、`ASIOGetBufferSize:624`、`ASIOCanSampleRate:645`/`ASIOGetSampleRate:654`、`ASIOGetLatencies:582`。**必须在 init 之后**（`common/asio.cpp` 每个 wrapper 先判 `theAsioDriver`），且在 `ASIOStart` 之前调用合法 ⇒ **只读能力、不建流的进程是干净的**。
- 控制面板：`common/iasiodrv.h:22 virtual ASIOError controlPanel() = 0;`，`common/asio.h:868` 声明 + 注释"If no panel is available ASE_NotPresent will be returned"。`CLSCTX_INPROC_SERVER` ⇒ 窗口建在**调用者进程内**，本面板可直接弹，不需额外 GUI 进程。
- **SDK 不提供任何跨进程互斥**（`asiolist.cpp:174` 的 `asiodrv` 只是进程内链表字段）。第二个进程 init 的后果**由驱动自己决定**。→ 这是面板必须"只在 orender 停止时 init"的根本原因。
- SDK 样例的 `controlPanel` 全是 `return ASE_NotPresent;`（`driver/asiosample/asiosmpl.cpp:478`、`common/asiodrvr.cpp:126`），**无现成面板可抄**。

### 1.6 本机路径（已实测存在）

| 物 | 位置 |
|---|---|
| `orender.exe` | `%LOCALAPPDATA%\Programs\Omniphony Studio\orender.exe`（同目录**无** `*_bridge.dll`） |
| `ffplay.exe` | `%APPDATA%\Smoothie\bin\ffplay.exe`（**在 PATH 上**，裸名 `ffplay` 可解析） |
| `viola_bridge.dll` | `D:\viola-bridge\dist-live\viola-bridge-windows-x86_64\viola_bridge.dll` |
| 管道 | `\\.\pipe\orender.input`（硬编码，不可配） |
| viola-bridge 现有进程管理代码 | **无**（全仓 `Command::new`/`CreateProcessW` 零命中）；仅 PowerShell `Start-Process` 范式 |

## 2. 设计共识（26 条）

| # | 决策 | 结论 |
|---|---|---|
| 1 | 面板改什么 | orender 的**输出后端 + 设备**（S1 的 ASIO 恒为 viola-bridge，不动） |
| 2 | 面板档位 | 只读状态 + 可调 |
| 3 | 界面形态 | 独立 Rust exe，**wry + WebView2**（2026-10-03 修订，见第 9 节） |
| 4 | 阻塞语义 | **非阻塞**；单实例互斥体防重复开窗 |
| 5 | pwsh 退路 | 留（但启动实现在 Rust，脚本只作旁路） |
| 6 | 执行者 | **面板自己**拉起 orender + ffplay；**Apply = 重启 orender** |
| 7 | 实时状态来源 | 不 tail 日志、不加 IPC、**不开 OSC**、不监听 renderer 广播 |
| 8 | 驱动探测时机 | **只在 orender 停止时** |
| 9 | 面板退出 | 杀掉**自己拉起的** orender + ffplay |
| 10 | 控制手段 | **纯重启**（不开 OSC 热改） |
| 11 | 音源 | 不带 feeder（面板只拉 orender + ffplay） |
| 12 | 配置落点 | `%LOCALAPPDATA%\viola-panel\panel.yaml`（免提权；**因独立为新项目已从 viola-bridge 目录改到这里**） |
| 13 | 交付 | 本仓库自己的 CI job 产出 `viola-panel.exe` |
| 14 | 在 S1 内打开 | **本轮不做**，记待办 |
| 15 | `file` 后端下 | 设备区**隐藏 / 置灰** |
| 16 | 窗口布局 | 顶部状态条 + 「输出」「渲染」两组 + 底部固定 Apply |
| 17 | S1 未连接 | 状态条提示，**不报错** |
| 18 | 自动拉起 | **不自动**，按 Start |
| 19 | 启动实现 | Rust `std::process::Command` 自己 spawn；**单测锁 argv 字面量** |
| 20 | 路径定位 | 内置默认探测 + `panel.yaml` 覆盖 |
| 21 | 启动失败报告 | 状态条红字 + 退出码 + **stderr 尾巴** |
| 22 | 采样率/缓冲改谁 | 改 **orender 输出采样率**（`--output-sample-rate`），不动 viola-bridge 驱动参数 |
| 23 | "输出驱动"指谁 | **orender 的输出 ASIO 驱动** |
| 24 | 配置绑定 | 面板用**自有 `--config`** 喂 orender（必须由面板启动 orender 才生效），不改系统 config |
| 25 | 设备下拉 | **只读显示**（注册表枚举），**不逐个探测** |
| 26 | 系统默认端点 | **只读显示**当前默认输出端点 + 提示"要换去 Windows 声音设置"；另：**只 init 当前选中的那一个设备**以显示驱动信息 + 弹其控制面板 |

**注 26 的边界**：不遍历探测全部设备；仅对**当前选中**的设备做一次 `init` → 读能力 → （可选）`controlPanel()`，且**只在 orender 停止时**、**不调用 `ASIOStart`**。

## 3. 界面布局

```
┌─ 状态条 ─────────────────────────────────────────────────────────────┐
│ S1 ─▸ viola_asio ─▸ \\.\pipe\orender.input ─▸ ● orender(PID 1234) ─▸ ffplay ─▸ 默认端点 │
└──────────────────────────────────────────────────────────────────────┘
┌─ 输出 ───────────────────────────────────────────────────────────────┐
│ 后端      [ file ▾ | asio ]                                          │
│ 设备      [ <只读下拉，注册表枚举> ▾ ]        (file 后端 → 整块隐藏)   │
│ 采样率    [ 48000 ▾ ]                                                 │
│ latency   [ 220 ] ms                                                  │
│ 驱动信息  ASIO4ALL v2 · 输出 8ch · 48000 Hz · buffer 256..2048       │
│ 系统默认端点  Speakers (Realtek)  ⓘ 去 Windows 声音设置改             │
│ [ 打开驱动控制面板 ]                                                  │
└──────────────────────────────────────────────────────────────────────┘
┌─ 渲染 ───────────────────────────────────────────────────────────────┐
│ ☑ 启用 VBAP   布局 [ ____________ ] [浏览]                            │
│ 运行统计      可用项：进程存活/退出码、ffplay 队列、面板自配值         │
└──────────────────────────────────────────────────────────────────────┘
[ Start ]                                        [ Apply（重启 orender）]
```

## 4. 运行模型

### 4.1 启动（按 Start）
Rust 侧用 `cmd /c` 拼接管道（与 `live-playout.ps1:93` 同构，理由是避免两进程 stdio 对接在 Windows 上的复杂度）：

```
orender.exe render "\\.\pipe\orender.input" --continuous \
  --bridge-path "<viola_bridge.dll>" \
  --enable-vbap [--speaker-layout "<layout.yaml>"] \
  --output-backend <file|asio> [--output-device "<exact name>"] \
  --output-sample-rate <rate> \
  --output-file - --output-file-format raw-f32 \
  --config "<%LOCALAPPDATA%\viola-panel\panel.yaml>" \
  --loglevel info
| ffplay.exe -nodisp -hide_banner -loglevel info -f f32le -ar <rate> -ch_layout stereo -i -
```

- **argv 字面量由单测锁定**，防与 orender 的 CLI 漂移。
- **调用形态固定为 `cmd.exe /S /C "<line>"`**：`/S` 去掉首尾引号剥离，最外层必须自己
  再包一对引号。不可用 `Command::args` 传 line（Rust 会把 line 自身的 `"` 转义成 `\"`，
  cmd.exe 不认，含空格的 `orender.exe` 路径会在第一个空格处被截断，引擎以 exit 255 秒退）。
  故用 `CommandExt::raw_arg` 逐段传 `/S`、`/C`、`"<line>"`。launcher 单测 `cmd_receives_the_line_with_quotes_intact` 真起 cmd.exe 锁定此点。
- `--output-file -` / `--output-file-format raw-f32` 仅 `file` 后端需要。
- 不把 orender 输出直接写命名管道（`file_sink.rs` 用 `CREATE_ALWAYS`，对管道报 `os error 87`）。
- 子进程 `CREATE_NO_WINDOW`；stdout/stderr 重定向到面板日志文件（供状态条读尾部）。

### 4.2 Apply
1. 写 `panel.yaml`（面板自有的完整输出设置）
2. 杀掉本面板拉起的 orender + ffplay
3. 重新走 4.1

### 4.3 退出
杀掉本面板拉起的 orender + ffplay（**只杀自己的**，按 PID 记录，不按进程名扫）。

### 4.4 配置
`%LOCALAPPDATA%\viola-panel\panel.yaml`，同时承担两个角色：
- 面板自身的持久化（路径、上次选择）
- 作为 `--config` 传给 orender（承载 `render.binaural.output_mode` 等**无 CLI flag** 的项）

### 4.5 路径解析（Q20）
按序探测，`panel.yaml` 可覆盖：`orender.exe` → `%LOCALAPPDATA%\Programs\Omniphony Studio\orender.exe`；`ffplay.exe` → PATH；`viola_bridge.dll` → 已知 dist 路径。

## 5. 设备枚举与驱动信息

| 用途 | 手段 | 副作用 |
|---|---|---|
| 列出设备名 | 读 `HKLM\SOFTWARE\ASIO` 子键 + `Description` | **无**（不 LoadLibrary） |
| 显示驱动信息 | 对**当前选中**设备 `CoCreateInstance` → `init(NULL)` → 读 channels/bufferSize/sampleRate/latencies | 有（驱动被载入本进程），故**只在 orender 停止时** |
| 打开驱动面板 | 上一行的实例上调 `controlPanel()` | 同上；窗口开在本进程内 |

**硬约束**：**不调用 `ASIOStart`**；用完即释放；不在 orender 运行时做。

## 6. 降级与已知代价

1. **无 16 通道电平表 / 无驱动内实时统计**：驱动 DLL 内的 `callbacks`/`worst_gap`/`ring`/`dropped`/`written`/`connected` 全在 DLL 地址空间，独立进程读不到；不 tail 日志、不开 OSC 就无数据源。面板只显示：进程存活/退出码、ffplay 自身队列统计、面板自配值。
2. **binaural 与 16 通道互斥**：binaural 下 `output_channel_count()` 返回 2（`renderer/src/spatial_renderer/mod.rs:1416-1425`），16 通道只在 speaker 模式存在。
3. **Apply 会断音一次**（见 1.4）。
4. **本机没有能出声的 ASIO 输出设备**（见 1.2）→ 设备选择在本机**目前无实用出口**，价值在于将来接物理声卡 + 诊断。
5. **面板显示的采样率是面板自配值**，不是驱动真值（除非在该次探测里读到了）。

## 7. 配置与布局（T1–T3 已定）

| # | 问题 | 结论 |
|---|---|---|
| T1 | 面板与 orender 系统配置的一致性 | **(a)** 面板只写自己管的项（输出后端 / 设备 / 采样率 / binaural 等）；**其余交给 `%ProgramData%\omniphony\config.yaml`**。不镜像全部输出设置。 |
| T2 | 布局 YAML 从哪来 | **(a)** 面板**自带一份 7.1.4（12ch）布局 YAML** 作默认；`panel.yaml` 可指别处。 |
| T3 | 配置文件数量 | **(a)** **一个文件两用**：`panel.yaml` 既是面板自身设置，又整份作为 `--config` 传给 orender；面板忽略自己不认识的键。 |

**T1 的已知后果（接受）**：面板未传的项会落到 orender 的系统配置——即“面板管输出侧少数项、系统 config 管其余”。**代价**：若你在外部改了系统 config 的输出项，面板不会同步显示真值。

## 8. 实现计划

### M0 骨架
- 新仓库 `D:\viola-panel`（独立 git），`Cargo.toml`：`wry`、`tao`、`serde`、`serde_json`、`serde_yaml_ng`（COM/注册表/MMDevice 为手写 FFI，不引 `windows` crate）
- 单实例互斥体、窗口骨架、日志文件
- **验收**：`cargo build` 出 exe，双击开窗，再双击不开第二窗

### M1 路径解析 + 配置读写
- 按 4.5 探测，`panel.yaml` 读写
- **验收**：单测覆盖"探测优先级"与"配置覆盖探测"

### M1.5 自带布局资源（T2）
- 随 exe 内置一份 7.1.4（12ch）布局 YAML（作 `--speaker-layout` 默认值），`panel.yaml` 可覆盖路径
- **验收**：默认路径存在且可被 orender 加载（跑一次无 layout 相关报错）

### M2 设备枚举（只读）
- 读 `HKLM\SOFTWARE\ASIO` 列设备名 + Description
- **验收**：单测/快照断言本机三个设备名与 `orender list-asio-devices` 输出一致

### M3 启动器（核心）
- 按 4.1 拼 argv 并 spawn `cmd /c ... | ...`；记录 PID；捕获 stdout/stderr
- **验收**：单测锁 argv 字面量；本机手动跑一次能出声（经 ffplay）

### M4 驱动信息 + 控制面板（受限探测）
- 对当前选中设备 `init(NULL)` → 读能力 → `controlPanel()`
- **验收**：本机对 ASIO4ALL v2 读出通道/采样率；按钮能弹出面板

### M5 状态条 + Apply 重启
- 按 4.2 / 4.3；失败按 Q21 显示退出码 + stderr 尾巴
- 界面层按第 9 节重写为 WebView + 静态前端（替代原 egui 渲染）
- **验收**：手动跑 Apply，观察 S1 断音一次后自动恢复

### M6 CI
- 本仓库自己的 GitHub Actions job（windows-latest）：`cargo test` → `cargo build --release` → `Get-FileHash` → `upload-artifact`
- **验收**：云端产出 `viola-panel.exe`，本机只下载不编译

### 待办（不在本轮）
- **D1：已完成（2026-10-03，viola-bridge `f0c8eca`）**。`crates/viola_asio/src/driver.rs` 的 `asio_control_panel` 现在用 `GetModuleHandleExW` + `GetModuleFileNameW` 取本 DLL 路径，在同目录找 `viola-panel.exe`；找不到就 `ASE_NotPresent`，找到则先 `FindWindowW(NULL, "viola-panel")`，已开着就 `ShowWindow(SW_RESTORE)` + `SetForegroundWindow`，否则 `ShellExecuteW` 拉起（面板有单实例互斥体，重复启动会静默退出，所以必须先在窗口层判断）。`scripts/register-asio.ps1` 新增 `-PanelExe` 参数把 exe 拷到 `C:\ProgramData\viola-asio\`。真机验证：Studio One 7 加载了新版 DLL，点击「控制面板」按钮面板正常弹出。

## 9. 界面层修订（2026-10-03）：egui → WebView

> 修订原因：用户要求「用 transitions.dev 组件库重写 UI 使其更漂亮」。transitions.dev 只发布
> CSS + React 片段（**无 Rust/egui 目标**），逐字使用需要能渲染 CSS 的宿主；且 egui 默认字体
> 无 CJK 字形，本机中文端点名已实测渲染为空白框（`docs/img/m5-window.png`）。
> 用户裁定改用 WebView 渲染层。

### 9.1 取代第 3 条共识

| 项 | 原 | 现 |
|---|---|---|
| 窗口层 | `eframe`/`egui` 0.36 | `wry` + `tao` |
| 前端 | Rust 内联渲染 | 静态 `index.html` + `styles.css` + `app.js` |
| 通信 | 同进程直接调用 | `serde_json` over wry IPC，命令名见 9.3 |
| 序列化 | 仅 `serde_yaml_ng` | `serde_yaml_ng` + `serde_json` |

**不选 Tauri**：其 CLI/打包生态习惯走 npm，本机无 node；本面板仅约 8 个数据绑定，
Tauri 的插件体系与打包器换不回其复杂度。`wry` 是 Tauri 的底层，只加两个 crate。
**不选「Rust 起 localhost HTTP 服务 + 系统浏览器」**：会把「独立桌面程序」降级为
「后台进程 + 浏览器标签页」，与第 4 条「非阻塞独立 exe」的精神不符，且引入端口与进程管理。

### 9.2 前端零构建

前端是三个静态文件，以 `include_str!` 编入 exe，**无 npm、无 node_modules、无 bundler、
无构建步骤**。CI 仍是 `cargo test` + `cargo build` 两步。理由：transitions.dev 的规范本身
即「复制粘贴、不引入动效库」（其 SKILL.md 明写 `don't pull in a motion library`），
且其 React 变体是给已有 React 项目用的。

### 9.3 IPC 契约

前端 → Rust，单通道 JSON，命令名固定如下：

| 命令 | 载荷 | 返回 |
|---|---|---|
| `get_state` | 无 | 全量状态快照 |
| `save` | `PanelSettings` | `ok` / 错误串 |
| `start` | 无 | 同上 |
| `stop` | 无 | 同上 |
| `apply` | 无 | 同上（= 保存 + 重启） |
| `probe_driver` | 无 | `ok`（结果经 `get_state` 读回） |
| `open_driver_panel` | 无 | 同上 |
| `read_engine_log` | 无 | 日志尾部字符串 |

**轮询而非推送**：前端每 500 ms 调一次 `get_state`，与既有
`request_repaint_after(500ms)` 同构。耗时操作（驱动探测、控制面板）仍在独立线程执行，
前端轮询读回结果 —— 既有 `mpsc` + `try_recv` 的并发模型不变。

### 9.4 冻结与开口

- **冻结**：第 2 节 26 条共识中除第 3 条外的全部内容；`config` / `launcher` /
  `registry` / `asiodrv` / `mmdev` 五个模块的**函数签名与行为**一律不动。
- **开口（仅此一处）**：给 `DriverInfo` 与 `AsioDevice` 补 `Serialize` 派生（纯增量，
  为过 IPC），并新增 `serde_json` 依赖。原有 26 个单测均不引用 egui，全部保持通过
  （本轮在 `main.rs` 另增 3 个，总计 29 个）。
- 被删除的是 `main.rs` 的 egui 渲染代码与 `eframe` 依赖；`main.rs` 自身的
  `acquire_single_instance` / `log_tail` 辅助函数保留。
- **例外增开（真机测试发现）**：`launcher.rs` 的 `cmd /S /C` 引号传递修正（见 4.1），
  函数签名未变，新增 1 个真起 cmd.exe 的单测（共 30 个）。

### 9.5 视觉方向（Q5 裁定）

**插入式机架单元 + 信号链**。面板的本质是
`S1 → viola_asio → pipe → orender → ffplay → 默认端点` 这条链上的一个可配置插入点。

- 顶部状态条改为**真实信号链示意图**，标出本面板所处位置与当前断点。
- 分区呈机架丝印感；阳极氧化深灰褐底（非纯黑）；琥珀色 LED 表示已启用，信号绿表示流通。
- 数值一律等宽（`48000`、PID、buffer 区间）。
- 动效遵循 transitions.dev 的 token 刻度：时长 7 级
  （stagger 40 / micro 80 / quick 150 / fast 250 / medium 350 / slow 400 / very-slow 500 ms），
  缓动以 `cubic-bezier(0.22, 1, 0.36, 1)` 为主，开慢关快（面板 400→350、toast 350→250）。
- **必须保留 `prefers-reduced-motion: reduce` 守卫**（transitions.dev SKILL.md 要求；
  注意其 `_root.css` 本身不含守卫，守卫需逐片段带上）。

### 9.6 本次修订的已知代价

本机**没有 Rust 工具链且不安装**（用户裁定走 CI）。因此 Rust 侧（窗口、IPC、编译）
只能在 CI 验证；前端可在本机 Edge 中带桥接桩独立验证并截图。若 CI 连续两轮卡在同一
错误，需回头重新讨论工具链。

## 10. 许可证

**GPL-3.0-or-later**（用户裁定）。**已知后果：本面板不能闭源商用。**

本面板不复制、不链接 viola-bridge 代码，仅通过进程边界调用（spawn `orender.exe`、跑 `ffplay`、读写自有 YAML），因此本可自选宽松许可；选 GPL 是用户的明确决定。

## 11. 与 viola-bridge 的边界

| 允许 | 禁止 |
|---|---|
| spawn `orender.exe` / `ffplay.exe` | copy viola-bridge 源码 |
| 读写自有 `%LOCALAPPDATA%\viola-panel\panel.yaml` | 链接 `viola_asio.dll` / `viola_bridge.dll` |
| 读 `HKLM\SOFTWARE\ASIO`（公共契约） | 改 `%ProgramData%\omniphony\config.yaml` |
| 用 `viola_bridge.dll` 作为 orender 的 `--bridge-path` 参数值 | 从 viola-bridge 仓库引入 CI/构建配置 |

## 附：ASIO 商标

界面需显示：`ASIO is a trademark and software of Steinberg Media Technologies GmbH`
