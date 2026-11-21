# Freenapse-DeathAdder V4 Pro

> **雷蛇雷云 4（Razer Synapse 4）超轻量板载替代工具 · 炼狱蝰蛇 V4 Pro（Razer DeathAdder V4 Pro / DAV4 Pro）专属定制版**  

本项目在原版 [freenapse](https://github.com/MatthewSaintBull/freenapse) `0.5.2` 的基础上，增加适配**炼狱蝰蛇 V4 PRO（Razer DeathAdder V4 Pro）**，并作出些许优化，由 **Gemini 4 Carbon** 和 **ChatGPT 6 Astra PRO** 共同完成修改。

个人使用，分享出来，省一省大家的token。

👉 **[前往 Releases 下载单文件 `freenapse.exe` 即开即用](https://github.com/TRAAACER/Freenapse-DeathAdder-V4-Pro/releases/latest)**

---

## 一、代替雷云 4（Razer Synapse 4 平替 / 免雷云板载驱动）

如果你厌倦了官方雷云 4 臃肿的后台全家桶、强制联网账号与资源占用，可以用这一个 `1.0 MB` 的原生小工具替代它：

| 对比项 | 官方雷云 4 (Razer Synapse 4) | 本项目 (Freenapse) |
| :--- | :--- | :--- |
| **安装与体积** | 数百 MB 安装包 + 多组件系统服务 | **单文件 `1.0 MB` 绿色免安装**，双击即用 |
| **后台进程与内存** | 10+ 个进程与服务，常驻约 `1 GB+` 内存 | **仅 1 个托盘进程，开机常驻私有内存 `< 2 MB`**（设置完也可直接退出） |
| **账号与联网** | 需登录雷蛇账号、持续后台联网 | **免登录、零联网、零遥测** |
| **配置保存方式** | 依赖后台常驻同步 | **100% 直写鼠标板载内存（Onboard Memory）**，关闭软件或换电脑依然生效 |
| **游戏与反作弊表现** | 后台持续占用 CPU，偶发冲突 | 进出游戏切换配置为**纯事件驱动（零轮询）**；无宏功能，**零全局键盘钩子、零 `SendInput` 模拟输入** |

- **桌面省电、进游戏无感秒切**：支持按游戏进程/目录自动切换专属回报率、DPI 档位与侧键映射（`80 ms` 事件防抖 + 后台增量直写，进出全屏游戏丝滑无感切换）。
- **极简托盘与静默自启**：支持一键开启「开机自启」，开机静默驻留右下角托盘显示实时电量百分比；精简四行右键菜单（实时状态摘要 / 打开控制中心 / 开机自启 / 退出），小而美不打扰。

## 二、针对炼狱蝰蛇 V4 Pro（DeathAdder V4 Pro）的专属支持

针对 **Razer DeathAdder V4 Pro（有线 `1532:00BE` / 无线 `1532:00BF`）** 完成了完整硬件协议解锁与界面定制：

- **满血性能与板载改键**：支持有线 / 无线双 **8000 Hz** 原生回报率、**100 – 45,000 DPI**（最多 5 档板载切换与 X/Y 轴独立灵敏度）、动态灵敏度自定义曲线、传感器握姿角度校准（`-30° ~ +30°`）与板载按键重映射（含 Hypershift 层）。
- **二代半球形接收器（HyperSpeed Gen-2 Dome Dongle）控制**：支持读取接收器固件与序列号，独立设置接收器上**左 / 中 / 右 3 颗状态指示灯**（电量、连接状态、回报率、DPI 档位、关闭）及 **5 档亮度**。
- **Focus Pro 45K 二代传感器校准**：完整支持智能追踪（`1 / 2 / 3 mm`）与**非对称抬起 / 着陆截断等级**（抬起 `2..26`、着陆 `1..25`）双向读写。
- **侧键一键「显示桌面」**：内置「桌面快捷」预设，可将侧键直接写入板载 `显示桌面`（`Win+D`）、任务视图、截图、锁屏、虚拟桌面切换等系统组合键，无需后台软件即可触发。

## 三、相比原版 `0.5.2` 的改进与删改说明

1. **解锁炼狱蝰蛇 V4 Pro 完整读写**：验证并启用 `1532:00BE`（有线）与 `1532:00BF`（无线）双模 8K 回报率、45K DPI、板载配置及 `02/8c` 按键回读协议兼容。
2. **新增二代半球形接收器协议**：实现 `tid 0xff` 下固件/序列号读取、3 颗指示灯独立映射（`07/15`、`07/95`）与亮度调节（`07/16`、`07/96`），并加入单灯异步读取保护防止覆盖其余灯位。
3. **适配 V4 Pro 传感器非对称高度回读**：解析 `0b/85/0b` 报文校验抬起/着陆等级，实现配置一致时零重复 `SET` 写入，同时兼容旧机型写入策略。
4. **移除宏功能，回归 100% 纯板载**：彻底删除宏录制/播放、软件启动程序、文本输入以及多余的「纯板载反作弊模式」开关，全工程完全去除 `WH_KEYBOARD_LL` 全局键盘钩子与 `SendInput` 模拟输入，单文件体积进一步缩减至 `1.0 MB`，对各类游戏反作弊彻底透明。
5. **异步增量切屏、确定性 DPI 跟随与防卡键优化**：采用 `SetWinEventHook` 零轮询事件通知 + 独立后台设备工作线程，支持快速切屏**最新目标合并抢占**、**`HwCache` 字段级增量写入**（进出游戏双向零冗余读取，仅写变动项）、**游戏/桌面 DPI 档位与实时传感器 XY 值双重确定性同步**、**仅响应雷蛇 `VID_1532` 热插拔**（不受耳机/手柄等无关 HID 外设干扰）以及 **`Win+D` 等组合键松开静默护栏**（彻底解决切屏瞬间按侧键导致的修饰键粘连）。
6. **全中文界面与极简托盘菜单**：重绘 V4 Pro 右手人体工学俯视图与底部按键，新增板载「桌面快捷」分类与「开机自启」开关；大幅精简托盘右键菜单，仅保留状态摘要、打开控制中心、开机自启与退出四项核心操作。
7. **健壮性与隐私保护**：默认关闭启动外网更新检查；完善瞬态 HID 读失败错误传播，默认对界面与命令行的鼠标序列号及系统用户目录路径脱敏。

**开源协议**：基于原项目 [MatthewSaintBull/freenapse](https://github.com/MatthewSaintBull/freenapse) 修改，遵循 [MIT License](LICENSE)（独立开源项目，与 Razer Inc. 无关联）。

---

# Freenapse-DeathAdder V4 Pro

> **Lightweight Razer Synapse 4 Onboard Alternative · Custom Edition for Razer DeathAdder V4 Pro (`DAV4 Pro`)**  
> *(Note: The GUI and tray menu in this custom build are localized in Simplified Chinese; `config.toml` and CLI commands remain standard ASCII/English.)*

Built upon upstream [freenapse](https://github.com/MatthewSaintBull/freenapse) `v0.5.2`, this project adds full hardware support for the **Razer DeathAdder V4 Pro** along with background performance and anti-cheat safety optimizations, co-developed with **Gemini 4 Carbon** and **ChatGPT 6 Astra PRO**.

shared simply for everyone to use and save tokens.

👉 **[Download standalone `freenapse.exe` from Releases](https://github.com/TRAAACER/Freenapse-DeathAdder-V4-Pro/releases/latest)**

---

## 1. A Lightweight Razer Synapse 4 Alternative

Replace Razer Synapse 4's multi-process background suite and mandatory cloud login with a single `1.0 MB` native Win32/Rust executable:

| Feature | Razer Synapse 4 | Freenapse-DeathAdder V4 Pro |
| :--- | :--- | :--- |
| **Footprint & Install** | Hundreds of MBs + background Windows services | **Single `1.0 MB` portable `.exe`**, no installer |
| **Background Processes & RAM** | 10+ processes, `1 GB+` RAM | **1 tray process, `< 2 MB` startup private RAM** (or `0` if closed after configuring) |
| **Account & Network** | Requires Razer ID & internet | **No account, zero network requests, zero telemetry** |
| **Storage** | Requires background software sync | **100% onboard memory**, persists across reboots & PCs |
| **Gaming & Anti-Cheat** | Constant background CPU activity | **Event-driven** game detection (zero polling) + **no macros, zero input hooks, zero `SendInput`** |

- **Seamless Per-Game Profile Switching**: Asynchronous differential switching (`80 ms` event debounce + cached field-level writes) applies game-specific polling rate, DPI stage, and button bindings smoothly without blocking or stuck modifiers.
- **Minimalist Tray & Silent Autostart**: Launches silently into the Windows system tray at login with a live battery percentage icon and a clean 4-item context menu.

## 2. Tailored for Razer DeathAdder V4 Pro (`DAV4 Pro`)

Full protocol support unlocked and verified on **Razer DeathAdder V4 Pro (`1532:00BE` wired / `1532:00BF` wireless)**:

- **Full Performance & Onboard Remapping**: Native wired & wireless **8000 Hz** polling rates, **100 – 45,000 DPI** (up to 5 stages, independent X/Y), custom Dynamic Sensitivity splines, sensor angle rotation (`-30° .. +30°`), and onboard button remapping (including Hypershift).
- **HyperSpeed Gen-2 Dome Dongle Support**: Reads dongle firmware/serial and configures the **Left / Center / Right 3 status LEDs** independently (`Battery`, `Connection`, `Polling Rate`, `DPI Stage`, or `Off`) plus **5 brightness levels**.
- **Focus Pro 45K Gen-2 Calibration**: Supports Smart Tracking (`1 / 2 / 3 mm`) and **Asymmetric Cut-off** (`2..26` lift-off / `1..25` landing) read & write.
- **Onboard Windows Desktop Shortcuts**: One-click presets to bind side buttons directly in onboard memory to `Show Desktop` (`Win+D`), Task View, Screenshot, Lock PC, Virtual Desktop switching, and media controls — working even when the app is not running.

## 3. Summary of Improvements & Changes over Upstream `v0.5.2`

1. **DeathAdder V4 Pro Verification**: Unlocked full read/write support for wired (`1532:00BE`) and wireless (`1532:00BF`) models, including 45K DPI, dual 8000 Hz polling, and `02/8c` button readback compatibility.
2. **HyperSpeed Gen-2 Dome Dongle Protocol**: Implemented `tid 0xff` firmware/serial queries, 3-LED slot mapping (`07/15`, `07/95`), brightness control (`07/16`, `07/96`), and single-slot read-before-patch protection.
3. **Focus Pro Gen-2 Asymmetric Readback**: Added `0b/85/0b` lift/land level readback verification for zero redundant `SET` writes on V4 Pro while preserving pre-write compatibility for older models.
4. **Removed Macros for Pure 100% Onboard Operation**: Completely removed macro recording/playback, software app launch / text typing, `WH_KEYBOARD_LL` keyboard hooks, and `SendInput` injection — shrinking the binary to `1.0 MB` with zero input-path overhead.
5. **Fast Non-Blocking Differential Profile Switching**: Event-driven foreground detection (`SetWinEventHook`) paired with a dedicated background HID worker featuring latest-target preemption, `HwCache` field-level differential writes, deterministic game/desktop DPI stage + live XY sensor synchronization, `VID_1532`-scoped hotplug filtering, and a shortcut-release fence (`Win+D` stuck-key prevention).
6. **Ergonomic UI & Minimalist Tray Menu**: Re-illustrated right-handed DeathAdder V4 Pro top/bottom views, added onboard Windows shortcut presets, and streamlined the tray right-click menu to four essentials (status summary, open settings, autostart, quit).
7. **Resilience & Privacy**: Disabled startup update checks by default, propagated transient HID read errors cleanly, and masked hardware serial numbers and `%USERPROFILE%` paths by default.

**License**: Based on [MatthewSaintBull/freenapse](https://github.com/MatthewSaintBull/freenapse), released under the [MIT License](LICENSE). Independent open-source project, not affiliated with Razer Inc.
