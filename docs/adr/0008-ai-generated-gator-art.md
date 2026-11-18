# ADR 0008：鳄鱼素材改为 AI 生成线稿（v3）

- 状态：Accepted（2026-11-18）
- 相关：FR-GATOR-01/02/03/05、ADR 0007
- 取代：ADR 0007 里「`scripts/make_gator_v2.py` 程序化绘制」的做法（契约本身不变）

## 背景

ADR 0007 的 v2 素材由脚本按几何模板逐笔绘制。90×120 下虽然细节齐全，但观感偏硬、表情呆板；用户看完真机预览后明确否决：「鳄鱼太丑了，彻底放弃这一版」，要求用参考图让图像模型生成。

## 决策

- 母版：用 codex 的 `image-gen` 技能（`~/.codex/skills/image-gen/SKILL.md`，模型 gpt-image-2，edit 模式）生成六张 1254×1254 黑白线稿；`idle` 以用户参考图 `docs/reference/gator-style/ref-coffee.png` 为基准，其余五张以 `idle` 为角色基准逐张编辑，保证是同一只。
- 母版入库：`art/gen-v3/{idle,working,waiting,done,error,sleep}.png`，缩到 512×512 存仓库（PNG 二值线稿，压缩后每张 30–90 KB）；重新生成时可用 1254px 原图。
- 转换：`scripts/prep_gator_art.py`
  - 六张取并集墨迹 bbox（pad 随素材宽度等比）→ LANCZOS 缩放到 90×120 内 → 阈值二值化（默认 170，偏粗更耐墨水屏）→ 居中贴到 90×120 白底。
  - `--masters` 把 1254px 原图缩到 512px；同一脚本用 idle 的头部生成 `plugin/icon.png`（128×128）。
- 运行时契约不变：`assets/gator/default/{action}.png` 90×120 L 模式，`(300,152)` 1:1 绘制，`sprite.rs` 仍贪心矩形化（场景元素随线稿变化）。
- 旧脚本 `scripts/make_gator_v2.py` 删除；ADR 0007 作为历史记录保留。

## 替代方案

- **保留程序化绘制**：可控、可复现，但形象观感不达标，用户已否决。
- **直接用 1254px 原图**：没有 1-bit 设备意义，且元素数爆炸（墨水屏单帧刷新与 JSON 体积都吃亏）。
- **矢量化**：线稿转 SVG 再拆线段，理论上更省元素，但工具链与调参成本高，v1 不做。

## 修订：error 动作改为「轻微惊吓」表情（2026-11-18）

第一版 `error` 是「创可贴 + × 眼 + 汗」，用户看完上屏效果后说「看起来像被打了」，要求换成尴尬表情（参考 `docs/reference/gator-style/ref-awkward.png`：侧目、波浪嘴、无奈）。第二版做出尴尬苦笑后，用户又指出「不要这个两颊斜线，有点怪了，改为一个轻微的惊吓表情即可」。

处理：以 `art/gen-v3/idle.png` 为输入图，用同一个 gpt-image-2 edit 接口重新生成——去掉咖啡杯，保留同一只角色与站姿，只改神态：睁大的眼睛（保留高光）、略微抬起的眉毛、小圆嘴、额头一滴冷汗；**不画两颊斜线**，不带创可贴 / × 眼 / 星星 / 伤口。新母版覆盖 `art/gen-v3/error.png`（512px），再跑 `scripts/prep_gator_art.py` 转换。其余五张未动。

## 后果

- 形象与参考图一致（圆头短吻 gator、粗描边、高光眼、咖啡馆/工位等动作），六动作可一眼区分。
- 素材依赖 AI 生成流程：重新生成需要 image-gen 技能与网关凭据；仓库里保留脚本与母版即可复现转换，但重新「画」需要跑一次 codex。
- 元素数量随线稿密度变化，`layouts_obey_hub_limits` 的上限（400）继续覆盖。
