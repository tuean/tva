"""生成 TVA 应用图标。纯标准库，不依赖 Pillow。
设计：macOS 圆角方块 + 蓝紫渐变底 + 白色麦克风剪影。
用有向距离场做抗锯齿，一次采样即可。
输出：resources/TVA.icns、resources/icon-1024.png
"""
import math, os, struct, subprocess, zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RES = os.path.join(ROOT, "resources")
N = 1024


def rr_sdf(x, y, cx, cy, hw, hh, r):
    qx, qy = abs(x - cx) - (hw - r), abs(y - cy) - (hh - r)
    return (math.hypot(max(qx, 0.0), max(qy, 0.0))
            + min(max(qx, qy), 0.0) - r)


def cap_sdf(x, y, cx, y0, y1, r):
    a, b = y0 + r, y1 - r
    cy = a if y <= a else (b if y >= b else y)
    return math.hypot(x - cx, y - cy) - r


def arc_sdf(x, y, cx, cy, rad, t):
    th = t * 0.5
    f = abs(math.hypot(x - cx, y - cy) - rad) - th
    if y < cy:
        f = max(f, cy - y)
    c1 = math.hypot(x - (cx - rad), y - cy) - th
    c2 = math.hypot(x - (cx + rad), y - cy) - th
    return min(f, c1, c2)


GLYPH_SCALE = 1.12
GLYPH_CY = 0.5125


def glyph_sdf(x, y):
    xs = (x - 0.5) / GLYPH_SCALE + 0.5
    ys = (y - GLYPH_CY) / GLYPH_SCALE + GLYPH_CY
    d = min(cap_sdf(xs, ys, 0.5, 0.215, 0.545, 0.105),
            arc_sdf(xs, ys, 0.5, 0.455, 0.215, 0.062),
            cap_sdf(xs, ys, 0.5, 0.600, 0.760, 0.032),
            rr_sdf(xs, ys, 0.5, 0.776, 0.155, 0.035, 0.035))
    return d * GLYPH_SCALE


def cover(f, size):
    a = 0.5 - f * size
    return 0.0 if a < 0.0 else (1.0 if a > 1.0 else a)


def render(size):
    out = bytearray(size * size * 4)
    for py in range(size):
        v = (py + 0.5) / size
        row = py * size * 4
        for px in range(size):
            u = (px + 0.5) / size
            a_bg = cover(rr_sdf(u, v, 0.5, 0.5, 0.5, 0.5, 0.2237), size)
            if a_bg <= 0.0:
                continue
            a_gl = cover(glyph_sdf(u, v), size)
            t = min(max((u + v) * 0.5, 0.0), 1.0)
            t = t * t * (3.0 - 2.0 * t)
            r = 0.427 + (0.278 - 0.427) * t
            g = 0.545 + (0.310 - 0.545) * t
            b = 1.000 + (0.878 - 1.000) * t
            r = r * (1.0 - a_gl) + a_gl
            g = g * (1.0 - a_gl) + a_gl
            b = b * (1.0 - a_gl) + a_gl
            o = row + px * 4
            out[o] = int(r * 255.0 + 0.5)
            out[o + 1] = int(g * 255.0 + 0.5)
            out[o + 2] = int(b * 255.0 + 0.5)
            out[o + 3] = int(a_bg * 255.0 + 0.5)
    return out


def png(path, size, pix):
    raw = bytearray()
    for y in range(size):
        raw.append(0)
        raw += pix[y * size * 4:(y + 1) * size * 4]

    def ch(tag, data):
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))

    head = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    blob = (b"\x89PNG\r\n\x1a\n" + ch(b"IHDR", head)
            + ch(b"IDAT", zlib.compress(bytes(raw), 9)) + ch(b"IEND", b""))
    with open(path, "wb") as f:
        f.write(blob)


def preview(pix, size, cols=52):
    rows = cols // 2
    lines = []
    for ry in range(rows):
        s = ""
        for rx in range(cols):
            x0 = int(rx * size / cols)
            x1 = max(x0 + 1, int((rx + 1) * size / cols))
            y0 = int(ry * size / rows)
            y1 = max(y0 + 1, int((ry + 1) * size / rows))
            ta = tl = cnt = 0
            stx, sty = max(1, (x1 - x0) // 4), max(1, (y1 - y0) // 4)
            for yy in range(y0, y1, sty):
                for xx in range(x0, x1, stx):
                    o = (yy * size + xx) * 4
                    ta += pix[o + 3]
                    tl += (pix[o] * 299 + pix[o + 1] * 587 + pix[o + 2] * 114) // 1000
                    cnt += 1
            if cnt == 0 or ta / cnt < 20:
                s += " "
            else:
                lum = tl / cnt
                s += "." if lum < 90 else ("*" if lum < 170 else "#")
        lines.append(s)
    return "\n".join(lines)


def main():
    print("渲染 %dx%d ..." % (N, N))
    pix = render(N)
    master = os.path.join(RES, "icon-1024.png")
    png(master, N, pix)
    print(preview(pix, N))

    iconset = os.path.join(RES, "TVA.iconset")
    subprocess.run(["rm", "-rf", iconset], check=True)
    os.makedirs(iconset)
    plan = [(16, "icon_16x16.png"), (32, "icon_16x16@2x.png"),
            (32, "icon_32x32.png"), (64, "icon_32x32@2x.png"),
            (128, "icon_128x128.png"), (256, "icon_128x128@2x.png"),
            (256, "icon_256x256.png"), (512, "icon_256x256@2x.png"),
            (512, "icon_512x512.png"), (1024, "icon_512x512@2x.png")]
    for s, name in plan:
        out = os.path.join(iconset, name)
        if s == N:
            png(out, N, pix)
        else:
            subprocess.run(["sips", "-z", str(s), str(s), master, "--out", out],
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    icns = os.path.join(RES, "TVA.icns")
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns], check=True)
    subprocess.run(["rm", "-rf", iconset], check=True)
    print("已生成 %s (%d bytes)" % (icns, os.path.getsize(icns)))
    print("已生成 %s" % master)


main()
