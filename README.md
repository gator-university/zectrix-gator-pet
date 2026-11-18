# 鳄鱼大宠物

**把 DeepSeek Harness 的实时状态推到桌角那块墨水屏上——抬头就能看见它在干活、在等你，还是要你拍板。**

一只像素小鳄鱼，跟着 DSH 的会话状态换动作。它是 **DeepSeek Harness 插件**：
DSH 打开它就起来，DSH 关闭它就退出，设置页就在 DSH 的 Plugins 里，没有别的常驻程序。

`Windows 10 / 11 x64` · `v0.7.2` · `MIT` · DSH `>= 0.2.1-alpha.2` · ZECTRIX Note 4（400×300 单色）

<p align="center">
  <img src="docs/screens/working.png" width="420" alt="设备上的一帧：单任务详情，右下角是鳄鱼大宠物">
</p>

<p align="center"><sub>400×300 的实际排版（<code>dsh-pet preview</code> 渲染）：单任务详情页，右下角是 gator</sub></p>

## 它能做什么

- **自己跟着 DSH 走**——插件在 DSH 启动时拉起宠物进程，DSH 退出就收尾；被强杀时宠物 10 秒内自检退出，不占着设备。
- **多任务并列**——1 个任务看详情，2 个任务各给两行，3 个以上分页紧凑列表；等待你处理的永远排第一页。
- **一眼看懂状态**——正在工作 `●`、等你处理 `！`、任务完成（矢量对勾）、遇到问题 `×`、已停止 `■`。
- **一只鳄鱼六个动作**——待命、干活、等你、完成、出错、睡觉；完成画面停 5 秒再回待命。
- **低闪**——默认 mono 单色刷新（1–2 秒/帧），状态变化合并推送，最少 3 秒间隔。
- **花絮可换**——右下角句子在 `assets/quips.txt` 里，一行一句，改完重启宠物即生效。
- **提示音**——进入「等你处理 / 任务完成」时响一次，设备双击确认键可静音。

## 安装

### 前置条件

1. **DSH 版本**：`>= 0.2.1-alpha.2`（插件需要 bundle 与 client 能力）。
2. **设备固件 ≥ 3.7.5**：本地推送是 NOTE4 3.7.2 才引入的协议；3.6.5 及更早**不支持**。
   官方 3.7.5 完整包（5,014,576 字节，SHA-256
   `212b467dbfa0f539bb041fb3d7240d7faaad6df6fc534c2e9ee8b6e99a14aa4f`）：
   <https://wiki.zectrix.com/downloads/firmware/full_3.7.5.bin>
   刷写用 Chrome / Edge 打开 <https://www.zectrix.com/firmware-updater.html>，刷完重新配网。
3. **同一局域网 + 待机保持 Wi-Fi**：设备「设置 → 网络同步 → 待机保持 Wi-Fi」必须打开，
   否则空闲约 2 分钟就下线，推送一定失败。本机 mDNS 被防火墙拦，所以插件用 IP 直连
   （默认 `192.168.1.162`，在设置页里改）。

### 三步

1. DSH → 设置 → **Plugins** → 安装插件：
   - 装发布版：填仓库地址 `https://github.com/gator-university/zectrix-croc-pet`；
   - 本机开发：选本地目录 `E:\myAIProjects\zetrix-dsh-screen\plugin`（`link:` 安装）。
2. **重启 DeepSeek Harness**——宿主插件与设置页都在启动时加载。
3. 打开 设置 → **Plugins → 鳄鱼大宠物**：填设备 IP，点「测试连接」，保存。
   第一次保存后 ≈2 秒，设备就会开始跟。

> 插件自带 `bin/dsh-pet.exe`，不依赖 .NET / VC++ / Rust 运行时，也不会写 DSH 的数据目录。

## 设置页

设置页在 DSH 的 Plugins 里（和官方插件同一个入口），字段：

| 字段 | 说明 |
| --- | --- |
| 跟随 | 关闭后立即释放设备，回设备自己的默认界面 |
| 模式 | 实时 / 平衡 / 安静 / 极简（控制细节刷新合并间隔） |
| 提示音 + 音量 | 进入「等你处理 / 任务完成」时响一次，0–100 |
| 设备 IP | 支持「测试连接」，直接返回设备是否可达与电量 |
| 刷新格式 | mono（默认，约 1–2 秒/帧）/ gray4（约 3.5 秒）/ gray16（约 5 秒） |

保存后写入 `%LOCALAPPDATA%\zectrix-dsh-pet\pet-config.json`，宠物进程每 2 秒热读，**不用重启 DSH**。

## 设备手势

| 手势 | 动作 |
| --- | --- |
| 单击确认 | 回首页（不批准任何操作） |
| 双击确认 | 静音 / 恢复提示音（屏幕右下角即时反馈） |
| 三击确认 | 退出到设备普通界面（跟随暂停并释放设备） |
| 单击上 / 下 | 在多任务之间切换 / 翻页 |
| 长按 | 按规格无操作 |

恢复跟随：设置页把「跟随」重新打开，或 DSH 重启后插件重新拉起。

## 鳄鱼素材

- 路径：`assets/croc/default/{action}.png`
- `action`：`idle`、`working`、`waiting`、`done`、`error`、`sleep`
- 规格：90×120 单色 PNG（L 模式，0 墨 / 255 纸），按 1:1 直接画在右下角 `(300,152)`，不再放大
- 风格基线：gator（短吻鳄）——圆头、短宽 U 吻、2px 实心描边、白高光眼睛、浅色肚皮分节线
- 今后装扮：`assets/croc/<outfit>/<action>.png`，默认 `default`；可选第二帧 `<action>-2.png`（v1 不用）
- 六张母版在 `art/gen-v3/`（gpt-image-2 / image edit 生成，同一角色），转换脚本 `scripts/prep_croc_art.py` 统一裁剪 → 90×120 二值化，并顺带生成 `plugin/icon.png`
- 替换形象：用同尺寸、同路径的 6 个文件覆盖即可，不用改代码（契约见 `docs/adr/0007-croc-asset-contract-90x120.md`；素材来源见 `docs/adr/0008-ai-generated-croc-art.md`）

## 花絮（右下角句子）

`assets/quips.txt`（源码）与 `plugin/bin/quips.txt`（安装副本）：UTF-8，一行一句，≤20 字；`#` 开头的行忽略。文件缺失或为空时用内置池。改安装副本后重载插件生效。
改完后重启宠物进程（或重载插件）生效。

## 屏幕什么时候亮

| 情况 | 设备 |
| --- | --- |
| DSH 在线、没有任务 | 保持设备默认界面，**不占用、不刷新** |
| 有任务在跑 / 等你处理 | 立即占用并显示 |
| 任务完成 | 显示完成画面 5 秒，然后回待命；无后续任务按 5 分钟宽限交还 |
| 跟随关闭 / 三击退出 | 立即释放，回默认界面 |

限制：同一时刻只有一个程序能占设备（固件约束）。宠物占着时，请先三击退出再试别的。

## 排错

**测试连接失败 / 日志「没有发现可用设备」**
1. 设备与电脑在同一 2.4 GHz 网络，且「待机保持 Wi-Fi」已开；
2. 用 IP 而不是 mDNS：设置页填 `192.168.1.x`；
3. 设备刚被别的程序占用，等约 8 秒再试。

**设备连着但画面不变**
- 看日志 `%LOCALAPPDATA%\zectrix-dsh-pet\pet.log`；
- 确认 DSH 里确实有 `正在工作 / 等你处理` 的会话；
- 确认「跟随」开着。

**DSH 退出后宠物还在**
- 正常退出会由插件清理；被强杀时宠物 10 秒内自检退出（`--watch-pid`）。
- 若还残留，任务管理器结束 `dsh-pet.exe` 即可；下次 DSH 启动会再拉起。

**装完插件在 Plugins 里看不到**
- 必须**重启 DSH**（宿主插件与 client 都在启动阶段加载）；
- 确认 `~/.dsh/profiles/desktop/package.json` 的 `dsh.profile.bundles` 里有 `zectrix-croc-pet`。

## 隐私边界

- 只**读** `~/.dsh/sessions` 下的会话日志（状态、标题、token 用量），不写 DSH 数据；
- 会话标题会显示在墨水屏上，内容不会离开本机；
- 与设备只走局域网 UDP（32342），无云端、无账号。

## 从源码构建

```powershell
cargo test                 # 54 lib + 15 bin = 69
cargo build --release      # target\release\dsh-pet.exe
copy target\release\dsh-pet.exe plugin\bin\dsh-pet.exe
```

要求 Rust ≥ 1.80（MSVC 工具链）。预览排版不连设备：

```powershell
dsh-pet preview --demo working --out out\working.png
```

## 目录结构

```
plugin\             DSH 插件包（package.json / index.js / client.js / bin / locales / icon）
src\                Rust：dsh.rs 会话跟随、layout.rs 版式、sprite.rs 鳄鱼、raster.rs 绘制、
                    voice.rs 语音诊断、device.rs 设备通道、model.rs 状态模型
assets\croc\        鳄鱼动作 PNG（default 装扮）
assets\quips.txt    右下角花絮
docs\SPEC.md        功能清单与逐项验收（唯一功能规格）
docs\adr\           架构决策记录（生命周期、命名、素材、刷新、设置页、花絮、90×120 素材契约）
docs\screens\       版式截图（live 双任务 / working / waiting / done / error / stopped / header 头部特写）
GLOSSARY.md         术语表
```

## 归属

本项目是 [MuskCheng/zectrix-workbuddy-pet](https://github.com/MuskCheng/zectrix-workbuddy-pet) 的 fork：
原项目跟随 WorkBuddy，本 fork 改为跟随 **DeepSeek Harness** 并改成插件形态。
原项目 MIT，Copyright (c) 2026 MuskCheng；本 fork 修改部分同 MIT，版权声明见 [LICENSE](LICENSE)。
