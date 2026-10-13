#!/usr/bin/env python3
"""Draws the films' stage: a quiet pixel-art valley at 320x180, scaled x6 to 1920x1080.

Seeded and dependency-light (Pillow only), so the art is reproducible:
    python3 shared/stage.py            # writes shared/stage.png
"""
import math
import random
import sys
from pathlib import Path

from PIL import Image

W, H, SCALE = 320, 180, 6
SEED = 7

# A small, muted sunset palette: the stage frames the windows, it never competes with them.
P = {
    "sky0": (92, 80, 112),
    "sky1": (134, 98, 124),
    "sky2": (184, 116, 122),
    "sky3": (222, 146, 116),
    "sky4": (240, 186, 132),
    "sun": (252, 226, 172),
    "cloud": (240, 178, 150),
    "cloud_shade": (176, 112, 126),
    "far": (164, 112, 128),
    "far_snow": (196, 138, 140),
    "mid": (128, 86, 108),
    "mid_shade": (116, 78, 100),
    "hill": (98, 68, 90),
    "pine_far": (78, 56, 76),
    "pine": (74, 52, 62),
    "pine_dark": (54, 38, 50),
    "water": (204, 132, 122),
    "water_dark": (160, 100, 112),
    "water_glint": (252, 214, 160),
    "meadow": (172, 116, 72),
    "meadow_dark": (138, 88, 64),
    "grass": (206, 150, 86),
    "flower_a": (246, 204, 130),
    "flower_b": (236, 152, 128),
    "flower_c": (252, 232, 200),
}

BAYER = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]]


def noise1d(rng, n, octaves):
    """Smooth 1D value noise in [0, 1], as a list of n samples."""
    out = [0.0] * n
    amp_total = 0.0
    for period, amp in octaves:
        knots = [rng.random() for _ in range(n // period + 3)]
        for x in range(n):
            i, f = divmod(x / period, 1)
            i = int(i)
            f = f * f * (3 - 2 * f)
            out[x] += amp * (knots[i] * (1 - f) + knots[i + 1] * f)
        amp_total += amp
    return [v / amp_total for v in out]


def main(out_path):
    rng = random.Random(SEED)
    img = Image.new("RGB", (W, H))
    px = img.load()

    def put(x, y, colour):
        if 0 <= x < W and 0 <= y < H:
            px[x, y] = colour

    # Sky: eight close bands; each edge gets two rows of sparse dither (25%, then 75%).
    keys = ["sky0", "sky1", "sky2", "sky3", "sky4"]
    bands = []
    for i in range(8):
        t = i / 7 * (len(keys) - 1)
        a, b = P[keys[min(int(t), len(keys) - 2)]], P[keys[min(int(t), len(keys) - 2) + 1]]
        f = t - min(int(t), len(keys) - 2)
        bands.append(tuple(round(a[c] + (b[c] - a[c]) * f) for c in range(3)))
    horizon = 104
    step = 11
    for y in range(horizon + 1):
        band, offset = divmod(y, step)
        band = min(band, 7)
        for x in range(W):
            colour = bands[band]
            if band < 7 and offset == step - 2 and BAYER[y % 4][x % 4] < 4:
                colour = bands[band + 1]
            if band < 7 and offset == step - 1 and BAYER[y % 4][x % 4] < 12:
                colour = bands[band + 1]
            px[x, y] = colour

    # A low, pale sun with a one-pixel halo of dither.
    sx, sy, sr = 228, 70, 9
    for y in range(sy - sr - 3, sy + sr + 4):
        for x in range(sx - sr - 3, sx + sr + 4):
            d = math.hypot(x - sx, y - sy)
            if d <= sr:
                px[x, y] = P["sun"]
            elif d <= sr + 2.5 and (x + y) % 2 == 0:
                px[x, y] = P["sun"]

    # Clouds: unions of flat ellipses, with a shaded underside.
    def cloud(cx, cy, puffs):
        cells = set()
        for dx, dy, rx, ry in puffs:
            for y in range(cy + dy - ry, cy + dy + ry + 1):
                for x in range(cx + dx - rx, cx + dx + rx + 1):
                    if ((x - cx - dx) / rx) ** 2 + ((y - cy - dy) / ry) ** 2 <= 1:
                        cells.add((x, y))
        bottom = max(y for _, y in cells)
        for x, y in cells:
            if 0 <= x < W and 0 <= y < H:
                px[x, y] = P["cloud_shade"] if y >= bottom - 1 else P["cloud"]

    cloud(46, 22, [(0, 0, 14, 4), (10, -3, 9, 4), (-9, 1, 8, 3), (20, 1, 7, 3)])
    cloud(150, 12, [(0, 0, 11, 3), (8, -2, 7, 3), (-8, 1, 6, 2)])
    cloud(276, 30, [(0, 0, 13, 3), (-9, -2, 8, 3), (7, -3, 6, 3), (16, 1, 6, 2)])
    cloud(100, 48, [(0, 0, 8, 2), (6, -1, 5, 2)])

    # Ridgelines: far snowy range, then a nearer blue-green range, then forested hills.
    far = noise1d(rng, W, [(64, 1.0), (24, 0.5), (9, 0.2)])
    mid = noise1d(rng, W, [(48, 1.0), (16, 0.45), (6, 0.15)])
    hill = noise1d(rng, W, [(40, 1.0), (13, 0.3)])
    far_y = [int(58 + (1 - v) * 40) for v in far]
    mid_y = [int(78 + (1 - v) * 28) for v in mid]
    hill_y = [int(98 + (1 - v) * 12) for v in hill]

    for x in range(W):
        for y in range(far_y[x], horizon + 1):
            depth = y - far_y[x]
            cap = 72 - far_y[x]
            snow = depth < cap - 1 or (depth < cap + 1 and (x + y) % 2 == 0)
            px[x, y] = P["far_snow"] if snow else P["far"]
    for x in range(W):
        for y in range(mid_y[x], horizon + 1):
            # The lower slopes fade into haze with a short dither.
            haze = y > horizon - 8 and BAYER[y % 4][x % 4] < (y - horizon + 8) * 2
            px[x, y] = P["mid_shade"] if haze else P["mid"]
    for x in range(W):
        for y in range(hill_y[x], horizon + 1):
            px[x, y] = P["hill"]

    # Lake: the hills reflected, darker, broken by horizontal ripples.
    lake_top, lake_bottom = horizon + 1, 132
    for y in range(lake_top, lake_bottom):
        for x in range(W):
            src = 2 * horizon - y
            colour = P["water"]
            if src >= hill_y[x]:
                colour = P["water_dark"]
            if (y * 13 + x * 7) % 211 == 0 and y > lake_top + 4:
                colour = P["water_glint"]
            px[x, y] = colour
    for y in range(lake_top + 3, lake_bottom, 5):
        for x in range(W):
            if (x // 6 + y) % 4 == 0:
                px[x, y] = P["water_dark"]

    # The sun's reflection: broken strokes under it that narrow and thin out toward the shore.
    for y in range(lake_top + 2, lake_bottom - 2):
        depth = (y - lake_top) / (lake_bottom - lake_top)
        if (y - lake_top) % 2:
            continue
        half = int(sr * (1 - depth * 0.7))
        jitter = ((y * 7) % 5) - 2
        for x in range(sx - half + jitter, sx + half + jitter + 1):
            if (x + y // 2) % 7 != 0:
                put(x, y, P["water_glint"] if depth < 0.55 else P["sun"])

    # Pines: one sprite drawn at several heights, in two depth rows along the shores.
    def pine(bx, by, h, colour, shade, reflect_to=None):
        trunk = max(1, h // 9)
        body = h - trunk
        tiers = max(2, body // 6)
        tier_h = body / tiers
        drawn = []
        for i in range(tiers):
            # Each tier is a stepped triangle that overlaps the one below it.
            top = by - trunk - body + int(i * tier_h * 0.8)
            bottom = by - trunk - body + int((i + 1) * tier_h) + 1
            max_half = (h / 4.2) * (i + 1) / tiers + 1
            for y in range(top, bottom + 1):
                half = int(max_half * (y - top + 1) / (bottom - top + 1) + 0.5)
                for x in range(bx - half, bx + half + 1):
                    drawn.append((x, y, shade if x > bx or (x == bx + half and half > 0) else colour))
        for x, y, c in drawn:
            put(x, y, c)
        for y in range(by - trunk, by + 1):
            put(bx, y, P["pine_dark"])
        if reflect_to is not None:
            for x, y, _ in drawn:
                ry = 2 * by - y + 1
                if ry < reflect_to and (ry - by) % 3 != 0:
                    put(x, ry, P["water_dark"])

    for x in range(-4, W + 4, 5):
        if rng.random() < 0.55:
            pine(x + rng.randint(-1, 1), lake_top, rng.randint(9, 15), P["pine_far"], P["pine_far"], lake_top + 9)

    # Meadow: rolling foreground with a dithered edge, grass tufts and a few flowers.
    meadow = noise1d(rng, W, [(70, 1.0), (22, 0.3)])
    meadow_y = [int(lake_bottom + 2 + (1 - v) * 10) for v in meadow]
    for x in range(W):
        for y in range(lake_bottom, H):
            if y >= meadow_y[x]:
                px[x, y] = P["meadow"]
            elif y >= meadow_y[x] - 2 and (x + y) % 2 == 0:
                px[x, y] = P["meadow"]
            else:
                px[x, y] = P["water"] if y < lake_bottom + 3 else P["meadow_dark"]
    for x in range(W):
        for y in range(meadow_y[x], H):
            share = (y - 160) / 6
            if share >= 1 or (share > 0 and share * 16 > BAYER[y % 4][x % 4] + 0.5):
                px[x, y] = P["meadow_dark"]

    for _ in range(70):
        x = rng.randrange(W)
        y = rng.randrange(meadow_y[x] + 2, H)
        tuft = P["grass"] if y < 160 else P["meadow"]
        put(x, y, tuft)
        put(x - 1, y - 1, tuft)
        put(x + 1, y - 1, tuft)
    for _ in range(26):
        x = rng.randrange(W)
        y = rng.randrange(meadow_y[x] + 3, H)
        px[x, y] = P[rng.choice(["flower_a", "flower_b", "flower_c"])]

    # Nearer pines frame the left and right edges of the valley.
    for bx, h in [(6, 46), (16, 38), (27, 30), (-2, 34), (36, 22), (296, 44), (306, 52), (316, 40), (286, 30), (276, 22)]:
        pine(bx, meadow_y[max(0, min(W - 1, bx))] + 4, h, P["pine"], P["pine_dark"])

    img.resize((W * SCALE, H * SCALE), Image.NEAREST).save(out_path, optimize=True)


if __name__ == "__main__":
    main(Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).with_name("stage.png"))
