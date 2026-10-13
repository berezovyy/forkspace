# README films, v2

A second take on the README films (the first lives in `../video`). The same stories, staged like
product shots: real-looking windows on a pixel-art valley, a few layers of depth, colour only
where it means something, and a hand on the mouse. Like v1, every film is a
[HyperFrames](https://github.com/heygen-com/hyperframes) composition: plain HTML and CSS driven
by one paused GSAP timeline, rendered frame by frame, so every render is identical.

| Film           | Shows                                                    |
| :------------- | :------------------------------------------------------- |
| `hero.html`    | three workspaces in milliseconds with dependencies inside, change, test, merge one, drop all |
| `how.html`     | the overlay model: reads, copy-up writes, whiteouts      |
| `agents.html`  | three agents on one task, `compare`, keep one            |
| `servers.html` | dev servers per workspace behind `run` and `switch`      |

## Render

Requires Node.js 22+ and FFmpeg with `libwebp` and `libx264`.

```sh
./render.sh            # all films
./render.sh hero how   # some films
```

Each film renders at 240fps, and every four frames are averaged into one 60fps frame in
`renders/` (ignored by Git). That is motion blur, the way a camera shutter makes it; without it,
anything that travels strobes and the film looks choppy at 60fps. The 60fps frames then become a
looping 1280×720 lossless animated WebP in `../films-v2/`, and an MP4 in `renders/` for sharing.
Keep the WebP lossless: lossy WebP leaves blocky trails on slow fades. The WebP keeps an alpha
channel: outside the stage's rounded corners the film is transparent, so it sits cleanly on
any page colour (the MP4 shows those corners as black).

To scrub a film interactively, run `npx hyperframes@0.8.150 preview` here and open the
composition in the Studio.

## Conventions

- Terminal output is copied from real `forkspace` runs; keep it in sync when the CLI's output
  changes. Timings and sizes on screen come from
  [`docs/benchmarks.md`](../../docs/benchmarks.md); measure again before changing them.
- The stage is `shared/stage.png`, drawn by `shared/stage.py` (Pillow, seeded): a 320×180
  pixel-art valley scaled ×6 with hard edges. Edit the script and rerun it rather than editing
  the PNG. It sits outside `#scene`, so nothing a film does can fade it.
- Text always sits on a window or card, never on the art. Windows are `.panel` and `.term`;
  their title bars carry `FS.LIGHTS`, coloured only on the window that has focus.
- Depth comes from a few planes: a front window may overlap another's padding, never its
  content. Focus lifts a window (`--shadow-lift`) and dims the others' contents.
- The pointer (`FS.pointer`, moved with `FS.hand`) does what a person would: clicks a window
  before typing in it, rests out of the way, points at what the beat is about, and is back at
  rest before the loop ends.
- `shared/forkspace.css` holds the palette and the window, file row and terminal styles. Inside
  the windows the palette is [Ayu](https://github.com/ayu-theme) Dark (or Light, via the
  `theme` variable), kept nearly monochrome: colour only marks added, modified and deleted
  files (Ayu's VCS colours), the text cursor (Ayu's accent) and the focused window's controls.
- Motion is opacity and short moves only: no bounces, glows or scale pops.
- Keep curves gentle and long travels slow: a steep curve crams a move into a few frames.
- `shared/forkspace.js` holds the easing vocabulary (`fs.out`, `fs.in`, `fs.move`,
  `fs.swap`), the seeded typist, and a terminal whose every frame is a function of time.
- Every film ends on its first frame so the WebP loops without a jump. Reset by dipping window
  contents, not whole windows: lossless WebP pays for every pixel that changes over the art.

Geist and Geist Mono are bundled under the SIL Open Font License (`shared/fonts/OFL.txt`).
