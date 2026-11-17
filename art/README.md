# 鳄鱼母版（AI 生成）

这里保存 `assets/croc/default/*.png` 的母版，以及追溯信息。

- 生成：codex `image-gen` 技能（`~/.codex/skills/image-gen/SKILL.md`），模型 `gpt-image-2`，`--mode edit`。
  - `idle` 以 `docs/reference/croc-style/ref-coffee.png` 为参考图；
  - 其余五张以 `idle` 为角色基准逐张编辑，保证同一只 gator。
- 母版规格：`gen-v3/` 下 512×512 白底黑白线稿（原生成 1254×1254，入库前缩到 512 以控制仓库体积）。
- 转换：仓库根目录执行

  ```powershell
  python scripts/prep_croc_art.py            # 生成 assets/croc/default/*.png 与 plugin/icon.png
  python scripts/prep_croc_art.py --masters  # 首次：把 1254px 原图缩到 512px
  ```

  脚本会取六张并集墨迹框统一裁剪、等比缩放到 90×120 内、按阈值（默认 170）二值化，并居中。

- 重新生成：把新的六张 1254×1254 线稿覆盖 `gen-v3/`，先跑 `--masters`，再重跑一次转换脚本；然后 `cargo build --release` 并把 exe 复制到 `plugin/bin/`。
- 只换形象、不改代码：也可以直接用同尺寸（90×120、L 模式、0/255）PNG 覆盖 `assets/croc/default/`，见 ADR 0007。
