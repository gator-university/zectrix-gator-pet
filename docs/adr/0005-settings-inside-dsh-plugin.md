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
