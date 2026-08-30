#!/usr/bin/env python3
"""生成 y-sync 品牌资产：应用图标（icns/ico/png）+ 托盘状态图标。

用法: scripts/gen_icons.py            # 需 Pillow (python3 -m venv + pip install pillow)
输出: desktop/src-tauri/icons/*, desktop/src-tauri/tray/*.png
"""
import math
import os
import subprocess
import tempfile

from PIL import Image, ImageDraw, ImageFilter

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ICONS = os.path.join(ROOT, "desktop/src-tauri/icons")
TRAY = os.path.join(ROOT, "desktop/src-tauri/tray")

# 品牌色：靛蓝 → 天蓝 对角渐变
C1, C2 = (91, 95, 239), (56, 189, 248)
STATUS = {
    "gray": (156, 163, 175, 255),
    "green": (34, 197, 94, 255),
    "blue": (59, 130, 246, 255),
    "yellow": (245, 158, 11, 255),
    "red": (239, 68, 68, 255),
}


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def gradient(size, c1, c2):
    small = Image.new("RGB", (64, 64))
    px = small.load()
    for y in range(64):
        for x in range(64):
            px[x, y] = lerp(c1, c2, (x + y) / 126)
    return small.resize((size, size), Image.BILINEAR)


def sync_glyph(draw, cx, cy, r, w, color):
    """圆形双箭头同步符号（顺时针追逐）。Pillow 角度：3 点钟为 0、顺时针增大（y 向下）。"""
    w = int(round(w))
    box = [int(round(v)) for v in (cx - r, cy - r, cx + r, cy + r)]
    half = 62                        # 每段弧半跨度；两侧留 56° 箭头间隙
    top = (270 - half, 270 + half)
    bottom = (90 - half, 90 + half)
    for start, end in (top, bottom):
        draw.arc(box, start=start, end=end, fill=color, width=w)
        a = math.radians(start)      # 圆头只做在尾端，头端由箭头收尾
        x, y = cx + r * math.cos(a), cy + r * math.sin(a)
        draw.ellipse([x - w / 2, y - w / 2, x + w / 2, y + w / 2], fill=color)
    for ang in (top[1], bottom[1]):  # 箭头：底边在弧末端，尖端顺切向伸出
        a = math.radians(ang)
        x, y = cx + r * math.cos(a), cy + r * math.sin(a)
        tx, ty = -math.sin(a), math.cos(a)
        nx, ny = math.cos(a), math.sin(a)
        L, Wd = w * 1.85, w * 1.42
        draw.polygon([(x + tx * L, y + ty * L),
                      (x - nx * Wd / 2, y - ny * Wd / 2),
                      (x + nx * Wd / 2, y + ny * Wd / 2)], fill=color)


def app_icon(master=1024):
    canvas = Image.new("RGBA", (master, master), (0, 0, 0, 0))
    m = round(master * 0.10)          # macOS 图标四周留白
    side = master - 2 * m
    radius = round(side * 0.225)

    bg = gradient(master, C1, C2).convert("RGBA")
    mask = Image.new("L", (master, master), 0)
    ImageDraw.Draw(mask).rounded_rectangle([m, m, m + side, m + side], radius=radius, fill=255)
    canvas.paste(bg, (0, 0), mask)

    # 顶部轻微高光，增加层次
    hl = Image.new("RGBA", (master, master), (0, 0, 0, 0))
    ImageDraw.Draw(hl).rounded_rectangle(
        [m, m, m + side, m + side * 0.62], radius=radius,
        fill=(255, 255, 255, 26))
    hl_alpha = hl.split()[3]
    hl_alpha = hl_alpha.filter(ImageFilter.GaussianBlur(master * 0.02))
    white = Image.new("RGBA", (master, master), (255, 255, 255, 0))
    white.putalpha(hl_alpha)
    canvas = Image.alpha_composite(canvas, white)

    # 同步符号
    layer = Image.new("RGBA", (master, master), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    sync_glyph(d, master / 2, master / 2, master * 0.245, master * 0.078, (255, 255, 255, 255))
    canvas = Image.alpha_composite(canvas, layer)
    return canvas


def tray_icon(color, size=32, ss=8):
    """托盘状态图标：高分辨率绘制后缩放（32px，与历史尺寸一致）。"""
    big = size * ss
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    sync_glyph(d, big / 2, big / 2, big * 0.30, big * 0.105, color)
    return img.resize((size, size), Image.LANCZOS)


def main():
    os.makedirs(ICONS, exist_ok=True)
    os.makedirs(TRAY, exist_ok=True)

    icon = app_icon()
    png1024 = os.path.join(tempfile.mkdtemp(), "icon_1024.png")
    icon.save(png1024)

    # macOS iconset → icns
    iconset = os.path.join(tempfile.mkdtemp(), "y-sync.iconset")
    os.makedirs(iconset)
    sizes = {"16x16": 16, "16x16@2x": 32, "32x32": 32, "32x32@2x": 64,
             "128x128": 128, "128x128@2x": 256, "256x256": 256,
             "256x256@2x": 512, "512x512": 512, "512x512@2x": 1024}
    for name, s in sizes.items():
        icon.resize((s, s), Image.LANCZOS).save(os.path.join(iconset, f"icon_{name}.png"))
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(ICONS, "icon.icns")], check=True)

    # 其余格式
    icon.resize((512, 512), Image.LANCZOS).save(os.path.join(ICONS, "icon.png"))
    icon.resize((256, 256), Image.LANCZOS).save(os.path.join(ICONS, "128x128@2x.png"))
    icon.resize((128, 128), Image.LANCZOS).save(os.path.join(ICONS, "128x128.png"))
    icon.resize((32, 32), Image.LANCZOS).save(os.path.join(ICONS, "32x32.png"))
    icon.resize((256, 256), Image.LANCZOS).save(os.path.join(ICONS, "icon.ico"),
                                                sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])

    # 托盘
    for name, color in STATUS.items():
        tray_icon(color).save(os.path.join(TRAY, f"{name}.png"))

    print("✓ 图标生成完成:", ICONS, TRAY)


if __name__ == "__main__":
    main()
