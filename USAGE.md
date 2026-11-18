# 鳄鱼大宠物 · 使用手册

面向两种人：日常只通过 **DSH 设置页**和**设备按键**使用的用户，以及需要用命令行做验收/排错的开发者。
功能清单与验收状态见 [docs/SPEC.md](docs/SPEC.md)。

## 1. 日常使用

### 启动与退出

不需要手动启动。DSH 启动时插件会跑 `dsh-pet.exe watch --watch-pid <DSH Host pid>`；
DSH 正常退出时插件结束它；DSH 被强杀时它每 2 秒自检父进程，10 秒内自己退出并释放设备。

### 设置页

DSH → 设置 → **Plugins → 鳄鱼大宠物**：

- **跟随**：关闭 = 立刻释放设备，回设备默认界面；重新打开 = 恢复跟随。
- **模式**：实时（0s）/ 平衡（10s）/ 安静（30s）/ 极简（120s）——数字是「细节类变化」的合并间隔，
  状态切换（开始、等你、完成）始终尽快刷。
- **提示音 + 音量**：进入「等你处理 / 任务完成」时响一次，同一等待不会反复响。
- **设备 IP**：填设备局域网地址；「测试连接」会直接跑一次只读的设备检查并返回输出。
- **刷新格式**：mono（默认，最不容易闪，约 1–2 秒/帧）、gray4（约 3.5 秒）、gray16（约 5 秒）。

保存写入 `%LOCALAPPDATA%\zectrix-dsh-pet\pet-config.json`，宠物每 2 秒热读，无需重启 DSH。

### 设备手势

| 手势 | 动作 |
| --- | --- |
| 单击确认 | 回首页 |
| 双击确认 | 静音 / 恢复 |
| 三击确认 | 退出回设备普通界面（跟随暂停 + 释放设备） |
| 单击上 / 下 | 多任务时切换 / 翻页 |
| 长按 | 无操作 |

### 花絮

`assets/quips.txt`（源码）/ `plugin/bin/quips.txt`（安装副本）一行一句，≤20 字，`#` 开头为注释。改完重启宠物进程（重载插件）生效。

### 退出与卸载

- 临时停用：设置页关「跟随」，或设备三击。
- 彻底移除：Plugins 里禁用/移除 `zectrix-croc-pet`；DSH 下次启动不再拉起宠物。
- 本机数据（可删）：`%LOCALAPPDATA%\zectrix-dsh-pet\`（state.json、pet-config.json、pet.log、recordings）。
  旧版本留下的开机启动/快捷方式会在新版宠物首次启动时自动清理。

## 2. 配置文件

`%LOCALAPPDATA%\zectrix-dsh-pet\pet-config.json`（插件设置页读写，也可手改后 2 秒生效）：

| 键 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `follow` | bool | true | 是否跟随并占用设备 |
| `sound` | bool | true | 提示音开关 |
| `volume` | 0–100 | 40 | 音量 |
| `target` | string | `192.168.1.162` | 设备 IP / 设备 id |
| `format` | `mono`/`gray4`/`gray16` | `mono` | 屏幕刷新格式 |
| `mode` | `realtime`/`balance`/`calm`/`minimal` | `calm` | 模式（会覆盖 refresh_secs/animation） |
| `refresh_secs` | int | 30 | 细节合并秒数（手动覆盖 mode 时用） |
| `animation` | bool | false | 是否让花絮/细节动起来（省电默认关） |
| `stale_minutes` | int | 15 | 多久没更新算「状态陈旧」 |
| `title_source` | `session`/`prompt` | `session` | 标题来源。`session`=只读 DSH `session/title`（无标题用「目录名 第N轮」），不读正文；`prompt`=显式 opt-in，允许把提示词首行当标题、提问前 60 字当 detail，**会写入 state.json 并显示在设备上** |

`state.json` 是宠物自己的运行缓存（任务快照、上次推送指纹、电量/RSSI、team_busy 等），
一般不用手改；删掉等于重置缓存，下次启动重新扫会话。排查隐私时可用
`dsh-pet forget --titles` 把已有标题换成 `session-<短号>` 并清空提醒指纹。

## 3. 命令行（诊断/验收用，不是日常入口）

```text
dsh-pet watch   [--watch-pid PID] [--target IP] [--format mono|gray4|gray16] [--idle-grace 秒] [--verbose]
                                    守护：无任务不占设备；任务结束按宽限交还
                                    --verbose 时状态行附加标题（默认只记 session-短号 + 状态词）
dsh-pet preview [--demo <状态>] [--out 路径] [--frame N] [--session <ID>]
                                    渲染 400×300 PNG，不连设备（最常用的验收工具）
dsh-pet scene   [--demo <状态>]     打印布局 JSON（autotest 断言用）
dsh-pet sessions                    列出可跟随的会话、状态、进度、上下文占用
dsh-pet set     [--follow on|off] [--sound on|off] [--volume 0-100]
                [--animation on|off] [--refresh 秒] [--calm on|off]
                [--format mono|gray4|gray16] [--title-source session|prompt]
                                    查看/修改 state.json 里的设置（2 秒内生效）
dsh-pet check   [--target IP|ID]    只检查设备（型号/电量/信号），不推画面
dsh-pet sound   --status waiting|done [--target IP|ID] [--volume 0-100]
                                    单独播一次提示音
dsh-pet push    [--demo <状态>] [--target IP|ID] [--format mute]
                                    单次推送后断开（调试版式用）
dsh-pet voice   probe|paste|open|targets|windows|focus|doctor|say|preview
                                    语音链路诊断（设备手势已按规格改为长按无操作）
dsh-pet where                       打印配置/数据/会话路径
dsh-pet forget                      删除本机状态缓存
dsh-pet forget --titles             只做隐私清理：已有任务标题换成 session-<短号>（无会话 id 用
                                    task-<序号>），清空提醒指纹；不删设备地址/设置
dsh-pet --version                   版本号
```

状态名：`idle`（无）/ `working` / `waiting` / `done` / `error` / `stopped` / `unknown`。

常用验收：

```powershell
cargo test                                          # 81 项
.\plugin\bin\dsh-pet.exe preview --demo working --out out\working.png
.\plugin\bin\dsh-pet.exe check --target 192.168.1.162
.\plugin\bin\dsh-pet.exe sessions
```

## 4. 排错

| 现象 | 处理 |
| --- | --- |
| 设置页「设置桥不可用」 | 宠物进程没起来：看 `pet.log`；重启 DSH；确认插件已启用 |
| 测试连接失败 | 设备离线/不同网段/被别的程序占用（等 8 秒）；确认「待机保持 Wi-Fi」已开 |
| 连上但画面一直不换 | 确认 DSH 里有活跃任务；设置页「跟随」是否打开；日志有无「已推送」 |
| 反复闪屏 | 用 mono；模式调「安静/极简」；避免多会话高频抖动 |
| 设备一直是默认界面 | 无任务时本来就不占用；有任务但未占用时看 `pet.log` 的「检测到任务」 |
| 想彻底重来 | 关跟随 → 三击退出 → 删 `state.json`/`pet-config.json` → 重启 DSH |

## 5. 开发与目录

- 源码：`src/`（Rust 2021，`cargo test` 81 项）
- 插件：`plugin/`（`index.js` 宿主：spawn + 配置桥 127.0.0.1:4517；`client.js`：设置页）
- 规格：`docs/SPEC.md`；决策：`docs/adr/`；术语：`GLOSSARY.md`
- 截图：`docs/screens/`
- 装本地改动：

```powershell
cargo build --release
copy target\release\dsh-pet.exe plugin\bin\dsh-pet.exe
# 然后在 DSH 的 Plugins 里重新安装本地目录（或重启 DSH 让插件重载）
```
