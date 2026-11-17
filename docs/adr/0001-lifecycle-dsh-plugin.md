# ADR 0001：宠物进程随 DSH 生命周期（插件 spawn + Job + 自检）

- 状态：Accepted（2026-11-18）
- 相关：FR-LIFE-01..04

## 背景

旧形态用「启动文件夹 vbs + 计划任务看门狗 + 独立面板」，用户明确要求更安全、低侵入：DSH 打开就打开，DSH 关闭就关闭；不要再有常驻命令行/托盘/计划任务。

DSH 桌面版的可扩展点是 cordis 插件树（bundle）。插件运行在 Host 子进程（`ELECTRON_RUN_AS_NODE=1`），在 `apply(ctx)` 里可以 `ctx.subprocess.spawn()`；Windows 下 DSH 用 Job Object 做 kill-on-close。

## 决策

1. 本仓库同时交付：Rust `dsh-pet.exe`（无界面 daemon/CLI）+ 一个纯 JS 的 DSH Host bundle。bundle 的 `apply()` 用 `ctx.subprocess.spawn()` 拉起 exe（`watch --watch-pid <host pid>`），`ctx.effect` 的 disposer 里 `terminate()`。
2. exe 自身再兜一层：每 2 秒检查 `--watch-pid`（或 `--parent`）进程是否存活，存活失败立即退出；并保持命名互斥体单实例，防止 HMR/配置变更重跑 `apply` 起两份。
3. 不依赖 DSH 的优雅退出：正常退出、Host 崩溃、主进程被强杀（IPC disconnect）、Job 关闭，四条路径任一生效即可。
4. 不再保留托盘/独立面板/开机自启/计划任务；旧形态的启动项、快捷方式在升级时清理。

## 备选与理由

- 外部看门狗轮询 Host 进程：需自启、有轮询延迟、多安装路径不可靠 → 只作为 exe 自检的实现方式，不做独立程序。
- 快捷方式包装：自动更新/开始菜单启动会绕过 → 否。
- MCP stdio：零代码但要求 exe 讲 MCP JSON-RPC，且推送/手势/设备状态不是 MCP 语义 → 否。

## 后果

- 插件安装/更新需重启 DSH；开发期用本地 link bundle 验证。
- 若 DSH 未来改掉 `ctx.subprocess` 或 Job 行为，需要重新验证兜底路径。
