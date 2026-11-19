# 开发 checkpoint

更新日期：2026-11-20（Asia/Shanghai）

## 恢复入口

- 功能与验收以 [SPEC.md](SPEC.md) 为唯一来源；审计结果见 [AUDIT-2026-11-18.md](AUDIT-2026-11-18.md)。
- 本次从 `C:\Users\liujj\AppData\Local\Temp\dsh-pet-handoff-20261119-1210.md` 接手，代码基线为 `main` 的 `688a2f3`，版本 0.8.3。

## 本次完成

- 用户确认设置页完整表单正常，没有 `Failed to fetch`；证据已补入 FR-SET-01、FR-SET-02。
- 配置桥 `/health` 返回 `{"ok":true,"version":"0.8.3"}`。
- 设置页验收阶段只更新了验收文档与 checkpoint，没有改设置、安装副本或版本；之后按已确认规格完成了下列多任务代码实现。
- 多任务鳄鱼布局已实现：每页最多 3 项，2 项起保留 90×120 鳄鱼；提问/审批等待反白，等待优先并回首页，跨页计数和鳄鱼动作反映全部活跃任务。
- Rust 121 项测试通过，release 构建通过；插件 25 项测试逐文件通过。六张可追踪的多任务正式渲染预览在 `docs/screens/multi-task/`。
- 用户于 2026-11-19 确认真机显示效果良好。`cargo fmt --check` 无法运行，因为当前 stable 工具链未安装 rustfmt；`git diff --check` 通过。

## 后续工作

- 用户通过 `/grill-me` 确认了多任务鳄鱼布局，详见 [规格](specs/multi-task-gator-layout.md)。本地 3 张票已完成；用户已确认真机效果。
- 正式预览目录：`docs/screens/multi-task/`；生成器：`examples/multi_task_preview.rs`。本地构建预览也保存在被忽略的 `out/multi-task-gator-layout-rendered/`。
- 已知工具限制：当前 stable toolchain 没有 rustfmt；`npm test` 的 Node 测试调度器被子进程 `EPERM` 阻止，但两个测试文件分别运行通过。
- 一次误运行的 `preview --help` 被 CLI 当作实际预览命令，命令期间用户目录 `state.json` 的时间戳发生变化；备份步骤报访问拒绝。未读取或回滚该文件，也未继续访问用户缓存。
- 交接中的可选项为 GitHub Actions CI、SDK tag 固定和 `binarySha256`；尚未选择实施范围。
- 新功能先更新 SPEC 和任务票，再按用户确认的范围实施。多任务改版已提交并推送到 `mine/main`（`05cddb8`），未安装、未发布，也未重启 DSH。

## Git 写入结论（2026-11-20）

- 2026-11-19 的 `.git/index.lock: Permission denied` 根因是沙箱只允许宿主文件工具写入仓库子目录，shell 子进程对 `src/`、`docs/`、`.git` 等已有子目录无写权限；不是 NTFS ACL 拒绝。`.git` 上两个历史 SID 的 DENY 项未匹配当前令牌，目录属主也仍是当前账户。
- 以放宽的沙箱模式一次性完成暂存、提交与推送后验证：本地 HEAD 与 `mine/main` 同为 `05cddb8`，工作区干净；`origin/main` 仍领先 67 个提交，未向 origin 推送。
- 后续会话若回到受限沙箱，git 写入仍会失败；应在当前运行时边界内申请放宽，或改用宿主文件工具做只读检查。

## 操作约束

- 远端 `mine` 指向本项目；`origin` 是上游，不向其推送。
- 安装更新只用 DSH `plugin_manager`；更换插件 JS 或 exe 后需完全退出并重启 DSH。
- Rust 构建依赖相邻目录 `../zectrix-local-push-sdk`；删除 worktree 前先处理 SDK junction，避免删除真实 SDK。
