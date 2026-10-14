"""Generate the README's terminal-style banner, light and dark, next to this script:

    python3 assets/logo/build_logo.py

The banner is figlet's ANSI Shadow font drawn as geometry (full blocks as rects, the
box-drawing shadow as double strokes), so it renders the same without any font installed.
"""
from pathlib import Path

# ink is a (start, end) pair for a left-to-right gradient
THEMES = {
    "light": dict(ink=("#f29718", "#e8566b")),
    "dark":  dict(ink=("#ffb454", "#f28779")),
}

f = lambda v: f"{v:.2f}".rstrip("0").rstrip(".")

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
    out = Path(__file__).parent
    for name, t in THEMES.items():
        with open(out / f"logo-{name}.svg", "w") as fh:
            fh.write(banner_svg(t))
