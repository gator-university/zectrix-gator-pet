# ADR 0005：设置页放进 DSH 插件，删除独立面板与托盘

- 状态：Accepted（2026-11-18）
- 相关：FR-SET-01..04

## 背景

旧版有独立 egui 控制面板 + 托盘图标 + 开机自启 vbs。用户要求「设置做成 DSH 插件内的页面」，并接受 exe 驻留随 DSH。

## 决策

- 删除独立面板、托盘图标、开机自启、启动文件夹/快捷方式；唯一用户入口是 DSH 的 Plugins 页 → 本插件详情页。
- 插件 = Host bundle（负责拉起 exe）+ 可选 client 页面（React，挂在插件详情页，样式用 DSH 主题 token）。
- 设置数据流：页面写 `%LOCALAPPDATA%\zectrix-dsh-pet\pet-config.json`；exe 每 2 秒轮询该文件并热生效。不通过 DSH 的 config editor 保存运行时设置（避免改字段重启插件/重启宠物进程）。
- 「测试连接」由插件 host 侧调用 `dsh-pet.exe check --json` 获取结果，经 host↔client 通道返回；若该通道能力不足，降级为写「测试请求」文件，页面显示请求已发出。

## 后果

- DSH 未启动时无法改设置、也无法手动启动宠物（这是用户明确要的低侵入行为）。
- 插件 client 页面需要写 UI（纯 JS React + 主题 token），不是零成本。
- 开发/验收仍保留 CLI 诊断命令，但不面向用户文档。

## 修订（2026-11-18，v0.7.3）

- 现象：插件卡片在「设置 → 内置插件」可见，但设置表单不出现。
- 根因：`plugin/package.json` 的 `dsh.client.inject: ["slots"]`。该字段声明的是「先加载哪些客户端模块包」（如 `@deepseek-ai/dsh-client-ui-settings`），写服务名 `slots` 不是可解析的模块 id，client bundle 因此没被加载，`plugins.bundle.config` 的注册从未执行。
- 修复：移除 `dsh.client.inject`（本页只依赖核心提供的 `slots` 服务，用工厂里的 `inject: ['slots']` 即可），注册包在 `ctx.effect(...)` 里以便 HMR/停用时释放。
- 教训：配置页位置在侧栏「插件」页的组合包详情（描述与行之间），不是「设置 → 内置插件」（那里只读列表）。
