# README films

The four films in the README, as source. Each film is a
[HyperFrames](https://github.com/heygen-com/hyperframes) composition: plain HTML and CSS driven
by one paused GSAP timeline and rendered frame by frame, so every render is identical.

| Film           | Shows                                                                              |
| :------------- | :--------------------------------------------------------------------------------- |
| `hero.html`    | three workspaces in milliseconds with dependencies inside; test, diff, merge one, drop all |
| `how.html`     | the overlay model: reads, copy-up writes, deletion markers, merge                  |
| `agents.html`  | three agents on one task, `compare`, keep one                                      |
| `servers.html` | a dev server per workspace behind `run` and `switch`                               |

## Render

Requires Node.js 22+ and FFmpeg with `libwebp` and `libx264`.

```sh
./render.sh            # all films
./render.sh hero how   # some films
```

Each film renders at 240fps, and every four frames are averaged into one 60fps frame in
`renders/` (ignored by Git). The 60fps frames become a looping lossless animated WebP at
1920×1080 in `..`, which the README shows, and an MP4 in `renders/` for sharing.

To scrub a film, run `npx hyperframes@0.8.150 preview` here and open it in the Studio.

## How a film is built

- **The stage.** `shared/stage.png` is a pixel-art valley at sunset, drawn at 320×180 by
  `shared/stage.py` (Pillow, seeded) and scaled ×6 with hard edges. Edit the script and rerun
  it rather than editing the PNG. The stage has rounded corners and the frame outside them is
  transparent, so the WebP sits cleanly on any page colour; the MP4 shows the corners as black.
- **One card.** Every film is one `.shell` window at 48, 48, 1824×984, leaving a thin band
  of stage on each side. It carries the only title bar, window controls (`FS.LIGHTS`) and shadow.
- **Sub-cards on a grid.** Inside the card, `.panel` and `.term` sub-cards are flat (no
  shadow) and sit on a grid: 24px inside the card's edges and below its title bar, 20px
  gutters. Nothing overlaps. Text always sits on a card, never on the art.
- **Focus.** The card's window controls turn colour at the first click. Inside it, a neutral
  outline (`.focus`; `servers` uses its own `.ring`) marks the sub-card a beat is about, and
  where a beat singles one out, the others' contents dim to 40%.
- **The hand.** `FS.pointer` adds a mouse pointer and `FS.hand` moves it on the timeline. It does
  what a person would: clicks the terminal before typing, points at what each command is about,
  rests inside the card out of the way, and is back at rest before the loop ends.
- **Colour.** The palette is [Ayu](https://github.com/ayu-theme) Dark, kept nearly monochrome:
  colour only marks added, modified and deleted files (Ayu's VCS colours), the text cursor
  (Ayu's accent) and the focused card's controls.
- **Motion** is opacity and short moves: no bounces, glows or scale pops. `shared/forkspace.js`
  holds the easing vocabulary (`fs.out`, `fs.in`, `fs.move`, `fs.swap`); no curve peaks above
  about 2.9× its average speed, and long travels get long durations.
- **Determinism.** `FS.drive` renders each frame as a function of the timeline's time, and the
  terminal's typist is seeded, so seeking anywhere gives the same frame.
- **The loop.** Every film ends on its first frame. The reset dips only the sub-cards'
  contents, never the card, its shadow or the art: lossless WebP pays for every pixel that
  changes.
- **Content.** Terminal output is copied from real `forkspace` runs; keep it in sync when the
  CLI's output changes. Timings and sizes come from
  [`docs/benchmarks.md`](../../../docs/benchmarks.md); measure again before changing them.

## Quality rules

- **Motion blur.** Without it, anything that travels more than a few pixels per frame strobes,
  and the film looks choppy even at 60fps. Rendering at 240fps and averaging four frames into
  one (`tmix`) is the blur a camera shutter makes.
- **Lossless.** Lossy WebP stores mostly partial frames, and slow fades of low-contrast UI leave
  blocky trails. Every step up to the WebP is lossless too.
- **Full size.** On high-density screens GitHub draws the film about 1,800 pixels wide; a smaller
  film is stretched and soft. Lay out at 1:1 and never scale the scene.
- **Crisp text.** `#scene` gets `will-change: transform` and nothing else does. Without that
  single layer Chrome draws resting text with LCD antialiasing and switches to grayscale whenever
  something fades, so text shimmers and loops stop closing. More layers, or a scaled one, blur
  text. Only the pointer has its own layer.

## Doing this in another repo

1. Copy `render.sh`, `hyperframes.json` and `shared/` (fonts, vendor scripts,
   `forkspace.css`, `forkspace.js`, `stage.py`), and start from the film closest to yours.
2. Write the story first: one idea per film, real command output, real numbers with a source.
3. Lay out one card on the stage and sub-cards on a grid, at 1920×1080 and 1:1.
4. Build the timeline from the terminal's schedule: each `term.at(...).type(...).enter(...)`
   returns its end time, and every other beat hangs off those times.
5. Render, then check every film: the first frame matches the last (composited onto a solid
   colour, since transparent pixels' RGB is arbitrary), and no frame is shorter than 16 ms.
6. Watch it at full size on a high-density screen before calling it done.

Geist and Geist Mono are bundled under the SIL Open Font License (`shared/fonts/OFL.txt`).
