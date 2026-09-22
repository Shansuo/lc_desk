#!/usr/bin/env python3
"""生成 LC-Deck 的 macOS 应用图标（.icns）。

不依赖任何第三方库：用有符号距离场（SDF）直接渲染，自带抗锯齿，
再手写 PNG 编码，最后交给系统自带的 iconutil 打包成 .icns。

配色取自应用主题（深底 + teal 强调色），改主题后可重跑本脚本。

用法:
    python3 scripts/gen-icon.py [输出目录]      # 默认 scripts/icon-build
"""

import math
import os
import struct
import subprocess
import sys
import zlib

# ---- 画布与配色（与 src/theme.rs 保持一致）----
CANVAS = 1024
BG_TOP = (0x2A, 0x32, 0x40)
BG_BOTTOM = (0x12, 0x16, 0x1C)
SCREEN_FILL = (0x0A, 0x0D, 0x12)
SCREEN_STROKE = (0xE6, 0xED, 0xF7)
STAND = (0xC9, 0xD4, 0xE3)
ACCENT = (0x45, 0xD4, 0xBF)
INK = (0x0A, 0x0D, 0x12)

# macOS 图标网格：图形本身是圆角方形，四周留出边距
INSET = 100
RADIUS_RATIO = 0.2245  # 圆角半径 ≈ 宽度的 22.45%，接近系统 squircle

# 鼠标指针轮廓（局部坐标，y 向下），tip 在原点
CURSOR = [
    (0.0, 0.0),
    (0.0, 28.0),
    (7.0, 21.0),
    (11.5, 30.5),
    (16.5, 28.0),
    (12.0, 19.0),
    (19.5, 19.0),
]
CURSOR_TIP = (415.0, 348.0)
CURSOR_SCALE = 270.0 / 28.0  # 指针高度 270px（收小些，避免盖住显示器底座）
CURSOR_OUTLINE = 13.0

SIZES = [16, 32, 64, 128, 256, 512, 1024]


def clamp(v, lo=0.0, hi=1.0):
    return lo if v < lo else hi if v > hi else v


def aa(d):
    """有符号距离 → 覆盖率。约定：d<0 在形状内；过渡带 1px 抗锯齿。

    形状外扩 k → 新距离 d-k；内缩 k → 新距离 d+k。
    """
    return clamp(0.5 - d)


def sd_round_rect(px, py, cx, cy, hw, hh, r):
    qx = abs(px - cx) - (hw - r)
    qy = abs(py - cy) - (hh - r)
    ax, ay = max(qx, 0.0), max(qy, 0.0)
    outside = math.hypot(ax, ay)
    inside = min(max(qx, qy), 0.0)
    return outside + inside - r


def sd_polygon(px, py, pts):
    n = len(pts)
    d2 = 1e18
    sign = 1.0
    j = n - 1
    for i in range(n):
        ax, ay = pts[i]
        bx, by = pts[j]
        ex, ey = bx - ax, by - ay
        wx, wy = px - ax, py - ay
        t = clamp((wx * ex + wy * ey) / (ex * ex + ey * ey))
        dx, dy = wx - ex * t, wy - ey * t
        d2 = min(d2, dx * dx + dy * dy)
        # 穿越计数：判定点在多边形内还是外
        if (py >= ay) != (py >= by):
            if px < ex * (py - ay) / (by - ay) + ax:
                sign = -sign
        j = i
    return sign * math.sqrt(d2)


def over(dst, src, cov):
    """source-over 合成（直通 alpha）。dst 为 0..1 的 (r,g,b,a)，src 为 0..1 的 rgb。

    cov 是源的覆盖率（0..1），dst 保持预乘前的直通表达。
    """
    if cov <= 0.0:
        return dst
    sa = cov
    r, g, b, da = dst
    oa = sa + da * (1.0 - sa)
    if oa <= 0.0:
        return (0.0, 0.0, 0.0, 0.0)
    return (
        (src[0] * sa + r * da * (1.0 - sa)) / oa,
        (src[1] * sa + g * da * (1.0 - sa)) / oa,
        (src[2] * sa + b * da * (1.0 - sa)) / oa,
        oa,
    )


def nrm(rgb):
    return (rgb[0] / 255.0, rgb[1] / 255.0, rgb[2] / 255.0)


def render(size):
    """渲染指定边长的 RGBA 像素缓冲（几何按 1024 画布定义，按比例缩放）。"""
    k = size / CANVAS
    cx = cy = CANVAS / 2.0
    half = (CANVAS - 2 * INSET) / 2.0
    radius = (CANVAS - 2 * INSET) * RADIUS_RATIO

    # 显示器 / 支架 / 底座
    scx, scy, shw, shh = 512.0, 460.0, 250.0, 170.0
    screen_r, stroke = 28.0, 20.0
    stand = (512.0, 630.0, 684.0, 26.0)   # 中心x, 上边, 下边, 半宽
    base = (512.0, 684.0, 712.0, 104.0)   # 中心x, 上边, 下边, 半宽

    # 指针顶点（1024 空间）+ 包围盒（用于剪枝）
    cpts = [(CURSOR_TIP[0] + x * CURSOR_SCALE, CURSOR_TIP[1] + y * CURSOR_SCALE)
            for x, y in CURSOR]
    pad = CURSOR_OUTLINE + 2.0
    cbox = (min(p[0] for p in cpts) - pad, max(p[0] for p in cpts) + pad,
            min(p[1] for p in cpts) - pad, max(p[1] for p in cpts) + pad)

    g_top, g_bottom = nrm(BG_TOP), nrm(BG_BOTTOM)
    scr_fill, scr_stroke = nrm(SCREEN_FILL), nrm(SCREEN_STROKE)
    stand_col, accent, ink = nrm(STAND), nrm(ACCENT), nrm(INK)

    buf = bytearray(size * size * 4)
    for y in range(size):
        py = (y + 0.5) / k
        row = y * size * 4
        for x in range(size):
            pxx = (x + 0.5) / k

            # 1) 圆角方形底：形状外保持全透明
            d_bg = sd_round_rect(pxx, py, cx, cy, half, half, radius)
            cov_bg = aa(d_bg)
            if cov_bg <= 0.0:
                continue

            # 垂直渐变 + 顶部淡高光 + 内侧 3px 描边（深色桌面上有轮廓）
            t = clamp((py - (cy - half)) / (2.0 * half))
            col = (
                g_bottom[0] + (g_top[0] - g_bottom[0]) * t,
                g_bottom[1] + (g_top[1] - g_bottom[1]) * t,
                g_bottom[2] + (g_top[2] - g_bottom[2]) * t,
                cov_bg,
            )
            col = over(col, (1.0, 1.0, 1.0), clamp(1.0 - t * 2.2) * 0.06)
            col = over(col, (1.0, 1.0, 1.0), (aa(d_bg + 3.0) - aa(d_bg)) * 0.10)

            # 2) 显示器：先填屏面，再叠一圈描边（外框减去内缩后的框）
            if 240.0 <= pxx <= 784.0 and 270.0 <= py <= 660.0:
                d_scr = sd_round_rect(pxx, py, scx, scy, shw, shh, screen_r)
                col = over(col, scr_fill, aa(d_scr))
                col = over(col, scr_stroke, aa(d_scr) - aa(d_scr + stroke))

            # 3) 支架与底座
            if 380.0 <= pxx <= 644.0 and py >= 620.0:
                d_stand = sd_round_rect(
                    pxx, py, stand[0], (stand[1] + stand[2]) / 2,
                    stand[3] / 2, (stand[2] - stand[1]) / 2, 8.0)
                col = over(col, stand_col, aa(d_stand))
                d_base = sd_round_rect(
                    pxx, py, base[0], (base[1] + base[2]) / 2,
                    base[3], (base[2] - base[1]) / 2, 14.0)
                col = over(col, stand_col, aa(d_base))

            # 4) 指针：先铺深色描边（外扩）再填强调色，保证压在描边上也清晰
            if cbox[0] <= pxx <= cbox[1] and cbox[2] <= py <= cbox[3]:
                d_cur = sd_polygon(pxx, py, cpts)
                col = over(col, ink, aa(d_cur - CURSOR_OUTLINE))
                col = over(col, accent, aa(d_cur))

            i = row + x * 4
            buf[i] = int(clamp(col[0]) * 255.0 + 0.5)
            buf[i + 1] = int(clamp(col[1]) * 255.0 + 0.5)
            buf[i + 2] = int(clamp(col[2]) * 255.0 + 0.5)
            buf[i + 3] = int(clamp(col[3]) * 255.0 + 0.5)
    return bytes(buf)


def write_png(path, size, rgba):
    def chunk(tag, data):
        crc = zlib.crc32(tag + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", crc)

    stride = size * 4
    raw = b"".join(b"\x00" + rgba[y * stride:(y + 1) * stride] for y in range(size))
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 9))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def main():
    out_dir = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "icon-build"
    )
    iconset = os.path.join(out_dir, "LC-Deck.iconset")
    os.makedirs(iconset, exist_ok=True)

    for size in SIZES:
        rgba = render(size)
        # iconset 需要 @1x 与 @2x 两套命名
        if size <= 512:
            write_png(os.path.join(iconset, f"icon_{size}x{size}.png"), size, rgba)
        if size >= 32:
            write_png(os.path.join(iconset, f"icon_{size // 2}x{size // 2}@2x.png"), size, rgba)
        print(f"  渲染 {size}×{size}")

    icns = os.path.join(out_dir, "LC-Deck.icns")
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns], check=True)
    print(f"==> 已生成 {icns}")


if __name__ == "__main__":
    main()
