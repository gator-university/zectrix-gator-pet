# 鳄鱼大宠物 · 功能规格与验收（SPEC）

> 项目：`zectrix-gator-pet`（历史名 dsh-pet）
> 形态：**DSH 插件**（宿主 bundle 内置 `bin/dsh-pet.exe`），把 DeepSeek Harness 的实时状态推到 ZECTRIX Note 4 墨水屏。
> 版本：0.8.2 · 2026-11-18
> 状态取值：`已验收`（自动化+真机证据齐） / `待验证`（代码完成，缺一项证据） / `待实现`。
> 本文件是唯一功能清单；实现变更先改这里，再改代码。

## 0. 背景与来源

- 本项目是 [MuskCheng/zectrix-workbuddy-pet](https://github.com/MuskCheng/zectrix-workbuddy-pet) 的 fork；原项目跟随 WorkBuddy，本 fork 改为跟随 DeepSeek Harness 并改为 DSH 插件形态。
- 原项目 MIT，Copyright (c) 2026 MuskCheng；本 fork 保留原版权声明，修改部分同 MIT（见 `LICENSE`）。
- 设备：Zectrix Note 4（400×300 单色，`zectrix-s3-epaper-4.2`），固件 ≥ 3.7.5 才支持本地推送。
- 设备通道：`zectrix-local-push-sdk` 0.2.0（UDP 32342，局域网直连；本机 mDNS 被防火墙拦，一律用 IP）。
- DSH 侧只读 `~/.dsh/sessions` 下的会话日志，不写 DSH 数据；默认只读生命周期元数据
  （状态、`session/title`、token 用量），不读 `user/message` 正文与工具参数正文。
  `title_source=prompt` 是唯一例外（显式 opt-in，见 FR-DSH-03）。

## 1. 生命周期（FR-LIFE）

**FR-LIFE-01 DSH 启动即启动宠物进程**
- 步骤：退出宠物 → 在 DSH 中启用本插件 → 关闭并重新打开 DSH。
- 期望：DSH 启动后 ≤5 秒内 `dsh-pet.exe watch` 出现，`pet.log` 有启动行。
- 状态：已验收。
- 证据：`plugin/index.js` `apply()` spawn `bin/dsh-pet.exe watch --watch-pid <host pid>`；2026-11-18 02:10 禁用/启用插件触发重载，宿主自动拉起 pid 41048（父进程为 DSH Host），`pet.log` 记录「已启动宠物」与推送；**2026-11-18 18:03 重启 DSH 后从 Git 安装副本拉起** `C:\Users\liujj\.dsh\profiles\desktop\node_modules\zectrix-gator-pet\bin\dsh-pet.exe watch --watch-pid 52488`（pid 49176，父进程为 DSH Host），配置桥 4517 的 `/health` 返回该安装路径，18:03:31 推送成功（1.70s）。

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
- 期望：默认（`title_source=session`）标题只来自 DSH `session/title`；没有会话标题时用
  「项目目录名 + 第 N 轮」（N 用该项目内 rank，无 rank 记 1），**绝不读 `user/message` 正文**。
  `title_source=prompt` 是显式 opt-in：此时才允许用提示词首行兜底、才保留提问正文，
  且会把相应文字写入 `state.json` 并显示在设备上（README/USAGE/设置说明均标注）。
  两种模式都不显示裸 uuid。
- 状态：已验收（默认不读正文；prompt 模式测试覆盖 opt-in 边界）。
- 证据：`TitleSource`（`session` 默认 / `prompt` opt-in）、`Watcher::prompt_titles`、
  回退 `fallback_title()`（`project_name` + `rank_in_project`）；测试
  `user_message_never_becomes_title_by_default`、`prompt_mode_opt_in_only`。

## 4. 版式（FR-UI）

**FR-UI-01 单任务详情页**
- 内容：左上品牌「鳄鱼大宠物」12px（副行「Zetrix / DSH pet」9px）、右上模式徽章；状态大字+图标；标题≤3 行；详情一行；进度条；等待页题干/徽章；底部两态。
- 状态：已验收。
- 证据：`docs/screens/working.png`、`docs/screens/waiting.png`、`docs/screens/done.png`；`cargo test` lib 59 项（含 layout）。

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

## 5. 鳄鱼形象（FR-GATOR）

**FR-GATOR-01 一只鳄鱼，六个动作**
- 动作：`idle` 待命、`working` 干活、`waiting` 等你、`done` 完成、`error` 出错、`sleep` 睡觉；单帧静态，状态切换才重绘。
- 状态：已验收。
- 证据：`assets/gator/default/{6 动作}.png`（90×120，1:1 绘制在 (300,152)）；`sprite::draw_action` 贪心矩形化；测试 `all_actions_have_art`、`actions_are_pairwise_distinct`。

**FR-GATOR-02 触发规则**
- idle：无任务（含完成 5 秒后）；working：有 Working；waiting：有 Waiting；done：完成后 5 秒内；error：Error/Stopped（决定：Stopped 也走 error 动作）；sleep：连续 30 分钟无活跃任务（`model.last_active_at`）。
- 素材从 30×40@3× 换到 90×120@1× 后，触发规则与动作选择不变。
- 状态：已验收。
- 证据：`layout.rs` 动作选择分支；`last_active_at` 在扫描时刷新。

**FR-GATOR-03 素材契约**
- 路径 `assets/gator/default/{action}.png`；90×120 单色 PNG（L 模式，只含 0/255），按 1:1 直接绘制在 (300,152)，不再缩放；`sprite::draw_action(elements, x, y, action, scale)` 的 `scale` 参数保留（v1 传 1，供未来更换素材尺寸）；装扮目录 `assets/gator/<outfit>/`；可选第二帧（v1 不用）。
- 状态：已验收。
- 证据：6 张 90×120 PNG 由 `scripts/make_gator_v2.py` 可复现；`sprite` 测试直接解码 PNG 断言尺寸 90×120、墨量 6%–50%、包围盒 ≥40×60、单帧矩形数 ≤400、不越出 (300,152)–(390,272)；未知动作不画。

**FR-GATOR-04 手势**
- 单击回主页、双击静音切换、三击退出（follow=false 并释放设备）、长按无操作。
- 状态：已验收。
- 证据：连击 700ms 窗 + 650ms 双击落定；长按确认键日志「按规格无操作（不录音、不投递）」；原长按换造型（六套装扮）随新规格移除。

**FR-GATOR-05 形象风格基线（v3：AI 生成线稿）**
- 物种：gator / 短吻鳄（alligator）——圆头、短而宽的 U 形吻部、上颌盖住下颌；不画长 V 形吻与外露齿排。
- 风格：粗黑描边卡通风、白高光眼、浅色肚皮分节线、短肢体；头身比 chibi（头部约占身高 45%）。
- 规格：90×120 单色 PNG（L 模式，0 墨 / 255 纸），按 1:1 绘制在 (300,152)，`scale` 保留、v1 传 1。
- 六动作：`idle` 端咖啡、`working` 桌前敲笔记本、`waiting` 举手 + 问号、`done` 点赞 + 星星、`error` 轻微惊吓（睁大眼、小圆嘴、冷汗）、`sleep` 蜷卧闭眼 + Z。
- 素材契约：`assets/gator/default/{action}.png`，用户可用 6 个同尺寸同路径文件直接替换，不改代码（ADR 0007）。
- 生成：母版 `art/gen-v3/*.png` 由 gpt-image-2（image edit 模式；idle 以用户参考图为基准、其余以 idle 为角色基准）生成并入库存 512px；`scripts/prep_gator_art.py` 统一裁剪并转换 90×120 二值图，顺带生成 `plugin/icon.png`（ADR 0008）。
- 状态：已验收。
- 证据：`assets/gator/default/*.png`；`docs/screens/{working,waiting,done,error,stopped}.png`；`plugin/icon.png`；`src/sprite.rs` 测试 `all_actions_have_art`/`actions_are_pairwise_distinct`；`art/gen-v3/` 母版；设计记录 `docs/specs/gator-redesign-v2.md`、ADR 0007/0008。

## 6. 提示音与手势（FR-AUD）

**FR-AUD-01 提示音**
- 期望：进入 Waiting/Done 时响一次，同一等待指纹不重复；音量 0–100；设备双击静音切换。
  指纹只存 `sha256(key|状态)` 的前 16 位十六进制，不含标题或提问正文；Waiting 的
  `detail` 默认固定为「有 1 个问题待确认」（prompt 模式例外）；`pet.log` 状态行默认
  只记 `session-<短号>` + 状态词，`--verbose` 才附加标题。
- 状态：已验收。
- 证据：`announce()` + `announced`（`sound::notice_sig` / `is_notice_hash`）；旧格式条目
  在 `load_store_at` 加载时清空；设备双击「已静音 / 提示音已开」即时反馈；测试
  `announced_contains_hash_not_text`、`waiting_detail_has_no_question_text`。

## 7. 模式（FR-MODE）

- 实时（0s）、平衡（10s）、安静（30s）、极简（120s）：模式只决定细节合并节流，没有动画维度。
- 徽章显示当前模式；模式改变后 ≤2 秒生效并推一帧。
- 状态：已验收。
- 证据：`apply_pet_config()` mode 映射 + 日志「[配置] 插件配置已生效：follow=true 模式=安静 …」；徽章见 preview。

## 8. 设置页（FR-SET）

**FR-SET-01 位置**
- 期望：DSH 侧栏「插件」页 → 已安装插件 → 本插件详情里渲染配置表单（与官方插件一致）；「设置 → 内置插件」只是只读列表；无独立面板/托盘。
- 状态：已验收（0.7.3 修掉 `dsh.client.inject: ["slots"]` —— 该字段要的是「先加载哪些客户端模块包」，写成服务名会让 client bundle 加载失败；0.7.4 修深色主题下拉配色）。
- 证据：`plugin/client.js` 用 `window.__ModuleLoader__` + `ctx.effect(() => ctx.slots.inject('plugins.bundle.config', …))`；`package.json` `dsh.client{platform:web,immediately:true}`（无 inject）；用户在侧栏「插件 → 已安装插件 → 鳄鱼大宠物」看到跟随/模式/提示音/音量/设备 IP/刷新格式/保存/重新读取表单并保存生效（0.7.3/0.7.4 截图）。

**FR-SET-02 字段**
- 术语说明：见 GLOSSARY「配置桥」「宿主插件」「client bundle」。
- 跟随开关、模式、提示音+音量、设备 IP+「测试连接」、刷新格式（mono/gray4/gray16）、
  `title_source`（`session`/`prompt`，设置页已带隐私提示）。
- 动作调试（6 动作预览）v1 未做：列为后续项，不阻塞验收。
- 状态：已验收（字段全部接入设置页；动作调试为后续项）。
- 证据：`plugin/client.js`（0.7.4）字段齐全；`pet-config.json` 白名单在 `plugin/bridge.js` 与 `src/config.rs` 两端同校验。

**FR-SET-03 生效**
- 期望：保存后 ≤2 秒宠物读到新配置（写 `pet-config.json`，守护轮询）。
- 状态：已验收。
- 证据：`apply_pet_config()` 每 2 秒比对并打印「[配置] 插件配置已生效…」，变更时强推一帧。2026-11-18 02:11 经配置桥 POST `{"volume":35}`，1 秒内日志「[配置] 插件配置已生效：follow=true 模式=安静 音量=35 目标=192.168.1.162 格式=mono」。

**FR-SET-04 测试连接**
- 期望：点击后 3 秒内返回设备可达/不可达（及电量），不阻塞页面。
- 状态：已验收。
- 证据：宿主插件 HTTP `POST /test` → `dsh-pet check`（8s 超时）；实测返回 `{"ok":true,"output":"错误: 设备忙碌…（设备在线；当前画面由宠物维护，属正常）"}`，宠物占用设备时也能确认在线。

## 9. 安全与隐私（FR-SEC / FR-PRIV / FR-DATA）

**FR-SEC-01 配置桥鉴权**
- 期望：桥只监听 `127.0.0.1`；`/config`、`/test` 必须带逐安装 token（`%LOCALAPPDATA%\zectrix-dsh-pet\bridge-token`，0600）；`/token` 只接受白名单 Origin + 同源/同站请求；`/health` 免 token 但只回 `{ok, version}`。
- 状态：已验收。
- 证据：`plugin/bridge.js` + `plugin/test/bridge.test.js`（25 例，`npm test` 全绿）；提交 `9fc45fc`（T1）；0.8.2 起 `/token` 只信任 `dsh-app://app`、令牌 10 分钟轮换、`/test` 3 秒冷却、`requestTimeout`/`headersTimeout`/`maxConnections` 上限（提交 `b0ebbcd`，S1）。

**FR-SEC-02 跨站与最小暴露**
- 期望：任何非白名单 Origin 或 `Sec-Fetch-Site: cross-site` 一律 403；例外只有 DSH 桌面渲染进程自己的 `dsh-app://app`（Electron 自定义 scheme，跨源到本机 HTTP 时浏览器固定标 `cross-site`，无法被远程网页伪造）。CORS 只回精确 Origin + `Vary: Origin`，无 `*`；`/test` 只用已存设备 IP，忽略请求体 target。
- 状态：已验收。
- 证据：`bridge.test.js` 覆盖 401/403/415/413/400/429 与 OPTIONS 路径；0.8.2 起「http 回环 Origin 一律拿不到 token（含带 Sec-Fetch-Site）」与「`dsh-app://app` 可取且轮换」各有用例，25 例全过；提交 `9fc45fc`、`b0ebbcd`。

**FR-SEC-03 输入校验与原子写**
- 期望：设备 IP 限私网/回环 IPv4；音量 0–100；格式/模式限枚举；非法值 400 且不写盘；`pet-config.json` 与 `state.json` 均为 tmp+rename 原子写。
- 状态：已验收。
- 证据：`bridge.js` 校验 + `src/config.rs` 读端同校验；`write_json_atomic`（提交 `9fc45fc`、`482ab6a`）。

**FR-SEC-04 数据目录权限**
- 期望：数据目录与 token 文件 ACL 去掉继承，只留当前用户 + SYSTEM；收紧失败只警告、不阻塞。
- 状态：已验收。
- 证据：`plugin/winacl.js`（`createBridge` 启动时对数据目录与已有 token 执行 `icacls /inheritance:r /grant:r`）+ `plugin/test/winacl.test.js`（假 `execFile`）；提交 `729acb8`（S2）。

**FR-SEC-05 读取上限**
- 期望：会话日志读前 `metadata().len()>32MB` 跳过；zstd 解码 `Read::take(64MB)` 且最多 4096 帧；超限只警告一次。
- 状态：已验收。
- 证据：`src/dsh.rs`、`src/scan.rs` 上限 + 单测；提交 `f7715be`（S4）。

**FR-SEC-06 子进程与父进程校验**
- 期望：Rust 侧 target 只收私网/回环 IPv4 字面量；`--watch-pid` 用启动快照的进程创建时间 + 镜像名逐拍比对（镜像名不在 `DeepSeek Harness.exe`/`dsh-pet.exe` 时仅警告）；`tasklist` 走 `%SystemRoot%\System32\tasklist.exe`。
- 状态：已验收。
- 证据：`src/config.rs` target 校验、`src/main.rs` `probe_process`/`ParentGuard`；提交 `443c26a`（S5）。

**FR-PRIV-01 标题来源**
- 期望：默认 `title_source=session`，只读 `session/title`；缺失时用「项目目录名 第 N 轮」；完全不读 `user/message`；`prompt` 模式必须显式开启。
- 状态：已验收。
- 证据：`src/dsh.rs` 默认分支 + 4 个单测；提交 `7e30871`（T2）；设置页已带 privacy 提示。

**FR-PRIV-02 等待文案与提醒指纹**
- 期望：Waiting detail 默认固定「有 1 个问题待确认」；`state.json` 的提醒指纹只存 `sha256(key|status)` 前 16 位，不含标题/提问正文；旧长文案在加载时清空。
- 状态：已验收。
- 证据：`src/sound.rs` SHA-256 实现 + 向量测试；提交 `7e30871`。

**FR-PRIV-03 日志与历史清理**
- 期望：`pet.log` 默认只记 `session-<短号> · 状态词`，`--verbose` 才带标题；`dsh-pet forget --titles` 可把旧标题清成 session 短号并清空指纹，保留设置。
- 状态：已验收。
- 证据：`src/daemon.rs` 状态日志 + `cli.rs` forget 子命令；单测覆盖；0.8.0 部署时已对真机状态执行一次清理（`forget --titles`：替换 76 条标题、清空提醒指纹；旧 `pet.log` 归档后重建）。

**FR-PRIV-04 历史彻底清理**
- 期望：`forget --titles` 直接原子写净化 state（不滚 `.bak`），随后删除 `state.bak.json`/`state.corrupt-*.json`/`state.recovered.json`/`watch.log` 并逐项打印；degraded 时保留损坏现场与 recovered 证据。
- 状态：已验收。
- 证据：`src/store.rs` `save_store_at_no_backup` + 3 个新单测；提交 `5a57d09`（S3）。

**FR-DATA-01 损坏保护**
- 期望：`state.json` 解析失败时改名为 `state.corrupt-<ts>.json` 且**绝不覆盖**（降级只写 `state.recovered.json`）；正常写前滚 `state.bak.json`；`pet-config.json` 字段级容错。
- 状态：已验收。
- 证据：`src/store.rs` + 单测 `save_never_overwrites_a_state_file_it_cannot_parse` 等；提交 `80dd438`（T3）。

**FR-PERF-01 主循环开销**
- 期望：空闲 tick 平均 <5ms；`--trace-tick` 可输出每 10 拍耗时；会话扫描按 mtime 增量、团队扫描 10 秒全量 1 秒快查；`state.json` 按 stamp 缓存、写盘节流 1 秒。
- 状态：已验收。
- 证据：`--trace-tick` 实测空闲一拍 1.17ms（扫描 0.663 + 团队 0.430 + 读缓存 0.076）；提交 `6a86e7c`（T8）。

**FR-ARCH-01 模块拆分**
- 期望：`main.rs` <800 行，职责拆到 `store/config/scan/daemon/cli`；测试集中 `src/tests.rs`；`cargo test --offline` ≥80。
- 状态：已验收。
- 证据：`main.rs` 449 行；111 项测试（58 lib + 53 bin）全绿、0 warning；提交 `79d4625`…`e7cf357`（T9）。

## 10. 发布与归属（FR-REL）

**FR-REL-01 归属**：README/USAGE/SPEC 注明 fork 自 MuskCheng/zectrix-workbuddy-pet；LICENSE 保留原版权。
- 状态：已验收。

**FR-REL-02 安装方式**：DSH Add plugin → Git 地址 `github:gator-university/zectrix-gator-pet#path:/plugin`（仓库根是 Rust crate，插件清单在 `plugin/` 子目录；仓库已带 `plugin/bin/dsh-pet.exe`）。本地开发用 link 目录。
- 状态：已验收。公开仓库 https://github.com/gator-university/zectrix-gator-pet （`main` 已推送；tag `v0.8.2`；`plugin/bin/dsh-pet.exe` 已入库，安装无需本地构建）。
- 证据：`pnpm add "github:gator-university/zectrix-gator-pet#path:/plugin"` 安装到 profile `node_modules`（0.7.2 首装验证；0.7.3 修复设置页加载；0.8.0 安全整改后以同一 spec 升级）；重启 DSH 后宠物进程命令行指向 `…\.dsh\profiles\desktop\node_modules\zectrix-gator-pet\bin\dsh-pet.exe`，配置桥 `/health` 返回同一路径。

**FR-REL-03 版本**：`Cargo.toml` 与插件 `package.json` 同步（0.8.2）；`engines.dsh >= 0.2.1-alpha.2`。
- 状态：已验收。

## 11. 可验证性（FR-VER）

**FR-VER-01 自动化**：`cargo test` 全过（≥80 项），插件侧 `npm test` 全过。
- 状态：已验收。58 lib + 53 bin = **111** 项；`plugin` 侧 25 例。

**FR-VER-02 预览**：`dsh-pet preview --out …` 生成 400×300 PNG，覆盖单/双/3+、等待、完成、出错、空闲、6 动作。
- 状态：已验收。证据：`docs/screens/{live,working,waiting,done,error,stopped}.png`（由最终 90×120 素材重出）。
- 性能目标：单帧元素上限从 220 提到 **400**（90×120 素材的贪心矩形比 30×40 多；实测单帧 245–280 个场景元素 = 素材 208–274 个矩形 + 版式约 10，见 `src/layout.rs` 的 `layouts_obey_hub_limits`，上限 400；再高应简化素材而不是拆动作）。

**FR-VER-03 真机**：关键页面推送到设备后由用户确认。
- 状态：已验收（2026-11-18 用户在真机运行 0.8.0 后确认：启动/跟随/多任务/完成停留/断线重连均正常；照片如需入库可后续补到 `docs/screens/`）。

**FR-VER-04 诊断命令保留**：`check`/`push`/`preview`/`sessions`/`set`/`sound`/`where`/`forget`；
`watch`/`run` 支持 `--verbose`（状态行附加标题）；`forget --titles` 只做隐私清理
（任务标题换 `session-<短号>`、清空提醒指纹，不删设置）。
- 状态：已验收（见 `usage()` 与 USAGE.md 第 3 节）。

## 12. 已知限制 / 后续项

- 同一时刻只能一个程序占设备（固件约束）。
- 动作调试页（FR-SET-02 子项）未做；列为后续项。
- 语音输入（`voice` 模块 + CLI）与六套猫造型已按规格移除（0.8.0）：设备长按无操作，
  `voice`/`skin` 命令、`pet_skin`/`animation` 设置项与相关模块全部删除；旧
  `pet-config.json`/`state.json` 里残留的 `pet_skin`、`animation` 键按未知字段静默忽略。
- 设置页需重启 DSH 后才可见（client bundle 在启动时加载）。
