# 01: 鳄鱼素材契约 v2（90×120 @1×）与六动作新形象

**What to build:** 设备右下角的鳄鱼换成参考图风格的圆头厚描边卡通形象：素材直接按 90×120 单色绘制，渲染时 1:1 画在 (300,150)，六个动作（idle/working/waiting/done/error/sleep）各有可辨认的姿势与表情，屏上不糊、不越界。

**Blocked by:** None (can start immediately)

**Status:** ready-for-agent

- [ ] `assets/croc/default/` 六个动作均为 90×120 单色 PNG（L 模式），主角是 gator（短吻鳄，alligator）：圆头、**短而宽的 U 形吻部**（不是长尖吻 croc，不外露齿排）、2px 黑描边、高光眼、浅色肚皮；风格符合 `docs/reference/croc-style/`；六张是同一只角色的不同姿态
- [ ] 动作语义：idle 端咖啡杯、working 敲笔记本、waiting 举手/问号、done 星星/点赞、error 绷带或 × 眼、sleep 闭眼 + Z
- [ ] 渲染路径改为 1:1（缩放参数传 1），画在 (300,150)，不超出 400×300 画布、不压到底部信息线
- [ ] `src/sprite.rs` 测试更新：尺寸 90×120、墨量比例 6%–50%、包围盒 ≥ 40×60、六动作互不相同、未知动作不画
- [ ] `cargo test` 全过（≥67 项）；`preview --demo <state>` 六张 PNG 生成到 `out/croc-v2-<state>.png` 作为视觉证据
- [ ] 版式层不因素材变大而越界（`layouts_obey_hub_limits` 等现有测试仍通过）
