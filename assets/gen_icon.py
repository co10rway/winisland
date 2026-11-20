#!/usr/bin/env python3
# 从 dynamic-island.svg 栅格化生成多尺寸 icon.ico（纯标准库，无第三方依赖）
# 用法: /usr/bin/python3 gen_icon.py <svg输入(仅取样式参考，实际按下方硬编码布局绘制)> <out.ico>
import zlib, struct, math, sys

def lerp(a, b, t):
    return tuple(int(round(a[i] + (b[i] - a[i]) * t)) for i in range(3))

def grad3(p, c0t0, c1t1, c2t2):
    (c0, t0), (c1, t1), (c2, t2) = c0t0, c1t1, c2t2
    if p <= t0: return c0
    if p >= t2: return c2
    if p <= t1:
        return lerp(c0, c1, (p - t0) / (t1 - t0))
    return lerp(c1, c2, (p - t1) / (t2 - t1))

BASE0 = (0x20, 0x2B, 0x48); BASE1 = (0x09, 0x0D, 0x18)
A0 = (0x54, 0xF5, 0xF2); A1 = (0x56, 0x8B, 0xFF); A2 = (0xB1, 0x6B, 0xFF)
DARK = (0x08, 0x0C, 0x17); CYAN = (0x65, 0xFA, 0xEF); WHITE = (255, 255, 255)

def base_grad(px, py):
    ux = (px - 8) / 240.0; uy = (py - 8) / 240.0
    return lerp(BASE0, BASE1, max(0.0, min(1.0, (ux + uy) / 2.0)))

def aurora(t):
    return grad3(max(0.0, min(1.0, t)), (A0, 0.0), (A1, 0.52), (A2, 1.0))

def rrect_sdf(px, py, x, y, w, h, r):
    cx, cy = x + w / 2.0, y + h / 2.0
    hw, hh = w / 2.0 - r, h / 2.0 - r
    dx, dy = abs(px - cx) - hw, abs(py - cy) - hh
    ox, oy = max(dx, 0.0), max(dy, 0.0)
    return math.hypot(ox, oy) + min(max(dx, dy), 0.0) - r

def circle_in(px, py, cx, cy, r):
    return (px - cx) ** 2 + (py - cy) ** 2 <= r * r

# 逐点合成：按画家算法把图层（RGBA）叠加到 (0,0,0,0)
def compose(px, py):
    layers = []
    d = rrect_sdf(px, py, 8, 8, 240, 240, 56)
    if d <= 0:
        c = base_grad(px, py)
        di = rrect_sdf(px, py, 11, 11, 234, 234, 53)
        if 0.0 < di < 1.5:
            c = lerp(c, WHITE, 0.09)
        layers.append((c, 1.0))
    if rrect_sdf(px, py, 32, 94, 192, 74, 37) <= 0:
        layers.append((aurora((px - 32) / 192.0), 0.15))
    if rrect_sdf(px, py, 32, 88, 192, 72, 36) <= 0:
        layers.append((aurora((px - 32) / 192.0), 1.0))
    if rrect_sdf(px, py, 38, 94, 180, 60, 30) <= 0:
        layers.append((DARK, 1.0))
    if circle_in(px, py, 75, 124, 8):
        layers.append((CYAN, 1.0))
    bars = [(154, 119, 10), (172, 113, 22), (190, 118, 12)]
    gt = aurora((px - 150) / 44.0)
    for bx, by1, blen in bars:
        if rrect_sdf(px, py, bx - 4, by1 - 4, 8, blen + 8, 4) <= 0:
            layers.append((gt, 1.0))
            break
    # over 合成
    R = G = B = A = 0.0
    for (cr, cg, cb), ca in layers:  # ca 为 0..1，乘 alpha
        a = ca + A * (1 - ca)
        if a <= 0: continue
        R = (cr * ca + R * A * (1 - ca)) / a
        G = (cg * ca + G * A * (1 - ca)) / a
        B = (cb * ca + B * A * (1 - ca)) / a
        A = a
    return (R, G, B, A)

def render(res, ss=3):
    S = res / 256.0
    out = bytearray(res * res * 4)
    n = ss * ss
    for y in range(res):
        for x in range(res):
            R = G = B = A = 0.0
            for sy in range(ss):
                py = (y + (sy + 0.5) / ss) * S
                for sx in range(ss):
                    px = (x + (sx + 0.5) / ss) * S
                    r, g, b, a = compose(px, py)
                    R += r * a; G += g * a; B += b * a; A += a
            R /= n; G /= n; B /= n; A /= n
            i = (y * res + x) * 4
            if A <= 0.0:
                out[i:i + 4] = bytes((0, 0, 0, 0))
            else:
                out[i] = int(R / A); out[i + 1] = int(G / A); out[i + 2] = int(B / A)
                out[i + 3] = int(A * 255)
    return bytes(out)

def png_encode(rgba, w, h):
    raw = bytearray()
    stride = w * 4
    for y in range(h):
        raw.append(0)
        raw.extend(rgba[y * stride:(y + 1) * stride])
    comp = zlib.compress(bytes(raw), 9)
    def chunk(typ, data):
        c = struct.pack('>I', len(data)) + typ + data
        c += struct.pack('>I', zlib.crc32(typ + data) & 0xffffffff)
        return c
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 6, 0, 0, 0)) + chunk(b'IDAT', comp) + chunk(b'IEND', b'')

def downscale(src, sw, dw):
    scale = sw / dw
    out = bytearray(dw * dw * 4)
    for y in range(dw):
        y0, y1 = int(y * scale), int((y + 1) * scale)
        for x in range(dw):
            x0, x1 = int(x * scale), int((x + 1) * scale)
            acc = [0, 0, 0, 0]; cnt = 0
            for yy in range(y0, y1):
                row = yy * sw
                for xx in range(x0, x1):
                    i = (row + xx) * 4
                    a = src[i + 3]
                    acc[0] += src[i] * a; acc[1] += src[i + 1] * a; acc[2] += src[i + 2] * a; acc[3] += a
                    cnt += 1
            oi = (y * dw + x) * 4
            if acc[3] <= 0:
                out[oi:oi + 4] = bytes((0, 0, 0, 0))
            else:
                out[oi] = int(acc[0] / acc[3]); out[oi + 1] = int(acc[1] / acc[3]); out[oi + 2] = int(acc[2] / acc[3])
                out[oi + 3] = int(acc[3] / cnt)
    return out

def write_ico(path, sizes):
    imgs = [(px, png_encode(rgba, px, px)) for px, rgba in sizes]
    header = struct.pack('<HHH', 0, 1, len(imgs))
    off = 6 + 16 * len(imgs)
    entries = b''; data = b''
    for px, png in imgs:
        b = 0 if px >= 256 else px
        entries += struct.pack('<BBBBHHII', b, b, 0, 0, 1, 32, len(png), off)
        data += png; off += len(png)
    with open(path, 'wb') as f:
        f.write(header + entries + data)

def main():
    res = 256
    print('render', res)
    big = render(res, ss=3)
    sizes = [(256, big)]
    for s in (64, 48, 32, 24, 16):
        print('downscale', s)
        sizes.append((s, downscale(big, res, s)))
    write_ico(sys.argv[2], sizes)
    print('done', sys.argv[2])

if __name__ == '__main__':
    main()
