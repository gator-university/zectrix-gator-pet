# 03: 图标/截图/SPEC/ADR 与 0.6.1 版本收尾

**What to build:** 新鳄鱼形象与中英品牌行同步到产品外观与文档：插件图标换成新鳄鱼头像，README 头图与截图重渲染，SPEC 增加 FR-CROC-05 并更新证据，记录 ADR 0007（素材契约 30×40@3× → 90×120@1×），版本升到 0.6.1。

**Blocked by:** 01（新素材）、02（新版式）

**Status:** ready-for-agent

- [ ] `plugin/icon.png` 为 128×128 单色新鳄鱼头像，`./icon` 导出与 `files` 打包不变；插件清单仍能通过安装/启用
- [ ] README 头图与 `docs/screens/{live,working,waiting,done,error,stopped}.png` 用最终版式与形象重渲染
- [ ] `docs/SPEC.md`：新增 FR-CROC-05（形象 v2 风格基线），更新 FR-CROC-01/02/03、FR-UI-05 的状态与证据；版本标注 0.6.1
- [ ] 新增 `docs/adr/0007-croc-asset-contract-90x120.md`，记录契约变更与理由；GLOSSARY 如涉术语同步
- [ ] `Cargo.toml` 与 `plugin/package.json` 版本同步 0.6.1，`cargo test` 全过，release 构建成功
- [ ] 三次提交在集成分支上可读（素材/版式/收尾），工作区干净
