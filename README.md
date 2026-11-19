# Freenapse-DeathAdder V4 Pro

> **雷蛇雷云 4（Razer Synapse 4）超轻量板载替代工具 · 炼狱蝰蛇 V4 Pro（Razer DeathAdder V4 Pro / DAV4 Pro）专属定制中文版**

本项目在原版 [freenapse](https://github.com/MatthewSaintBull/freenapse) `0.5.2` 的基础上，增加适配**炼狱蝰蛇 V4 PRO（Razer DeathAdder V4 Pro）**，并作出些许优化，由 **Gemini 4 Carbon** 和 **ChatGPT 6 Astra PRO** 共同完成修改。

后续没啥恶性 Bug 的话，不打算更新，单纯分享使用，省一省大家的token。

👉 **[直接前往 Releases 下载单文件 `freenapse.exe` 即开即用](https://github.com/TRAAACER/Freenapse-DeathAdder-V4-Pro/releases/latest)**

---

## 一、代替雷云 4（Razer Synapse 4 平替 / 免雷云驱动）

如果你厌倦了官方雷云 4 臃肿的后台进程、联网登录与资源占用，可以用这一个 `1.2 MB` 的原生小工具彻底替代它：

| 对比项 | 官方雷云 4 (Razer Synapse 4) | 本项目 (Freenapse) |
| :--- | :--- | :--- |
| **安装与体积** | 数百 MB 安装包 + 多组件服务 | **单文件 `1.2 MB` 绿色免安装**，双击即用 |
| **后台进程与内存** | 10+ 个进程与系统服务，常驻约 `1 GB+` 内存 | **仅 1 个托盘进程，常驻私有内存 `< 2 MB`**（甚至设完可直接退出） |
| **账号与联网** | 需登录雷蛇账号、后台联网 | **免登录、零联网、零遥测** |
| **存储方式** | 依赖后台常驻同步 | **直写鼠标板载内存（Onboard Memory）**，关掉软件或换电脑依然生效 |
| **后台与反作弊表现** | 持续后台占用，偶发掉帧或冲突 | 前台切游戏采用 `SetWinEventHook` **零轮询事件驱动**；纯板载键位下**零全局键盘钩子驻留**，并提供「纯板载反作弊模式」总开关 |

- **平时省电、进游戏满血**：支持按游戏进程/目录自动切换回报率、DPI 档位与专属侧键方案（窗口焦点切换后 `120 ms` 自动生效，退出游戏自动恢复桌面方案）。
- **无感驻留**：支持开启「开机静默自启」，开机直接静默缩进右下角托盘显示实时电量百分比，不弹主窗口。

---

## 二、针对炼狱蝰蛇 V4 Pro（DeathAdder V4 Pro）的专属适配与优化

原版 `0.5.2` 尚未解锁炼狱蝰蛇 V4 Pro 写入且不支持二代半球形接收器。本版本针对 **Razer DeathAdder V4 Pro（有线 `1532:00BE` / 无线 `1532:00BF`）** 完成了完整协议适配与体验打磨：

- **满血性能与板载改键**：完整解锁有线 / 无线双 **8000 Hz** 原生回报率、**100 – 45,000 DPI**（支持最多 5 档切换与 X/Y 轴独立灵敏度）、动态灵敏度自定义曲线、传感器握姿角度校准（`-30° ~ +30°`）、板载按键重映射（含 Hypershift 第二层）与板载宏存储。
- **HyperSpeed 二代半球形接收器（Gen-2 Dome Dongle）支持**：支持读取接收器固件与序列号，可独立自定义接收器上**左 / 中 / 右 3 颗状态指示灯**（电量状态、连接状态、回报率、DPI 档位、关闭）及 **5 档亮度**。
- **Focus Pro 45K 二代传感器高度校准**：完整支持智能追踪（`1 / 2 / 3 mm`）与**非对称抬起 / 着陆截断等级**（抬起 `2..26`、着陆 `1..25`），适配 V4 Pro 固件 `0b/85/0b` 精准回读比对，配置一致时零重复板载写入。
- **全中文右手人体工学界面 + 侧键「显示桌面」**：
  - 全中文控制中心与托盘右键菜单，重绘炼狱蝰蛇 V4 Pro 右手人体工学俯视图与底部 DPI / 电源键交互，修复分类标签溢出；
  - 新增「桌面快捷」预设，支持将鼠标侧键一键写入板载 **`显示桌面`（`Win+D`）**、任务视图、截图、锁屏、虚拟桌面切换等系统组合键，无需雷云后台也能随时生效。

---

## English Summary (Search Reference)

**Freenapse-DeathAdder V4 Pro** is a tiny (`~1.2 MB`), standalone, native Win32/Rust **Razer Synapse 4 alternative** tailored for the **Razer DeathAdder V4 Pro (`1532:00BE` wired / `1532:00BF` wireless, DAV4 Pro)** on Windows, built upon [freenapse](https://github.com/MatthewSaintBull/freenapse) `v0.5.2`:
- Writes DPI (up to 45,000), native wired/wireless 8000 Hz polling rate, Focus Pro Gen-2 Smart Tracking & Asymmetric Cut-off (`0b/85/0b` readback), Dynamic Sensitivity curves, and button/shortcut mappings directly to **onboard memory**.
- Full **HyperSpeed Gen-2 Dome Dongle** support: firmware/serial readout, individual Left / Center / Right 3-LED indicator mapping, and brightness control.
- Event-driven per-game profile switching (`SetWinEventHook`, zero background polling), `< 2 MB` RAM tray footprint, and anti-cheat-friendly pure-onboard mode.

---

## 开源协议

基于原项目 [MatthewSaintBull/freenapse](https://github.com/MatthewSaintBull/freenapse) 修改，遵循 [MIT License](LICENSE)（独立开源项目，与 Razer Inc. 无关联）。
