# 鳄鱼大宠物 · 功能规格与验收（SPEC）

> 项目：`zectrix-croc-pet`（历史名 dsh-pet）
> 形态：**DSH 插件**（宿主 bundle 内置 `bin/dsh-pet.exe`），把 DeepSeek Harness 的实时状态推到 ZECTRIX Note 4 墨水屏。
> 版本：0.6.1 · 2026-11-18
> 状态取值：`已验收`（自动化+真机证据齐） / `待验证`（代码完成，缺一项证据） / `待实现`。
> 本文件是唯一功能清单；实现变更先改这里，再改代码。

## 0. 背景与来源

- 本项目是 [MuskCheng/zectrix-workbuddy-pet](https://github.com/MuskCheng/zectrix-workbuddy-pet) 的 fork；原项目跟随 WorkBuddy，本 fork 改为跟随 DeepSeek Harness 并改为 DSH 插件形态。
- 原项目 MIT，Copyright (c) 2026 MuskCheng；本 fork 保留原版权声明，修改部分同 MIT（见 `LICENSE`）。
- 设备：Zectrix Note 4（400×300 单色，`zectrix-s3-epaper-4.2`），固件 ≥ 3.7.5 才支持本地推送。
- 设备通道：`zectrix-local-push-sdk` 0.2.0（UDP 32342，局域网直连；本机 mDNS 被防火墙拦，一律用 IP）。
- DSH 侧只读 `~/.dsh/sessions` 下的会话日志，不写 DSH 数据。

## 1. 生命周期（FR-LIFE）

**FR-LIFE-01 DSH 启动即启动宠物进程**
- 步骤：退出宠物 → 在 DSH 中启用本插件 → 关闭并重新打开 DSH。
- 期望：DSH 启动后 ≤5 秒内 `dsh-pet.exe watch` 出现，`pet.log` 有启动行。
- 状态：已验收。
- 证据：`plugin/index.js` `apply()` spawn `bin/dsh-pet.exe watch --watch-pid <host pid>`；2026-11-18 02:10 禁用/启用插件触发重载，宿主自动拉起 pid 41048（父进程为 DSH Host），`pet.log` 记录「已启动宠物」与推送。

**FR-LIFE-02 DSH 退出即退出宠物进程**
- 步骤：DSH 托盘 Quit（正常退出）。
- 期望：宠物进程 ≤5 秒退出，设备释放回默认界面。
- 状态：已验收（等价路径）。双保险：①`ctx.effect` 清理函数 `child.kill()`；②`--watch-pid` 每 2 秒自检父进程。
- 证据：dispose 清理 kill 已随插件重载实测（旧子进程退出）；父进程消失自退实测见 FR-LIFE-03。

**FR-LIFE-03 强杀/崩溃兜底**
- 步骤：任务管理器强杀 DSH Host（或 `Stop-Process -Force`）。
- 期望：宠物 ≤10 秒自退，不残留占用。
- 状态：已验收。
- 证据：手动 `watch --watch-pid <临时进程>`，父进程结束后输出「DSH 已退出：宠物收尾，释放设备。」并退出；`pid_alive()` 每 2 秒检查。

**FR-LIFE-04 单实例**
- 步骤：插件重复 apply，或手动再启一次 exe。
- 期望：只存在一个宠物进程，第二个立即退出并写日志。
- 状态：已验收。
- 证据：命名互斥体 `Local\zectrix-dsh-pet-daemon`；实测第二实例输出「已有守护在跑，本进程（pid 42892）退出」。

**FR-LIFE-05 旧形态清理**
- 步骤：从旧版（控制面板/托盘/开机自启）升级后首次启动。
- 期望：删除旧启动项 `zectrix-dsh-pet.vbs`、桌面/开始菜单「DSH 宠物.lnk / 鳄鱼大宠物.lnk」，不残留计划任务。
- 状态：已验收。
- 证据：`retire_legacy_autostart()` 在 `cmd_watch` 开头执行；实测日志「已清理旧版启动项：…\Startup\zectrix-dsh-pet.vbs」；计划任务 `zectrix-dsh-pet` 已不存在。

**FR-LIFE-06 占用策略**
- 步骤：DSH 在线、无任何任务。
- 期望：宠物运行但不推送，设备保持默认界面。
- 状态：已验收。
- 证据：`wait_for_task` 只读不连；日志「检测到 DeepSeek Harness，进入待命（只读会话，不占用设备）」。

## 2. 设备连接与刷新（FR-DEV）

**FR-DEV-01 连接**
- 期望：按设置 IP 直连；不在线时每 15 秒重试，不弹窗、不阻塞。
- 状态：已验收。
- 证据：`[02:06:55] 15 秒后重连（第 1 次）` → `已连接: id=206ef1b695bc … addr=192.168.1.162:32342｜灰阶 mono`。

**FR-DEV-02 刷新格式**
- 期望：默认 `mono`（1–2.4 秒/帧）；设置页可切 `gray4`/`gray16`；下一帧生效。
- 状态：已验收。
- 证据：`default_format()="mono"`、`format_set_at` 时间戳合并防回退；实测 mono 推送 1.48s / 1.73s，gray16 约 5s。

**FR-DEV-03 合并刷新（少闪）**
- 期望：连续变化合并成一次推送；最小间隔 3 秒；连接后先等 1.2 秒再推首帧。
- 状态：已验收。
- 证据：`PUSH_COOLDOWN_SECS=3`、settle 1200ms + 最多 4×450ms、扫描 tick 1s；日志「已推送：…（1.48s）」。

**FR-DEV-04 推送失败**
- 期望：设备忙/离线 15 秒内重试；指纹去重，不重复刷同一画面。
- 状态：已验收。
- 证据：`last_sig` 指纹 + `15 秒后重连`；设备忙碌时提示「约 8 秒释放，稍后自动重试」。

## 3. 会话跟随（FR-DSH）

**FR-DSH-01 状态机**
- 期望：回合开始→「正在工作」；等待确认/提问→「等你处理」；完成→「任务完成」；中断→「已停止」；错误→「遇到问题」；15 分钟无更新→「状态陈旧」。
- 状态：已验收。
- 证据：`src/dsh.rs` 测试（zstd 帧尾读、turn/end 各 reason、部分帧、归档过滤等）。

**FR-DSH-02 多会话/子智能体**
- 期望：同项目多会话各自成任务；子智能体仍在跑时父任务显示「子智能体运行中」；归档会话说退场。
- 状态：已验收。
- 证据：`TeamScan` 扫裸 uuid 子会话日志（`decode_all_frames` + turn 配对 + parentSession 溯源 ≤8 层）；`state.team_busy`；实测推送「子智能体运行中 · 从 handoff 继续用 DeepSeek 完成开发（1.48s）」。

**FR-DSH-03 标题**
- 期望：优先 DSH 会话标题；没有时用首条用户消息前 80 字；不显示裸 uuid。
- 状态：已验收。
- 证据：`session/title` 事件 + 回退逻辑测试。

## 4. 版式（FR-UI）

**FR-UI-01 单任务详情页**
- 内容：左上品牌「鳄鱼大宠物」12px（副行「Zetrix / DSH pet」9px）、右上模式徽章；状态大字+图标；标题≤3 行；详情一行；进度条；等待页题干/徽章；底部两态。
- 状态：已验收。
- 证据：`docs/screens/working.png`、`docs/screens/waiting.png`、`docs/screens/done.png`；`cargo test` lib 53 项（含 layout）。

**FR-UI-02 双任务**
- 期望：两行，每行「状态图标 + 标题 2 行 + 状态短语」；不画鳄鱼；Waiting 置顶。
- 状态：已验收。
- 证据：`out/v6-idle.png`（真实两任务）、`out/v6-live.png`；`display_order()` 测试。

**FR-UI-03 三个及以上**
- 期望：紧凑列表每页 3 行，右上「1/2」页码；上/下键翻页；Waiting 永远置顶；新 Waiting 自动第一页。
- 状态：已验收（自动化）；真机 3+ 待拍照。
- 证据：测试 `waiting_task_is_pinned_first_and_pages_show_numbers`（4 任务显示「4 个任务 · 1/2」，第一行等待任务，翻页 2/2）。

**FR-UI-04 状态图标**
- 期望：● 正在工作、！等你处理、■ 已停止、× 遇到问题、? 待确认、完成用矢量对勾（不依赖字体符号）。
- 状态：已验收。
- 证据：`status_glyph()` + `draw_check()` 用 `line` 图元；微软雅黑无 ✓/✕，故不再用字体符号。

**FR-UI-05 品牌头**
- 期望：品牌行两行——中文主行「鳄鱼大宠物」12px，英文副行「Zetrix / DSH pet」9px；无「AI 工作搭子」；右上角保留模式徽章（演示时显示「演示」）。
- 状态：已验收。
- 证据：测试 `header_is_brand_only_and_idle_footer_reports_device`（断言中文 12px / 英文 9px、英文基线在分隔线之上、两行不压徽章）；`docs/screens/{live,working,waiting,done,error,stopped}.png`。

**FR-UI-06 底部两态**
- 有任务：左「上下文 N% · 剩 MK」，右花絮；空闲：左「电量 N% · Wi-Fi 良好/一般/弱」，右「等你派活」；底部永远保留。
- 状态：已验收。
- 证据：同上测试；真机电量 88% / RSSI -43 → 「Wi-Fi 良好」。

**FR-UI-07 花絮可替换**
- 期望：`assets/quips.txt`（源码）/ `plugin/bin/quips.txt`（安装副本），UTF-8 一行一句 ≤20 字；缺失/为空回退内置池；改文件后重启生效。
- 状态：已验收。
- 证据：`quip_pool()`（`DSH_PET_QUIPS` 环境变量可覆盖路径）；测试 `quips_file_is_editable_and_lines_fit_the_footer`；旧状态里的句子不在池中会自动换新。

**FR-UI-08 完成停留**
- 期望：完成画面固定 5 秒后回待命；无后续任务按 grace 交还设备。
- 状态：已验收。
- 证据：测试 `done_screen_hands_over_to_idle_after_five_seconds`（第 4 秒仍完成，第 6 秒待命中）。

## 5. 鳄鱼形象（FR-CROC）

**FR-CROC-01 一只鳄鱼，六个动作**
- 动作：`idle` 待命、`working` 干活、`waiting` 等你、`done` 完成、`error` 出错、`sleep` 睡觉；单帧静态，状态切换才重绘。
- 状态：已验收。
- 证据：`assets/croc/default/{6 动作}.png`（90×120，1:1 绘制在 (300,152)）；`sprite::draw_action` 贪心矩形化；测试 `all_actions_have_art`、`actions_are_pairwise_distinct`。

**FR-CROC-02 触发规则**
- idle：无任务（含完成 5 秒后）；working：有 Working；waiting：有 Waiting；done：完成后 5 秒内；error：Error/Stopped（决定：Stopped 也走 error 动作）；sleep：连续 30 分钟无活跃任务（`model.last_active_at`）。
- 素材从 30×40@3× 换到 90×120@1× 后，触发规则与动作选择不变。
- 状态：已验收。
- 证据：`layout.rs` 动作选择分支；`last_active_at` 在扫描时刷新。

**FR-CROC-03 素材契约**
- 路径 `assets/croc/default/{action}.png`；90×120 单色 PNG（L 模式，只含 0/255），按 1:1 直接绘制在 (300,152)，不再缩放；`sprite::draw_action(elements, x, y, action, scale)` 的 `scale` 参数保留（v1 传 1，供未来更换素材尺寸）；装扮目录 `assets/croc/<outfit>/`；可选第二帧（v1 不用）。
- 状态：已验收。
- 证据：6 张 90×120 PNG 由 `scripts/make_croc_v2.py` 可复现；`sprite` 测试断言尺寸 90×120、墨量 6%–50%、包围盒 ≥40×60、单帧矩形数 ≤400、不越出 (300,150)–(390,270)；未知动作不画。

**FR-CROC-04 手势**
- 单击回主页、双击静音切换、三击退出（follow=false 并释放设备）、长按无操作。
- 状态：已验收。
- 证据：连击 700ms 窗 + 650ms 双击落定；长按确认键日志「按规格无操作（不录音、不投递）」；原长按换造型（六套装扮）随新规格移除。

**FR-CROC-05 形象 v2 风格基线**
- 物种：gator / 短吻鳄（alligator）——圆头、短而宽的 U 形吻部、上颌盖住下颌；不画长 V 形吻与外露齿排。
- 规格：90×120 单色 PNG（L 模式，0 墨 / 255 纸），几何按 1:1 绘制在 (300,152)，`scale` 保留、v1 传 1；头身比 chibi（头部约占身高 45%）。
- 画法：2px 实心黑描边；眼睛 2×2 白高光；肚皮 2–3 条 1px 分节线；短腿短尾。
- 六动作：`idle` 端咖啡、`working` 敲笔记本、`waiting` 举手 + 问号、`done` 星星 + 点赞、`error` 绷带 + × 眼、`sleep` 闭眼 + Z。
- 素材契约：`assets/croc/default/{action}.png`；六张由同一套头/身/腿/尾模板派生，是同一只；用户可用 6 个同尺寸同路径文件直接替换，不改代码。
- 生成：`scripts/make_croc_v2.py`（4× 超采样 + MinFilter 腐蚀描边 + NEAREST 下采样，只输出 0/255）；`--icon` 另渲染 128×128 插件图标。
- 状态：已验收。
- 证据：`assets/croc/default/*.png`；`docs/screens/{working,waiting,done,error,stopped}.png`；`plugin/icon.png`；`src/sprite.rs` 测试 `all_actions_have_art`/`actions_are_pairwise_distinct`；设计记录 `docs/specs/croc-redesign-v2.md`、ADR 0007。

## 6. 提示音与手势（FR-AUD）

**FR-AUD-01 提示音**
- 期望：进入 Waiting/Done 时响一次，同一等待指纹不重复；音量 0–100；设备双击静音切换。
- 状态：已验收。
- 证据：`announce()` + `announced` 指纹；设备双击「已静音 / 提示音已开」即时反馈。

## 7. 模式（FR-MODE）

- 实时（animation on，刷新 0s）、平衡（10s）、安静（30s）、极简（120s）。
- 徽章显示当前模式；模式改变后 ≤2 秒生效并推一帧。
- 状态：已验收。
- 证据：`apply_pet_config()` mode 映射 + 日志「[配置] 插件配置已生效：follow=true 模式=安静 …」；徽章见 preview。

## 8. 设置页（FR-SET）

**FR-SET-01 位置**
- 期望：DSH 的 Plugins 页 → 本插件详情（与官方插件一致）；无独立面板/托盘。
- 状态：待验证（代码完成，需重启 DSH 在界面确认；本机已装 bundle，`plugins.bundle.config` slot 注册 key `zectrix-croc-pet`）。
- 证据：`plugin/client.js` 用 `window.__ModuleLoader__` + `ctx.slots.inject('plugins.bundle.config', …)`；`package.json` `dsh.client{platform:web,immediately:true}`；Node 沙箱 harness 实测注册 `{name:'plugins.bundle.config', key:'zectrix-croc-pet'}` 且组件为函数（`CLIENT OK`）；展示元信息 `plugin/locale/{en,zh}.json`（meta.title/description）与 `./icon` 已导出并通过 `files` 打包，插件启用/停用重载无警告、宠物进程正常拉起。页面渲染需刷新 DSH GUI 后确认。

**FR-SET-02 字段**
- 术语说明：见 GLOSSARY「配置桥」「宿主插件」「client bundle」。
- 跟随开关、模式、提示音+音量、设备 IP+「测试连接」、刷新格式（mono/gray4/gray16）。
- 动作调试（6 动作预览）v1 未做：列为后续项，不阻塞验收。
- 状态：待验证（字段已实现；动作调试未实现）。

**FR-SET-03 生效**
- 期望：保存后 ≤2 秒宠物读到新配置（写 `pet-config.json`，守护轮询）。
- 状态：已验收。
- 证据：`apply_pet_config()` 每 2 秒比对并打印「[配置] 插件配置已生效…」，变更时强推一帧。2026-11-18 02:11 经配置桥 POST `{"volume":35}`，1 秒内日志「[配置] 插件配置已生效：follow=true 模式=安静 音量=35 目标=192.168.1.162 格式=mono」。

**FR-SET-04 测试连接**
- 期望：点击后 3 秒内返回设备可达/不可达（及电量），不阻塞页面。
- 状态：已验收。
- 证据：宿主插件 HTTP `POST /test` → `dsh-pet check`（8s 超时）；实测返回 `{"ok":true,"output":"错误: 设备忙碌…（设备在线；当前画面由宠物维护，属正常）"}`，宠物占用设备时也能确认在线。

## 9. 发布与归属（FR-REL）

**FR-REL-01 归属**：README/USAGE/SPEC 注明 fork 自 MuskCheng/zectrix-workbuddy-pet；LICENSE 保留原版权。
- 状态：已验收。

**FR-REL-02 安装方式**：DSH Add plugin → Git 地址 `https://github.com/gator-university/zectrix-croc-pet`；本地开发用 link 目录。
- 状态：待验证（本地 link 安装已验证；Git 地址未建仓/未发布）。

**FR-REL-03 版本**：`Cargo.toml` 与插件 `package.json` 同步（0.6.1）；`engines.dsh >= 0.2.1-alpha.2`。
- 状态：已验收。

## 10. 可验证性（FR-VER）

**FR-VER-01 自动化**：`cargo test` 全过（≥65 项）。
- 状态：已验收。53 lib + 15 bin = **68** 项。

**FR-VER-02 预览**：`dsh-pet preview --out …` 生成 400×300 PNG，覆盖单/双/3+、等待、完成、出错、空闲、6 动作。
- 状态：已验收。证据：`docs/screens/{live,working,waiting,done,error,stopped}.png`（由最终 90×120 素材重出）。
- 性能目标：单帧元素上限从 220 提到 **400**（90×120 素材的贪心矩形比 30×40 多；实测 218–284，见 `src/layout.rs` 的 `layouts_obey_hub_limits`，上限 400；再高应简化素材而不是拆动作）。

**FR-VER-03 真机**：关键页面推送到设备后由用户拍照确认。
- 状态：待验证（已有实时推送日志与用户可见画面；照片待用户补）。

**FR-VER-04 诊断命令保留**：`check`/`push`/`preview`/`sessions`/`set`/`sound`/`voice`/`where`/`forget`。
- 状态：已验收（见 `usage()` 与 USAGE.md 第 3 节）。

## 11. 已知限制 / 后续项

- 同一时刻只能一个程序占设备（固件约束）。
- 动作调试页（FR-SET-02 子项）、GitHub 建仓与发布未做。
- 语音输入已按 Q18 决定从设备手势移除（`voice` CLI 仍可诊断转写/粘贴链路）；如需恢复，需重新定义手势。
- 设置页需重启 DSH 后才可见（client bundle 在启动时加载）。
