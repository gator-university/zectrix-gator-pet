# 02: 品牌行中英双行（鳄鱼大宠物 + Zetrix / DSH pet）

**What to build:** 设备左上角同时显示中文品牌「鳄鱼大宠物」与英文产品名「Zetrix / DSH pet」；中文为主行、英文为副行，右侧模式徽章不变，版式其余元素顺势下移但不压到底部信息。

**Blocked by:** None (can start immediately)

**Status:** ready-for-agent

- [ ] 左上第一行「鳄鱼大宠物」12px，第二行「Zetrix / DSH pet」9px；两行左对齐，英文视觉权重低于中文
- [ ] 分隔线在两个品牌行之下；状态大字/多任务 heading 与分隔线之间保留呼吸空间，不重叠
- [ ] 单任务、双任务、3+ 任务、等待、完成、出错、待命各页面均不越界，底部两态信息仍完整可见
- [ ] `src/layout.rs` 测试更新：断言场景文本同时包含「鳄鱼大宠物」与「Zetrix / DSH pet」，且英文基线 < 分隔线 < 状态大字基线
- [ ] `cargo test` 全过；生成新版式预览 `out/v7-<state>.png` 作为证据
