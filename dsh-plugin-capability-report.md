# DSH 桌面版插件能力调查报告（只读调查）

目标：第三方本地 Rust exe（局域网墨水屏连接程序）能否随 DSH 启动/退出自动启停。

## 0. 结论速览

**可行，且推荐用「Host 插件 + `ctx.subprocess`」**，不需要修改 DSH 安装、不需要签名、不需要 native 代码：
一个约 80 行的纯 JS bundle，在 `apply()` 里 `ctx.subprocess.spawn()` 拉起 exe，在 `ctx.effect` 的 disposer 里 `terminate()`。
DSH 正常退出、Host 崩溃、Electron 主进程被强杀，三条路径都会走到 disposer；Windows 上还叠加 **Job Object** 内核级兜底（runner 一死，整棵进程树被 OS 杀掉）。

| 排序 | 方案 | 代码量 | 失败点 |
|---|---|---|---|
| A（推荐） | Host 插件 + `ctx.subprocess.spawn` | ~80 行 JS | 需装插件；HMR/config 变更会重跑 `apply`，exe 要单实例 |
| B（零代码，有前提） | 配置型 bundle 插 `@deepseek-ai/dsh-mcp-client`，`transport: stdio` | 0 行 | exe 必须讲 MCP JSON-RPC（stdio） |
| C（不改 DSH） | 外部看门狗轮询 Host 进程命令行 | 一个后台程序 | 有轮询延迟；需自启；多用户/多安装路径 |
| D（最脆） | 快捷方式包装脚本 | 一个 bat | 自动更新 `app.relaunch()`、开始菜单启动都会绕过 |

---

## 1. 环境实测（先纠正一个前提）

- `E:\Apps\DeepSeek Harness\resources\app.asar\dsh\` **不是目录**：`app.asar` 是 213,202,918 字节的单文件包（22,102 个内部条目），普通 `read`/`glob` 直接 IO error。只能用 asar 解析读取（本次用自建只读工具 `C:\Users\liujj\AppData\Local\Temp\dsh-asar\asar.js`）。
- 真正 unpacked 到磁盘的只有原生模块：`E:\Apps\DeepSeek Harness\resources\app.asar.unpacked\dsh\node_modules\`（`node-pty`、`@koromix/koffi-win32-x64`、`@img/sharp-win32-x64`、`sherpa-onnx-win-x64`、`@vscode/ripgrep-win32-x64/bin/rg.exe` 等）。
- **插件真身**在 profile 目录：`C:\Users\liujj\.dsh\profiles\desktop\`。`package.json` 的 `dsh.profile.bundles` =
  `["@deepseek-ai/dsh-base","@deepseek-ai/dsh-web-app","@deepseek-ai/dsh-experimental-agent-team-profile","@jingyi0605/codingns4dsh","billion-context"]`。
  插件文件装在 `C:\Users\liujj\.dsh\profiles\desktop\node_modules\`（**普通磁盘目录，不打包进 asar**，所以可以带 exe）。
- Electron 外壳自己的配置目录：`C:\Users\liujj\AppData\Roaming\@deepseek-ai\dsh-desktop\`，含 `desktop\settings.json`、`plugin-operations\operations.jsonl`、`logs\crash-*.log`、`background-close-confirmed`。

---

## 2. Q1 —— manifest、安装位置、生命周期、权限

### manifest = 普通 npm package.json + `dsh` 字段

内置官方文档（随应用发布）：`dsh/node_modules/@deepseek-ai/dsh-agent-preset/skills/cordis-plugin-development/references/host-plugin.md` 原文：

> A bundle is a package whose `package.json` declares `dsh.bundle.patch`; the YAML patch inserts plugin entries.

> A Host-only bundle needs no dependencies, install scripts, or build tool:

```json
{
  "name": "@local/my-plugin",
  "version": "1.0.0",
  "private": true,
  "type": "module",
  "exports": { ".": "./index.js" },
  "dsh": { "bundle": { "patch": "./cordis.patch.yml" } }
}
```

```yaml
- insert:
    - id: my-plugin
      name: '@local/my-plugin'
      config: {}
```

真实第三方实例：`C:\Users\liujj\.dsh\profiles\desktop\node_modules\billion-context\package.json` →
`"dsh": { "bundle": { "patch": "./dsh.bundle.patch.yml" }, "client": { "platform": "web" } }`。

### 生命周期：没有 onStart/onStop/activate/deactivate/dispose 这些命名

- 插件形状（`cordis/lib/types/registry.js:4-6,110-114`）：`isApplicable` 判 `typeof object.apply === 'function'`，`if (typeof plugin === 'function') return plugin; if (isApplicable(plugin)) return plugin.apply;`
  → 插件就是 `export function apply(ctx, config) {}`（可带 `export const inject`、`export const Config`）。
- **启动钩子 = `apply()` 在插件树挂载时被调用**；**退出钩子 = `ctx.effect` 的 disposer**。host-plugin.md 原文：
  > Register every resource inside `apply` with `ctx.effect` or `ctx.on` and return its cleanup.
- 官方退出链路真的会 dispose：`dsh-desktop-host/lib/index.js` 内
  `const stop = () => stopping ??= (async () => { … await running?.shutdown.shutdown(0); await send({type:"shutdown-complete"}); … })()`，
  触发点 `process.on("message", m => m.type === "shutdown" && stop())` 与 `process.once("disconnect", () => stop())`。
  后者意味着：**Electron 主进程被任务管理器杀掉 → IPC 断开 → Host 仍会 dispose 插件**。
- dispose 预算：`dsh/lib/profile-boot-e2mS45fC.js:13` `PROCESS_SHUTDOWN_TIMEOUT_MS = 5e3`，超时直接 `process.exit(code)`（`SIGTERM`/`SIGINT` 也走同一路径）。

### 进程边界：插件在 Host 子进程，不在主进程、不在渲染进程

实测（本机正在运行的三个关键进程）：

| PID | 命令行 | 身份 |
|---|---|---|
| 30512 | `"E:\Apps\DeepSeek Harness\DeepSeek Harness.exe"` | Electron 主进程 |
| 32800 | `"…DeepSeek Harness.exe" --expose-internals "…\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js" "…\app.asar\dsh" C:\Users\liujj\.dsh\profiles\desktop "…\runtime\primary-runtime" "…\pnpm.mjs" "…\runtime\bin"` | **Host 进程 = 插件运行处** |
| 51060 | `…\dsh-subprocess-local\lib\runner.js -- pwsh.exe …` | Host 拉起的外部进程 |

渲染端只有 `dsh.client`（`platform: "web"`）UI 插件，注入 `window.__DSH_BOOT__`，**没有 Node 权限**。

### Node 权限

Host 是普通 Node 进程（`ELECTRON_RUN_AS_NODE=1`），**无沙箱、无权限模型**：`child_process`、`net`（`\\.\pipe\*`）、`http`、`fs`、`ffi` 全可用。
现网证据：插件 `@jingyi0605/codingns4dsh` 已经用同一个 exe 起常驻子进程 —— PID 38876 `"…DeepSeek Harness.exe" C:\Users\liujj\.dsh\profiles\desktop\node_modules\@jingyi0605\codingns4dsh\data\build\dist\host\cli-bridge\mcp-stdio-entry.js`。

---

## 3. Q2 —— 有没有「启动时执行命令」的官方机制

**没有任何 `--autorun`/`--startup`、没有启动脚本键、没有 settings 键。**

- `dsh/lib/bin.js`（11,772 B，全文）：只有 `profile`（boot）、`plugin`（转发 pnpm）、`dump-config*` 三类模式；flags 仅 `--profile`、`--from-default-profile`、`--patch`、`--dump-config`、`--dump-config-schema`、`--dump-default-config`、`-V`。
- Electron 外壳设置只有一项：`lib/main.js:18367`
  `const defaults = { updates: { allowTestAuthPopupWindow: false } };`
  本机 `…\@deepseek-ai\dsh-desktop\desktop\settings.json` 实测内容就是 `{"updates":{"allowTestAuthPopupWindow":false}}`。
- 官方唯一扩展点 = 插件树。patch 层序（`dsh-app-boot/lib/index.js:1908-1917`）：
  1. bundle 自带 patches（按 `dsh.profile.bundles` 顺序）
  2. profile 自己的 `cordis.patch.yml`
  3. **`$DSH_HOME/cordis.patch.yml`**（全局层，对所有 profile 生效）
  4. `--patch` 叠加层
  5. telemetry 补丁

结论：**「DSH 启动时执行命令」在官方语义里就等于「某个插件的 `apply()` 执行命令」**。

---

## 4. Q3 —— 与外部进程通信、常驻

官方子进程 seam：`@deepseek-ai/dsh-subprocess`，service 名 `ctx.subprocess`（由 `dsh-base/cordis.patch.yml:234-235` 的 `- id: subprocess / name: '@deepseek-ai/dsh-subprocess-local'` 在**每个 profile 里默认挂载**）。

- 声明（`dsh-subprocess/lib/index.js` 原文）：
  > Subprocess seam (`ctx.subprocess`): execution-world executable lookup, fully specified provider-managed process ranges with raw or collected stdio, and one terminal-process primitive.

  > Disposal of the service terminates all still-running managed processes and awaits their exit.
- 调用：`spawn(spec)` **同步返回 handle**（`dsh-subprocess-local/lib/index.js:1547`）；spec 字段实测为
  `{ argv, stdio: { stdin, stdout, stderr, control }, env, cwd, graceMs, signal }`（`runner-launch-*.js:764-775` 校验 `graceMs` 必填、`argv[0]` 非空；`resolveExecutable` 要求带分隔符的路径必须是绝对路径）。
  handle 有 `terminate()` / `waitForExit()` / `done`。
- **Windows 上是 Job Object 容器**：`selectContainmentMode("ordinary")` → `if (probeWindowsJob()) return "windows-job"`（`:1596-1598`），实现 `launchWindowsJob` 让 `runner.js` 唯一持有 Job handle，`terminateForHostExit()` 直接 `runner.kill("SIGKILL")` → Job 关闭 → **整棵进程树由 OS 杀死**（连孙进程都跑不掉）。探测失败才退化为 `taskkill` 式的 fallback 并 warn。
- 环境清洗：`scrubbedParentEnv()` 剔除 `/KEY|PASSWORD|SECRET|TOKEN/i` 命名的变量和所有 `DSH_*`（"harness identity never leaks implicitly"），显式 `env` 在其后合并。
- Host 级兜底：`dsh-subprocess-local/lib/index.js:1477-1486`
  `ctx.effect(() => { const onHostExit = () => this.terminateForHostExit(); process.prependListener("exit", onHostExit); return async () => { await this.disposeManagedProcesses(); process.off("exit", onHostExit); }; }, "local subprocess teardown")`
- 通信手段（插件侧想用哪种都行）：`stdio` 的 `collect`/`piped`/`control` 三种模式、命名管道（`net`）、HTTP loopback、UDP 广播（局域网墨水屏常用）。
- 常驻：子进程进入服务内部 `this.live` 表，只要 Host 活着就一直被管理；不是"跑完即回收"。

---

## 5. Q4 —— 主进程启停会不会调到插件

会，但是**间接**，且有几个必须知道的语义坑：

1. `before-quit` 会拦你：`lib/main.js:21314`
   `app.on("before-quit", (event) => { if (shellInstallerOwnsQuit) return; if (quitting) return; event.preventDefault(); … quitConfirmation.confirm() })`
   → 退出前弹"还有任务在跑"确认框，用户**取消**则什么都不会发生（exe 也不该停）。
2. Host 停止阶梯：`lib/main.js:10127 async stop(requireGraceful = false)` → IPC `shutdown` → `exitsWithin(exited, 1e4)`（10 s）→ `child.kill("SIGTERM")` → 5 s → `SIGKILL` → 超时抛 `"dsh desktop host did not exit after SIGKILL"`。
3. 插件 dispose 只在 Host 内发生，预算是 5 s（见 §2）。
4. **关窗口 ≠ 退出**：`lib/main.js:21108`
   `window.on("close", (event) => { if (quitting || shellInstallerOwnsQuit || sessionEnding) return; event.preventDefault(); … hideMainWindow(window) })`
   → 窗口隐藏到托盘，进程与 Host 继续跑；提示语原文 `backgroundNoticeBody: "Running tasks will continue. You can reopen the window from the system tray."`（`lib/main.js:13804`）。只有注销/关机（`window.on("session-end")` 置 `sessionEnding = true`）才允许真退。

**因此「DSH 关闭就停 exe」必须绑定进程生命周期，不能绑窗口。**

---

## 6. Q5 —— 打包 / 签名 / 启用

- **能携带 native exe**：profile 的 `node_modules` 是普通目录，`files`/`exports` 里的文件原样落盘，无 asar 重打包。图标限制：SVG/PNG/JPEG/WebP ≤ 256 KiB 且必须在包内。
- **无代码签名、无人工审核**（本地目录 / tarball / git 都能直接装，见 `dsh-plugin-manager/lib/types/install-spec.js` 的 `parseInstallSpec`：registry、git、tarball、`file:`/`link:`/绝对路径）。
- 只有两道门：
  1. 版本门禁 `evaluatePluginCompatibility`（`dsh-app-boot/lib/index.js:1129`）：优先 `engines.dsh` 区间，否则逐个 `@deepseek-ai/dsh*` peerDependencies；不满足拒绝安装/启动（可 version exemption 豁免）。
  2. pnpm 构建脚本审批 `allowBuilds`（`build-approval.js`：`UNDECIDED_BUILD = 'set this to true or false'`）。把 exe 直接放进包里就**不需要 postinstall**，也就绕开这道审批。
- 启用方式：
  - agent 侧工具 `plugin_manager`：`install_bundle`（target 传绝对包目录）、`set_bundle`、`set_plugin`、`remove_bundle`；
  - UI：Settings → Plugins / Plugin Manager / Plugin Market；
  - CLI：`dsh plugin --profile <name> add <pkg>` —— 注意它**只是转发 pnpm**，且 `--profile desktop` 会被直接拒绝（`error: profile "desktop" is managed exclusively by the Electron application`），只有桌面 carrier 带 `manageDesktopProfile: true` 才能改。
- 坏插件兜底：致命启动失败会弹三按钮对话框（`lib/main.js:15963-15967`）：`exitApplication / restartApplication / **disableThirdPartyPlugins**`，并写 `%APPDATA%\@deepseek-ai\dsh-desktop\logs\crash-*.log`。
- 非必需插件加载失败**不会**拖垮启动：`auditStartupEntries`（`dsh-app-boot/lib/index.js:5216-5227`）只对 `requiredStartupEntryIds`（agent-loop、webserver、web-runtime、modules、connection…）抛 `StartupError`，其余只 warn。
  **但**运行期未捕获 rejection 会杀 Host：`installFailLoud`（`:4963-5001`）→ `<bin>: fatal load failure` → `proc.exit(1)`。

---

## 7. Q6 —— 替代方案事实核查

| 检查项 | 结果 |
|---|---|
| `--autorun` / `--startup` | 不存在（`bin.js` 全文无此 flag） |
| `dsh serve` / `dsh run` | 不存在；只有 `profile` / `plugin` / `dump-config*` |
| 单实例 | 有：`lib/main.js:15226-15233` `if (!application.requestSingleInstanceLock()) { application.quit(); return false; }` + `on("second-instance", focusOwner)` |
| DSH 自身开机自启 | 无：代码无 `setLoginItemSettings`；`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 实测没有 DeepSeek Harness 条目 |
| 固定端口 | 无：Host 固定 `--port 0`（随机）。4143 是 Web GUI dev server，不能当桌面信号 |
| pid / port 文件 | 无 |
| 启动日志 | 默认不写；仅启动失败写 `$DSH_HOME/logs/startup-<ISO>-<uuid>.log`（本机 `C:\Users\liujj\.dsh` 下无 `logs` 目录） |
| 可监视的可靠信号 | **只有进程命令行**（见下表） |

可用的进程匹配字符串（实测原文）：

- Host（= 插件树已加载）：
  `"E:\Apps\DeepSeek Harness\DeepSeek Harness.exe" --expose-internals "E:\Apps\DeepSeek Harness\resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js" "E:\Apps\DeepSeek Harness\resources\app.asar\dsh" C:\Users\liujj\.dsh\profiles\desktop "…\runtime\primary-runtime" "…\pnpm.mjs" "…\runtime\bin"`
  → 匹配 `dsh-desktop-host\lib\index.js` + profile 目录即可，与其他 Electron 子进程（`--type=gpu-process` / `renderer` / `utility`）天然区分。
- Electron 主进程：同一 exe，**无 `--type=`**、无其他 argv。

---

## 8. Q7 —— 路径排序与推荐

### A. Host 插件 + `ctx.subprocess.spawn`（推荐，生产）

依赖：一个 JS bundle（`package.json` + `cordis.patch.yml` + `index.js` + 随包 exe），用 `plugin_manager install_bundle` 装进 desktop profile。
权限：与 Host 同权（用户级），无需管理员。
实现要点：

```js
export const inject = ['subprocess']
export function apply(ctx, config) {
  const exe = fileURLToPath(new URL('./bin/inkbridge.exe', import.meta.url))
  ctx.effect(() => {
    let handle
    try {
      handle = ctx.subprocess.spawn({ argv: [exe, '--stdio-bridge'], stdio: { stdin: 'ignore', stdout: 'collect', stderr: 'collect' }, graceMs: 3000 })
    } catch (error) { ctx.logger.warn(`inkbridge: ${String(error)}`) }   // 绝不让 apply reject
    return async () => { try { handle?.terminate(); await handle?.waitForExit() } catch {} }
  }, 'inkbridge lifecycle')
}
```

失败点与对策：
- **HMR / 改 config 会重跑 `apply`**（host-plugin.md："Installing a new bundle can activate through HMR"、"Editing an ordinary field restarts the plugin"）→ exe 必须自带单实例保护（命名互斥量 / 命名管道握手），否则会起两份。
- **`apply` 里抛错**只影响自己（warn），但**异步漏出的 rejection** 会 `exit(1)` 杀 Host → spawn 必须同步 try/catch，异步错误自己 catch。
- **dispose 只有 5 s** → 先 `terminate()`（Job 关闭即死），不要等优雅 flush。
- exe 放在包内用 `import.meta.url` 定位（profile `node_modules` 是普通目录，`fileURLToPath` 可用）；spawn 要求绝对路径。
- 建议在 `package.json` 写 `engines.dsh`（当前 runtime 0.2.2-rc.1）防止未来大版本静默失效。

### B. 配置型 bundle 插 MCP client（零代码，前提是 exe 讲 MCP）

官方文档 `references/mcp-bundle.md` 原文：

> For stdio, use `transport: stdio`, `command`, and optional `args`, `env`, and `cwd`.

```yaml
- insert:
    - id: inkbridge-mcp
      name: '@deepseek-ai/dsh-mcp-client'
      config: { serverName: inkbridge, transport: stdio, command: 'C:\path\inkbridge.exe', args: ['--mcp'], failOnStartupError: true }
```

- 生命周期由 MCP client 托管：`close()` 走 stdin.end → 2 s → SIGTERM → 2 s → SIGKILL（`@modelcontextprotocol/client/dist/stdio.mjs`）；断线有重连策略 `RECONNECT_DEFAULTS = { enabled: true, initialDelayMs: 500, maxDelayMs: 3e4, maxAttempts: 10 }`。
- 环境是白名单继承（`DEFAULT_INHERITED_ENV_VARS`），`windowsHide: true`。
- 代价：exe 必须实现 MCP JSON-RPC over stdio；否则不适用。

### C. 外部看门狗（完全不碰 DSH）

轮询/WMI 监视 `dsh-desktop-host\lib\index.js` 那个进程 → 起 exe；进程消失 → 杀 exe。
失败点：轮询延迟（1–2 s 空窗）、自身需要自启（计划任务/启动项）、DSH 升级后路径变化、多 profile/多用户、exe 崩溃后要不要重启的策略。

### D. 快捷方式包装脚本

`start inkbridge.exe` → `DeepSeek Harness.exe` → `wait` → `taskkill`。
失败点致命：自动更新走 `app.relaunch()`（`lib/main.js:19956`）、单实例第二次启动直接退出、从开始菜单/托盘启动会绕过包装，实际不可靠。

**推荐 A**；若 Rust 端愿意实现 MCP stdio，B 是最省事的零代码方案；C 只作为"绝不允许改 DSH profile"时的兜底。
