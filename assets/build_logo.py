"""Generate the forkspace mark and the terminal-style banner (light and dark).

    python3 assets/build_logo.py assets

The banner is figlet's ANSI Shadow font drawn as geometry (full blocks as rects, the
box-drawing shadow as double strokes), so it renders the same without any font installed.
"""
import sys

# ink is a (start, end) pair for a left-to-right gradient
THEMES = {
    "light": dict(ink=("#f29718", "#e8566b"), node="#e9a86a", keep="#f29718", drop="#e8566b"),
    "dark":  dict(ink=("#ffb454", "#f28779"), node="#e6b98a", keep="#ffb454", drop="#f28779"),
}

f = lambda v: f"{v:.2f}".rstrip("0").rstrip(".")

# ── mark: original (left) forks; the upper branch merges back, the lower one is dropped
A, C_UP, C_DN, B = (5, 18), (18, -6), (18, 42), (31, 18)

def bez(p0, p1, p2, t):
    return tuple((1-t)**2*a + 2*(1-t)*t*b + t*t*c for a, b, c in zip(p0, p1, p2))

def split(p0, p1, p2, t):
    # first part of a quadratic bezier, up to t
    q = tuple(a + t*(b - a) for a, b in zip(p0, p1))
    return q, bez(p0, p1, p2, t)

ctrl, end = split(A, C_DN, B, 0.56)
ex, ey = bez(A, C_DN, B, 0.78)
X = 2.4

def mark_svg(t):
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 36 36" width="72" height="72">
  <g fill="none" stroke-width="3" stroke-linecap="round">
    <path d="M{A[0]} {A[1]}Q{C_UP[0]} {C_UP[1]} {B[0]} {B[1]}" stroke="{t['keep']}"/>
    <path d="M{A[0]} {A[1]}Q{f(ctrl[0])} {f(ctrl[1])} {f(end[0])} {f(end[1])}" stroke="{t['drop']}" stroke-dasharray="0 5.2"/>
    <path d="M{f(ex-X)} {f(ey-X)}l{f(2*X)} {f(2*X)}m0 -{f(2*X)}l-{f(2*X)} {f(2*X)}" stroke="{t['drop']}" stroke-width="2.6"/>
    <circle cx="{A[0]}" cy="{A[1]}" r="4" fill="{t['node']}" stroke="none"/>
    <circle cx="{B[0]}" cy="{B[1]}" r="4" fill="{t['node']}" stroke="none"/>
  </g>
</svg>
'''

# ── banner: `figlet -f "ANSI Shadow" FORKSPACE`, like the Neovim start screen
BANNER = """\
███████╗ ██████╗ ██████╗ ██╗  ██╗███████╗██████╗  █████╗  ██████╗███████╗
██╔════╝██╔═══██╗██╔══██╗██║ ██╔╝██╔════╝██╔══██╗██╔══██╗██╔════╝██╔════╝
█████╗  ██║   ██║██████╔╝█████╔╝ ███████╗██████╔╝███████║██║     █████╗
██╔══╝  ██║   ██║██╔══██╗██╔═██╗ ╚════██║██╔═══╝ ██╔══██║██║     ██╔══╝
██║     ╚██████╔╝██║  ██║██║  ██╗███████║██║     ██║  ██║╚██████╗███████╗
╚═╝      ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝╚══════╝╚═╝     ╚═╝  ╚═╝ ╚═════╝╚══════╝""".split("\n")

CW, CH = 10, 20                # terminal cell
D = 1.6                        # half-gap between the two strokes of a double line

def box_char(ch, x, y):
    """Double-line box-drawing glyph as path data; the strokes meet the cell edges so
    neighbouring glyphs join up."""
    l, r, t, b = x, x + CW, y, y + CH
    cx, cy = x + CW / 2, y + CH / 2
    p = lambda *pts: "M" + "L".join(f"{f(px)} {f(py)}" for px, py in pts)
    return {
        "═": p((l, cy-D), (r, cy-D)) + p((l, cy+D), (r, cy+D)),
        "║": p((cx-D, t), (cx-D, b)) + p((cx+D, t), (cx+D, b)),
        "╗": p((l, cy-D), (cx+D, cy-D), (cx+D, b)) + p((l, cy+D), (cx-D, cy+D), (cx-D, b)),
        "╔": p((r, cy-D), (cx-D, cy-D), (cx-D, b)) + p((r, cy+D), (cx+D, cy+D), (cx+D, b)),
        "╝": p((l, cy+D), (cx+D, cy+D), (cx+D, t)) + p((l, cy-D), (cx-D, cy-D), (cx-D, t)),
        "╚": p((r, cy+D), (cx-D, cy+D), (cx-D, t)) + p((r, cy-D), (cx+D, cy-D), (cx+D, t)),
    }[ch]

def banner_svg(t):
    blocks, lines = [], []
    for row, text in enumerate(BANNER):
        y = row * CH
        col = 0
        while col < len(text):
            if text[col] == "█":
                run = col
                while run < len(text) and text[run] == "█":
                    run += 1
                blocks.append(f"M{col*CW} {y}h{(run-col)*CW}v{CH}h-{(run-col)*CW}z")
                col = run
                continue
            if text[col] != " ":
                lines.append(box_char(text[col], col * CW, y))
            col += 1

    cols = max(map(len, BANNER))
    # block cursor one cell after the last letter: cap height, terminal-cell proportions
    cx, ch = (cols + 1) * CW, (len(BANNER) - 1) * CH
    cw = ch // 2
    W, H = cx + cw, len(BANNER) * CH
    a, z = t["ink"]
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}">
  <title>forkspace</title>
  <linearGradient id="ink" gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="{W}" y2="0">
    <stop offset="0" stop-color="{a}"/><stop offset="1" stop-color="{z}"/>
  </linearGradient>
  <path fill="url(#ink)" d="{''.join(blocks)}"/>
  <path fill="none" stroke="url(#ink)" stroke-opacity=".55" stroke-width="1.2" d="{''.join(lines)}"/>
  <rect x="{cx}" y="0" width="{cw}" height="{ch}" fill="{z}">
    <animate attributeName="opacity" values="1;0" keyTimes="0;.5" dur="1.1s" calcMode="discrete" repeatCount="indefinite"/>
  </rect>
</svg>
'''

if __name__ == "__main__":
    out = sys.argv[1]
    for name, t in THEMES.items():
        with open(f"{out}/mark-{name}.svg", "w") as fh:
            fh.write(mark_svg(t))
        with open(f"{out}/logo-{name}.svg", "w") as fh:
            fh.write(banner_svg(t))
