#!/usr/bin/env python3
"""生成鳄鱼 v2 六动作素材（90×120，单色 L 模式）。

画法：在 4× 超采样画布上用 PIL 画硬边形状（不做抗锯齿），用
`MinFilter` 腐蚀得到 2px 描边，最后 NEAREST 下采样到 90×120。
输出只有 0（墨）和 255（纸）两种像素，阈值 <128 即墨。

同一套头/身/腿/尾模板派生六个动作，只换眼睛、嘴、手臂和道具，
保证六张图是同一只角色。

用法：
    <bundled-python> scripts/make_croc_v2.py
    <bundled-python> scripts/make_croc_v2.py --sheet out/croc-v2-sheet.png
    <bundled-python> scripts/make_croc_v2.py --icon      # plugin/icon.png
"""

from __future__ import annotations

import argparse
import math
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "assets" / "croc" / "default"

W, H = 90, 120
SS = 4  # 超采样倍数
INK = 0
PAPER = 255
T = 2  # 描边宽度（最终像素）

# 渲染插件图标时临时改动：几何整体乘 SCALE、平移 (OX, OY) 到方形画布 CW×CH。
# 动作素材一律用默认值（1.0 / 0 / 0 / 90×120），输出不变。
SCALE = 1.0
OX = 0.0
OY = 0.0
CW, CH = W, H


# ---------------------------------------------------------------------------
# 基础绘制工具（坐标一律用最终 90×120 像素，内部乘 SS）
# ---------------------------------------------------------------------------


def canvas() -> Image.Image:
    return Image.new("L", (CW * SS, CH * SS), PAPER)


def _box(x0: float, y0: float, x1: float, y1: float):
    return [
        round((x0 * SCALE + OX) * SS),
        round((y0 * SCALE + OY) * SS),
        round((x1 * SCALE + OX) * SS),
        round((y1 * SCALE + OY) * SS),
    ]


def _pts(points):
    return [(round((x * SCALE + OX) * SS), round((y * SCALE + OY) * SS)) for x, y in points]


def _mask():
    return Image.new("L", (CW * SS, CH * SS), 0)


def paint(img: Image.Image, mask: Image.Image, width: int | None = None) -> None:
    """把 mask 当作一个实心形状：内部刷白（覆盖后面的元素），再描黑边。"""
    width = T if width is None else width
    img.paste(PAPER, mask=mask)
    eroded = mask.filter(ImageFilter.MinFilter(width * SS * 2 + 1))
    img.paste(INK, mask=ImageChops.subtract(mask, eroded))


def rect(img: Image.Image, x0, y0, x1, y1, r=0, **kw) -> None:
    m = _mask()
    d = ImageDraw.Draw(m)
    if r:
        d.rounded_rectangle(_box(x0, y0, x1, y1), radius=round(r * SS), fill=255)
    else:
        d.rectangle(_box(x0, y0, x1, y1), fill=255)
    paint(img, m, kw.get("width", T))


def ellipse(img: Image.Image, x0, y0, x1, y1, **kw) -> None:
    m = _mask()
    ImageDraw.Draw(m).ellipse(_box(x0, y0, x1, y1), fill=255)
    paint(img, m, kw.get("width", T))


def circle(img: Image.Image, cx, cy, r, **kw) -> None:
    ellipse(img, cx - r, cy - r, cx + r, cy + r, **kw)


def poly(img: Image.Image, points, **kw) -> None:
    m = _mask()
    ImageDraw.Draw(m).polygon(_pts(points), fill=255)
    paint(img, m, kw.get("width", T))


def bar_mask(points, w):
    """粗线段（圆头）的 mask；单独返回，便于和手掌合成一个轮廓。"""
    m = _mask()
    ImageDraw.Draw(m).line(
        _pts(points), fill=255, width=round(w * SS), joint="curve"
    )
    r = w / 2
    for x, y in (points[0], points[-1]):
        ImageDraw.Draw(m).ellipse(_box(x - r, y - r, x + r, y + r), fill=255)
    return m


def bar(img: Image.Image, points, w=8, **kw) -> None:
    """粗线段（圆头），用作四肢/尾巴的骨架。"""
    paint(img, bar_mask(points, w), kw.get("width", T))


def ink(img: Image.Image, points, w=2, color=INK) -> None:
    """画一条黑线（细节/表情），不产生描边。"""
    ImageDraw.Draw(img).line(
        _pts(points), fill=color, width=max(1, round(w * SS)), joint="curve"
    )


def ink_circle(img: Image.Image, cx, cy, r, color=INK) -> None:
    ImageDraw.Draw(img).ellipse(_box(cx - r, cy - r, cx + r, cy + r), fill=color)


def ink_rect(img: Image.Image, x0, y0, x1, y1, color=INK, r=0) -> None:
    d = ImageDraw.Draw(img)
    if r:
        d.rounded_rectangle(_box(x0, y0, x1, y1), radius=round(r * SS), fill=color)
    else:
        d.rectangle(_box(x0, y0, x1, y1), fill=color)


def arc(img: Image.Image, x0, y0, x1, y1, start, end, w=2, color=INK) -> None:
    ImageDraw.Draw(img).arc(
        _box(x0, y0, x1, y1),
        start=start,
        end=end,
        fill=color,
        width=max(1, round(w * SS)),
    )


# ---------------------------------------------------------------------------
# 共用模板
# ---------------------------------------------------------------------------

BUMP_L = (34.0, 20.0, 11.0)
BUMP_R = (56.0, 20.0, 11.0)
HEAD = (16, 18, 74, 62)  # 主体轮廓
# gator：短、宽、圆的 U 形吻部。用「根部高、鼻尖矮」的梯形而不是等高的扁片，
# 否则在正脸 + 侧吻的构图里会读成鸭嘴。
SNOUT = [(27, 34), (11, 38), (4, 44), (2, 49), (4, 54), (11, 58), (27, 58)]
TORSO = (25, 52, 63, 104)
LEG_L = (25, 96, 44, 117)
LEG_R = (46, 96, 65, 117)


def draw_body(img: Image.Image) -> None:
    """尾巴 → 身体 → 腿。头单独画，压在身上形成下巴线。"""
    # 尾巴在最后面：宽厚的短尾，从身体右侧伸出去再收细。
    poly(img, [(50, 88), (76, 78), (88, 84), (82, 92), (58, 101)])
    rect(img, *TORSO, r=18)
    # 肚皮三条 1px 横线（墨水屏上就是浅色肚皮的分节）。
    for y in (78, 85, 92):
        ink(img, [(34, y), (54, y)], w=1.5)
    # 腿：从身体下沿伸出来的粗短腿，平脚掌 + 两道脚趾线。
    rect(img, *LEG_L, r=5)
    rect(img, *LEG_R, r=5)
    for x0 in (29, 50):
        ink(img, [(x0, 113), (x0 + 10, 113)], w=1.2)


def draw_head(img: Image.Image) -> None:
    """圆头 + 两个眼包 + 短吻，合成一个轮廓（同一只鳄鱼）。"""
    m = _mask()
    d = ImageDraw.Draw(m)
    d.rounded_rectangle(_box(*HEAD), radius=round(17 * SCALE * SS), fill=255)
    d.polygon(_pts(SNOUT), fill=255)
    # 圆鼻尖，让短吻的前端是圆的而不是刀切的。
    d.ellipse(_box(1.5, 44.5, 9.5, 53.5), fill=255)
    for cx, cy, r in (BUMP_L, BUMP_R):
        d.ellipse(_box(cx - r, cy - r, cx + r, cy + r), fill=255)
    paint(img, m)


def draw_nose(img: Image.Image) -> None:
    ink_circle(img, 10, 44, 1.2)
    ink_circle(img, 15, 45, 1.2)


def open_eye(img: Image.Image, cx, cy, look=(0.0, 0.0), r=7.5) -> None:
    ink_circle(img, cx, cy, r)  # 黑眼圈
    ink_circle(img, cx, cy, r - 2, PAPER)  # 眼白
    ink_circle(img, cx + look[0], cy + look[1], 3.2)  # 瞳孔
    ink_circle(img, cx + look[0] - 1.4, cy + look[1] - 1.4, 1.1, PAPER)  # 高光


def happy_eye(img: Image.Image, cx, cy) -> None:
    """弯弯的闭眼（∩）。"""
    arc(img, cx - 7, cy - 4, cx + 7, cy + 6, 190, 350, w=2.4)


def sleep_eye(img: Image.Image, cx, cy) -> None:
    """睡觉的闭眼：下压的眼皮线 + 两根短睫毛，和 done 的弯眼区分开。"""
    ink(img, [(cx - 6.5, cy + 1), (cx, cy + 2.5), (cx + 6.5, cy + 1)], w=2.2)
    for dx in (-6, 6):
        ink(img, [(cx + dx, cy + 3), (cx + dx, cy + 5.5)], w=1.0)


def x_eye(img: Image.Image, cx, cy) -> None:
    ink(img, [(cx - 6, cy - 6), (cx + 6, cy + 6)], w=3)
    ink(img, [(cx - 6, cy + 6), (cx + 6, cy - 6)], w=3)


def mouth_smile(img: Image.Image, big=False) -> None:
    # 嘴线沿下颌走，嘴角向后上方收：读作笑，而不是鸭嘴的一道缝。
    if big:
        ink(img, [(5, 49), (12, 54), (22, 55), (34, 51), (44, 44)], w=3.4)
    else:
        ink(img, [(6, 50), (13, 54), (22, 54), (32, 51), (40, 46)], w=3.0)


def mouth_flat(img: Image.Image) -> None:
    ink(img, [(7, 51), (16, 54), (26, 53), (33, 51)], w=2.4)


def mouth_frown(img: Image.Image) -> None:
    # ∩ 形撇嘴：中间高、两边低。
    ink(img, [(7, 53), (16, 49), (26, 53)], w=2.6)


def mouth_o(img: Image.Image) -> None:
    ink_circle(img, 14, 51, 3.2)
    ink_circle(img, 14, 51, 1.4, PAPER)


def draw_arm(img: Image.Image, points, w=8.5, kind="fist", hand_r=6.5) -> None:
    """手臂 + 手掌合成一张 mask 再描边：手腕处不会出现「棒棒糖」接缝。"""
    m = bar_mask(points, w)
    d = ImageDraw.Draw(m)
    hx, hy = points[-1]
    d.ellipse(_box(hx - hand_r, hy - hand_r, hx + hand_r, hy + hand_r), fill=255)
    if kind == "open":
        for dx in (-hand_r * 0.55, 0, hand_r * 0.55):
            d.ellipse(
                _box(hx + dx - 3.0, hy - hand_r - 4.5, hx + dx + 3.0, hy - hand_r + 1.5),
                fill=255,
            )
    elif kind == "thumb":
        d.rounded_rectangle(
            _box(hx - 3, hy - 13, hx + 3, hy - 4), radius=round(2 * SS), fill=255
        )
    paint(img, m)


# ---------------------------------------------------------------------------
# 道具
# ---------------------------------------------------------------------------


def coffee(img: Image.Image) -> None:
    # 杯把放在左侧，避免被手臂挡住。
    arc(img, 4, 75, 13, 85, 90, 270, w=2.2)
    rect(img, 12, 72, 27, 88, r=3)
    ink_rect(img, 14, 76, 25, 78, INK)  # 咖啡液面
    ink_rect(img, 12, 88, 29, 90, INK)  # 杯垫
    # 一缕热气
    ink(img, [(16, 70), (18, 66), (16, 62)], w=1.5)


def laptop(img: Image.Image) -> None:
    rect(img, 30, 64, 62, 88, r=3)  # 屏幕外框
    ink(img, [(35, 71), (46, 71)], w=1.6)
    ink(img, [(35, 76), (53, 76)], w=1.6)
    ink(img, [(35, 81), (48, 81)], w=1.6)
    rect(img, 23, 90, 69, 102, r=3)  # 键盘
    for x in range(27, 66, 5):
        ink_circle(img, x + 1.5, 95, 0.9)
        ink_circle(img, x + 1.5, 99, 0.9)


def question(img: Image.Image) -> None:
    ink(
        img,
        [(71, 10), (76, 5), (83, 7), (85, 13), (82, 19), (78, 22), (78, 26)],
        w=2.6,
    )
    ink_circle(img, 78.5, 31.5, 2.2)


def stars(img: Image.Image) -> None:
    def star(cx, cy, r):
        pts = []
        for i in range(8):
            ang = -math.pi / 2 + i * math.pi / 4
            rad = r if i % 2 == 0 else r * 0.28
            pts.append((cx + math.cos(ang) * rad, cy + math.sin(ang) * rad))
        ImageDraw.Draw(img).polygon(_pts(pts), fill=INK)

    star(11, 24, 7)
    star(81, 34, 6)
    star(70, 8, 5)


def zzz(img: Image.Image) -> None:
    # 一个清楚的 Z 就够了；两个小 Z 在 90px 下会糊成一道闪电。
    ink(img, [(70, 6), (84, 6), (70, 17), (84, 17)], w=2.4)


def bandage(img: Image.Image) -> None:
    # 斜贴一块创可贴：旋转后在独立小图上画好，再贴回。
    w, h = 22, 9
    pad_n = 4  # 留白再腐蚀，否则 MinFilter 会在贴图边界上失效、只剩几个角。
    pad = Image.new("L", ((w + 2 * pad_n) * SS, (h + 2 * pad_n) * SS), 0)
    d = ImageDraw.Draw(pad)
    d.rounded_rectangle(
        [pad_n * SS, pad_n * SS, (pad_n + w) * SS - 1, (pad_n + h) * SS - 1],
        radius=3 * SS,
        fill=255,
    )
    ring = ImageChops.subtract(pad, pad.filter(ImageFilter.MinFilter(T * SS * 2 + 1)))
    stamp = Image.new("L", pad.size, 255)
    stamp.paste(0, mask=ring)
    # 创可贴上的小孔
    sd = ImageDraw.Draw(stamp)
    for i in (-3, 3):
        cx = (pad_n + w // 2) * SS + i * SS
        cy = (pad_n + h // 2) * SS
        sd.ellipse([cx - 0.8 * SS, cy - 0.8 * SS, cx + 0.8 * SS, cy + 0.8 * SS], fill=0)
    rot = stamp.rotate(-24, resample=Image.NEAREST, expand=True, fillcolor=255)
    # 完全贴在右脸颊内部，不压头部轮廓：外侧描边保持完整。
    img.paste(rot, (round((48 - pad_n) * SS), round((32 - pad_n) * SS)))


def thumb_hand(img: Image.Image, points) -> None:
    draw_arm(img, points, kind="thumb")


# ---------------------------------------------------------------------------
# 六个动作
# ---------------------------------------------------------------------------


def draw_idle(img: Image.Image) -> None:
    draw_body(img)
    draw_head(img)
    draw_nose(img)
    mouth_smile(img)
    open_eye(img, BUMP_L[0], BUMP_L[1])
    open_eye(img, BUMP_R[0], BUMP_R[1])
    draw_arm(img, [(30, 66), (21, 82)])
    coffee(img)


def draw_working(img: Image.Image) -> None:
    draw_body(img)
    draw_head(img)
    draw_nose(img)
    mouth_flat(img)
    open_eye(img, BUMP_L[0], BUMP_L[1], look=(0.5, 2.2))
    open_eye(img, BUMP_R[0], BUMP_R[1], look=(0.5, 2.2))
    laptop(img)
    # 两只手搭在键盘上（手臂被笔记本挡住，只露出手）。
    circle(img, 33, 92, 5)
    circle(img, 57, 92, 5)


def draw_waiting(img: Image.Image) -> None:
    draw_body(img)
    draw_head(img)
    draw_nose(img)
    mouth_o(img)
    open_eye(img, BUMP_L[0], BUMP_L[1] - 0.5, look=(0.0, -1.5), r=8)
    open_eye(img, BUMP_R[0], BUMP_R[1] - 0.5, look=(0.0, -1.5), r=8)
    # 右臂贴着头的右缘举起来，张开手掌（三根手指合成轮廓）。
    draw_arm(img, [(62, 72), (82, 72), (82, 50)], kind="open")
    question(img)


def draw_done(img: Image.Image) -> None:
    draw_body(img)
    draw_head(img)
    draw_nose(img)
    mouth_smile(img, big=True)
    happy_eye(img, BUMP_L[0], BUMP_L[1])
    happy_eye(img, BUMP_R[0], BUMP_R[1])
    # 右手在头外侧比赞：拇指朝上，避开吻部与嘴线。
    thumb_hand(img, [(62, 72), (82, 72), (82, 58)])
    stars(img)


def draw_error(img: Image.Image) -> None:
    draw_body(img)
    draw_head(img)
    draw_nose(img)
    mouth_frown(img)
    x_eye(img, BUMP_L[0], BUMP_L[1])
    x_eye(img, BUMP_R[0], BUMP_R[1])
    bandage(img)


def draw_sleep(img: Image.Image) -> None:
    draw_body(img)
    draw_head(img)
    draw_nose(img)
    mouth_flat(img)
    sleep_eye(img, BUMP_L[0], BUMP_L[1])
    sleep_eye(img, BUMP_R[0], BUMP_R[1])
    zzz(img)


ACTIONS = {
    "idle": draw_idle,
    "working": draw_working,
    "waiting": draw_waiting,
    "done": draw_done,
    "error": draw_error,
    "sleep": draw_sleep,
}


def render(action: str) -> Image.Image:
    img = canvas()
    ACTIONS[action](img)
    small = img.resize((W, H), Image.NEAREST)
    # 强制二值：只留 0 / 255。
    return small.point(lambda p: INK if p < 128 else PAPER)


def stats(im: Image.Image) -> str:
    w, h = im.size
    raw = im.tobytes()
    ink = sum(1 for p in raw if p < 128)
    xs = [i % w for i, p in enumerate(raw) if p < 128]
    ys = [i // w for i, p in enumerate(raw) if p < 128]
    bbox = (min(xs), min(ys), max(xs) + 1, max(ys) + 1)
    return f"ink {ink / (w * h) * 100:.1f}%  bbox {bbox[2]-bbox[0]}×{bbox[3]-bbox[1]}"


def _threshold(img: Image.Image, size: int) -> Image.Image:
    return img.resize((size, size), Image.NEAREST).point(lambda p: INK if p < 128 else PAPER)


def _ink_bbox(img: Image.Image):
    return img.point(lambda p: 255 - p).getbbox()


def _head_layer() -> Image.Image:
    img = canvas()
    draw_head(img)
    draw_nose(img)
    mouth_smile(img)
    open_eye(img, BUMP_L[0], BUMP_L[1])
    open_eye(img, BUMP_R[0], BUMP_R[1])
    return img


def render_icon(size: int = 128, pad: int = 4) -> Image.Image:
    """从同一头型模板渲染方形单色图标：3px 描边、白底、NEAREST、无灰阶。

    复用动作素材的头/眼/嘴形状，按包围盒等比放大到 size×size 并居中，
    所以图标和屏幕上的鳄鱼是同一只，不会另起一套画风。
    """
    global SCALE, OX, OY, CW, CH, T
    saved = (SCALE, OX, OY, CW, CH, T)
    try:
        CW = CH = size
        T = 3
        # 先按 1:1 量头部包围盒，再等比缩放到留出 pad 的方形里并居中。
        SCALE, OX, OY = 1.0, 0.0, 0.0
        bbox = _ink_bbox(_threshold(_head_layer(), size))
        w, h = bbox[2] - bbox[0], bbox[3] - bbox[1]
        SCALE = (size - 2 * pad) / max(w, h)
        OX = (size - SCALE * (bbox[0] + bbox[2])) / 2
        OY = (size - SCALE * (bbox[1] + bbox[3])) / 2
        return _threshold(_head_layer(), size)
    finally:
        SCALE, OX, OY, CW, CH, T = saved


def greedy_rects(im: Image.Image) -> int:
    """按 src/sprite.rs 的贪心矩形算法数元素，提前发现超出场景预算。"""
    m = [[im.getpixel((x, y)) < 128 for y in range(H)] for x in range(W)]
    used = [[False] * H for _ in range(W)]
    n = 0
    for y in range(H):
        for x in range(W):
            if not m[x][y] or used[x][y]:
                continue
            w = 0
            while x + w < W and m[x + w][y] and not used[x + w][y]:
                w += 1
            h = 1
            while y + h < H and all(m[x + i][y + h] and not used[x + i][y + h] for i in range(w)):
                h += 1
            for yy in range(y, y + h):
                for xx in range(x, x + w):
                    used[xx][yy] = True
            n += 1
    return n


def sheet(path: Path) -> None:
    scale = 3
    gap = 8
    im = Image.new("L", (6 * W * scale + 7 * gap, H * scale + 2 * gap), 220)
    for i, action in enumerate(ACTIONS):
        a = render(action)
        a = a.resize((W * scale, H * scale), Image.NEAREST)
        im.paste(a, (gap + i * (W * scale + gap), gap))
    path.parent.mkdir(parents=True, exist_ok=True)
    im.save(path)
    print(f"sheet: {path}")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--sheet", type=Path, default=None)
    ap.add_argument(
        "--icon",
        action="store_true",
        help="另渲染 128×128 插件图标（默认写 plugin/icon.png）",
    )
    ap.add_argument("--icon-out", type=Path, default=ROOT / "plugin" / "icon.png")
    args = ap.parse_args()
    OUT.mkdir(parents=True, exist_ok=True)
    for action in ACTIONS:
        im = render(action)
        path = OUT / f"{action}.png"
        im.save(path, optimize=True)
        print(f"{action:8s} {path.relative_to(ROOT)}  {stats(im)}  rects {greedy_rects(im)}")
    if args.icon:
        icon = render_icon()
        args.icon_out.parent.mkdir(parents=True, exist_ok=True)
        icon.save(args.icon_out, optimize=True)
        try:
            shown = args.icon_out.resolve().relative_to(ROOT)
        except ValueError:
            shown = args.icon_out
        print(
            f"icon     {shown}  "
            f"{icon.size[0]}×{icon.size[1]}  {stats(icon)}"
        )
    if args.sheet:
        sheet(args.sheet)


if __name__ == "__main__":
    main()
