# GLOSSARY · 鳄鱼大宠物

统一术语，文档、代码注释、spec、prompt 一律照此用词。

| 术语 | 英文/代码名 | 含义 |
|---|---|---|
| 宠物进程 | `dsh-pet` / daemon | 常驻的 Rust 可执行文件 `dsh-pet.exe`，负责扫 DSH 日志、连设备、推送画面。随 DSH 生命周期启停。 |
| DSH Host | host | DSH 桌面版 spawn 的 Node 后端子进程；插件 bundle 运行在这里。 |
| 插件 / bundle | bundle | DSH 的扩展包：`package.json` 声明 `dsh.bundle.patch`，patch 插入 cordis 行；插件代码导出 `apply(ctx, config)`。 |
| 插件详情页 | plugin page | DSH Plugins 页中本插件的配置页（React，纯 JS）。 |
| 设置文件 | `pet-config.json` | `%LOCALAPPDATA%\zectrix-dsh-pet\pet-config.json`，插件页面写入；宠物进程每 2 秒热读。 |
| 状态缓存 | `state.json` | `%LOCALAPPDATA%\zectrix-dsh-pet\state.json`，宠物进程写：任务缓存、当前设置、设备占用等。 |
| 会话 | session | DSH 的一个会话目录 `~/.dsh/sessions/<项目>/session-<uuid>/`。 |
| 任务 | task | 宠物对一条 DSH 会话的抽象，带任务 id、标题、状态。 |
| 回合 | turn | DSH 的一次执行（`turn/start`…`turn/end`）；宠物状态机的基本单位。 |
| 子智能体 | subagent | 由主会话派生的子会话（目录名裸 uuid，`parentSession` 指向父会话）。 |
| 正在工作 | `Working` | 有回合在跑。 |
| 等你处理 | `Waiting` | 等待用户确认/回答/授权。 |
| 任务完成 | `Done` | 回合正常结束。 |
| 已停止 | `Stopped` | 回合被中断/中止。 |
| 遇到问题 | `Error` | 回合以错误结束。 |
| 状态陈旧 | `Unknown` | 超时无更新，降级显示。 |
| 占用设备 | claim | 宠物进程接管墨水屏画面（有任务时）。 |
| 待命 | idle / standby | 宠物进程在线但无任务；设备显示自身默认界面（v1 不占用）。 |
| grace | 交还宽限 | 任务结束后继续守着设备的时长（默认 300 秒）。 |
| 动作 | action | 鳄鱼的六种姿态：idle/working/waiting/done/error/sleep。 |
| 装扮 | outfit | 未来扩展的一整套鳄鱼皮肤，路径 `assets/croc/<outfit>/`。 |
| 花絮 | quip | 设备右下角一句短话，来自 `assets/quips.txt`。 |
| 模式 | mode | 实时/平衡/安静/极简，控制动画与刷新节流。 |
| mono/gray4/gray16 | refresh format | 墨水屏刷新格式；mono 默认、最快、最少闪。 |
| 上下文 | context | DSH 会话 token 占用（input+cacheRead+cacheWrite）/窗口大小。 |
| 徽章 | badge | 设备右上角模式标识（演示数据时显示「演示」）。 |
| 手势 | gesture | 设备确认键的单按/双击/三击/长按。 |
| 矢量对勾 | `draw_check` | 用线段画的完成对勾，避免字体缺字。 |
| 插件兼容门槛 | `engines.dsh` | 插件声明的 DSH 版本范围，不满足会被跳过。 |
| 配置桥 | config bridge | 宿主插件在 127.0.0.1:4517 起的本地 HTTP 服务：`GET/POST /config`、`POST /test`、`GET /health`，供设置页读写 `pet-config.json` 与测试设备连接。 |
| 宿主插件 | host plugin | 插件里跑在 DSH Host（Node 子进程）的那一半，`apply(ctx)` 注册 effect，负责 spawn 宠物进程并起配置桥。 |
| 客户端 bundle | client bundle | 插件里在 DSH 渲染进程跑的那一半，`client.js` 用 `window.__ModuleLoader__.load({...})` 注册 React 设置页（`plugins.bundle.config` slot）。 |
| 冷却闸门 | cooldown gate | 两次推送之间的最小间隔（3 秒）；闸门内的变化攒成一次最新推送，避免连续刷新闪屏。 |
| 指纹 / 去重 | scene signature | 画面内容哈希；与上一帧相同则不推，避免无意义刷新。 |
| 稳定等待 | settle | 连上设备后先等 1.2 秒、再最多 4×450ms 等状态稳定，然后推首帧。 |
| 旧形态清理 | legacy cleanup | 首次启动时删除旧面板/托盘时代的启动项与快捷方式，不留残留。 |
| Add plugin | 安装入口 | DSH Plugins 页的安装对话框，支持包名/Git 仓库/本地目录。 |
