"""把 AI 生成（art/gen-v3）的 gator 线稿转成墨水屏素材。

用法：
  python scripts/prep_gator_art.py            # 生成 assets/gator/default/*.png 与 plugin/icon.png
  python scripts/prep_gator_art.py --masters  # 额外把 1254px 原图缩到 512px（瘦身仓库）

规则：
- 六张图共用同一裁剪框（六张并集的墨迹 bbox），保证角色比例一致。
- 等比缩放到 90×120 内，居中放到 90×120 白底画布。
- LANCZOS 缩放后按阈值二值化（默认 170，线稿偏粗一点更耐墨水屏）。
"""
import argparse
import glob
import os
from PIL import Image, ImageOps

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "art", "gen-v3")
OUT = os.path.join(ROOT, "assets", "gator", "default")
ICON = os.path.join(ROOT, "plugin", "icon.png")
ACTIONS = ["idle", "working", "waiting", "done", "error", "sleep"]
W, H = 90, 120


def ink_bbox(im, thr=200):
    return ImageOps.invert(im).point(lambda p: 255 if p > 255 - thr else 0).getbbox()


def union_box(images):
    pad = max(8, round(images[0].width * 0.019))  # 1254px 时约 24px，随素材尺寸等比
    boxes = [ink_bbox(im) for im in images]
    x0 = min(b[0] for b in boxes) - pad
    y0 = min(b[1] for b in boxes) - pad
    x1 = max(b[2] for b in boxes) + pad
    y1 = max(b[3] for b in boxes) + pad
    w, h = images[0].size
    return (max(0, x0), max(0, y0), min(w, x1), min(h, y1))


def to_mono(im, box, size, thr):
    crop = im.crop(box)
    scale = min(size[0] / crop.width, size[1] / crop.height)
    tw = max(1, round(crop.width * scale))
    th = max(1, round(crop.height * scale))
    small = crop.resize((tw, th), Image.LANCZOS)
    bw = small.point(lambda p: 0 if p < thr else 255, mode="L")
    canvas = Image.new("L", size, 255)
    canvas.paste(bw, ((size[0] - tw) // 2, (size[1] - th) // 2))
    return canvas


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--thr", type=int, default=170)
    ap.add_argument("--masters", action="store_true", help="把 1254px 原图缩到 512px")
    args = ap.parse_args()

    images = {}
    for a in ACTIONS:
        images[a] = Image.open(os.path.join(SRC, a + ".png")).convert("L")
    box = union_box(list(images.values()))

    os.makedirs(OUT, exist_ok=True)
    for a in ACTIONS:
        to_mono(images[a], box, (W, H), args.thr).save(os.path.join(OUT, a + ".png"))

    # 图标：用 idle 自己的墨迹框，取头部（上半部分，含眼睛和吻部）做 128×128 头像
    idle_box = ink_bbox(images["idle"])
    idle_box = (idle_box[0], idle_box[1], idle_box[2], idle_box[3])
    head_h = int((idle_box[3] - idle_box[1]) * 0.56)
    head = images["idle"].crop((idle_box[0], idle_box[1] - 8, idle_box[2], idle_box[1] + head_h))
    pad = 12
    canvas = Image.new("L", (head.width + pad * 2, head.height + pad * 2), 255)
    canvas.paste(head, (pad, pad))
    icon = to_mono(canvas, (0, 0, canvas.width, canvas.height), (128, 128), args.thr)
    icon.save(ICON)

    if args.masters:
        for a in ACTIONS:
            images[a].resize((512, 512), Image.LANCZOS).save(os.path.join(SRC, a + ".png"))

    print(f"crop box: {box}  threshold: {args.thr}")
    for a in ACTIONS:
        print(a, os.path.getsize(os.path.join(OUT, a + ".png")), "bytes")


if __name__ == "__main__":
    main()
