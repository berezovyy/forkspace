// Shared helpers for the forkspace README films: easing vocabulary, a seeded RNG, the file
// row builder, and a seekable terminal whose every frame is a pure function of time.
(function () {
  gsap.registerPlugin(CustomEase);

  // One small motion vocabulary, used everywhere so the films feel like one hand made them.
  // Curves stay gentle: a steep curve crams the travel into a few frames, and at 60fps that
  // reads as stutter. No curve here peaks above ~2.9x its average speed.
  CustomEase.create("fs.out", "0.25, 0.5, 0.25, 1"); // arrivals: brisk start, long settle
  CustomEase.create("fs.in", "0.55, 0, 0.9, 0.45"); // departures: gather, then leave
  CustomEase.create("fs.move", "0.45, 0, 0.25, 1"); // travel: soft start, soft landing
  CustomEase.create("fs.swap", "0.65, 0, 0.35, 1"); // in-place state changes

  function rng(seed) {
    let a = seed >>> 0;
    return function () {
      a = (a + 0x6d2b79f5) >>> 0;
      let t = a;
      t = Math.imul(t ^ (t >>> 15), t | 1);
      t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }

  function applyTheme(root) {
    const vars = (window.__hyperframes && window.__hyperframes.getVariables()) || {};
    root.dataset.theme = vars.theme === "light" ? "light" : "dark";
  }

  /** Adds a driver tween whose onUpdate receives the timeline time; also renders t = 0. */
  function drive(tl, duration, render) {
    const clock = { t: 0 };
    tl.to(clock, {
      t: duration,
      duration,
      ease: "none",
      onUpdate: () => render(clock.t),
    }, 0);
    render(0);
  }

  const clamp01 = (x) => Math.min(1, Math.max(0, x));
  const smooth = (x) => {
    const c = clamp01(x);
    return c * c * (3 - 2 * c);
  };

  /**
   * Fills `container` with file rows. Each row: { id, name, ignored, meta }. Returns a map from
   * row id to its element.
   */
  function tree(container, rows, prefix) {
    const out = {};
    for (const row of rows) {
      const el = document.createElement("div");
      el.className = "row" + (row.ignored ? " ignored" : "");
      if (row.id) el.id = prefix + "-" + row.id;
      el.innerHTML =
        '<span class="name">' + row.name + '<span class="strike"></span></span>' +
        '<span class="meta">' + (row.meta || "") + "</span>";
      container.appendChild(el);
      if (row.id) out[row.id] = el;
    }
    return out;
  }

  // ---------- Terminal ----------

  const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  const span = (cls) => (text) => '<span class="' + cls + '">' + esc(text) + "</span>";
  const h = {
    esc,
    add: span("add"),
    mod: span("mod"),
    del: span("del"),
    muted: span("muted"),
    b: span("b"),
  };

  /**
   * A terminal whose contents at time t are computed from a schedule of keystrokes and
   * outputs. Build the schedule with `term.at(t).type(...).enter(...)`, then call
   * `term.render(t)` from a driver.
   */
  class Terminal {
    constructor(body, opts) {
      this.body = body;
      this.prompt = opts.prompt;
      this.rows = opts.rows;
      this.lineHeight = opts.lineHeight || 46;
      this.seed = opts.seed || 7;
      this.entries = [];
      this.clears = [];
      this.last = "";
    }

    /** Starts a command line at time `t`; returns a chainable typist. */
    at(t) {
      const entry = { kind: "cmd", at: t, keys: [[t, ""]], enter: Infinity, done: Infinity };
      this.entries.push(entry);
      const random = rng(this.seed++ * 7919);
      let now = t;
      let text = "";
      const typist = {
        /** Types `s` at a human cadence: bursts within words, small hesitations at spaces. */
        type(s, speed) {
          const base = 0.043 / (speed || 1);
          for (let i = 0; i < s.length; i++) {
            const ch = s[i];
            const prev = i > 0 ? s[i - 1] : text.slice(-1);
            let gap = base * (0.55 + random() * 0.8);
            if (ch === " ") gap *= 1.5;
            if (prev === " " && (ch === "-" || ch === "." )) gap *= 1.35;
            if (random() < 0.05) gap += 0.09 + random() * 0.08;
            now += gap;
            text += ch;
            entry.keys.push([now, text]);
          }
          return typist;
        },
        /** Recalls a previous command from history (one keypress). */
        recall(s, delay) {
          now += delay === undefined ? 0.12 : delay;
          text = s;
          entry.keys.push([now, text]);
          return typist;
        },
        back(n) {
          for (let i = 0; i < n; i++) {
            now += 0.07 + random() * 0.03;
            text = text.slice(0, -1);
            entry.keys.push([now, text]);
          }
          return typist;
        },
        pause(s) {
          now += s;
          return typist;
        },
        /** Presses enter after a short beat; `lines` print `delay` seconds later. */
        enter(lines, delay) {
          now += 0.28;
          entry.enter = now;
          const printAt = now + (delay === undefined ? 0.06 : delay);
          const output = { kind: "out", at: printAt, lines: lines || [] };
          term.entries.push(output);
          entry.done = printAt;
          now = printAt;
          return typist;
        },
        get end() {
          return now;
        },
      };
      const term = this;
      return typist;
    }

    /** Prints extra lines at time `t` (for long-running commands). */
    print(t, lines) {
      this.entries.push({ kind: "out", at: t, lines });
    }

    /** Clears the screen at time `t`, as Ctrl-L would. */
    clear(t) {
      this.clears.push(t);
    }

    lineAt(t) {
      const lines = [];
      let lastClear = -Infinity;
      for (const c of this.clears) if (c <= t) lastClear = Math.max(lastClear, c);
      let active = null;
      let idleSince = 0;
      const ordered = this.entries.slice().sort((a, b) => a.at - b.at);
      for (const entry of ordered) {
        if (entry.at > t) break;
        if (entry.kind === "cmd") {
          let text = "";
          let lastKey = entry.at;
          for (const [kt, kv] of entry.keys) {
            if (kt <= t) {
              text = kv;
              lastKey = kt;
            }
          }
          const typing = t < entry.enter;
          if (typing) active = { lastKey };
          if (entry.at >= lastClear || typing) {
            lines.push({ html: this.prompt + esc(text), caret: typing, lastKey });
          }
          if (!typing) idleSince = entry.done;
        } else if (entry.at >= lastClear) {
          for (const line of entry.lines) lines.push({ html: line });
        }
      }
      if (!active) {
        idleSince = Math.max(idleSince, lastClear);
        lines.push({ html: this.prompt, caret: true, lastKey: idleSince });
      }
      return lines;
    }

    render(t) {
      const lines = this.lineAt(t);
      const overflow = Math.max(0, lines.length - this.rows);
      const visible = lines.slice(Math.max(0, overflow - 1));
      // Ease each one-line scroll over a few frames instead of jumping.
      const scrolled = this.scrollProgress(t, overflow);
      const html = visible
        .map((line) => {
          let caret = "";
          if (line.caret) {
            const idle = t - line.lastKey;
            const on = idle < 0.5 || (idle - 0.5) % 1.06 < 0.53;
            caret = '<span class="caret" style="opacity:' + (on ? 1 : 0) + '"></span>';
          }
          return '<div class="term-line">' + line.html + caret + "</div>";
        })
        .join("");
      const offset = overflow > 0 ? -(scrolled * this.lineHeight) : 0;
      const key = html + offset.toFixed(2);
      if (key === this.last) return;
      this.last = key;
      this.body.innerHTML = html;
      this.body.style.transform = "translateY(" + offset.toFixed(2) + "px)";
    }

    /** 0..1 progress of the most recent one-line scroll. */
    scrollProgress(t, overflow) {
      if (overflow === 0) return 0;
      // Find when the current line count was first reached.
      let lo = t - 0.25;
      const target = this.lineAt(t).length;
      let reached = t;
      for (let s = t; s >= lo; s -= 1 / 120) {
        if (this.lineAt(s).length < target) break;
        reached = s;
      }
      return smooth((t - reached) / 0.14);
    }
  }

  /** Window controls for a title bar: a grey set, and a coloured set shown while focused. */
  const LIGHTS =
    '<span class="lights"><span class="idle"><i></i><i></i><i></i></span>' +
    '<span class="live"><i></i><i></i><i></i></span></span>';

  /** Adds the mouse pointer to `parent` and returns it. */
  function pointer(parent) {
    const el = document.createElement("div");
    el.className = "pointer";
    el.innerHTML =
      '<svg viewBox="0 0 34 50" width="34" height="50">' +
      '<path d="M4 3 L4 38 L12.5 30 L18.5 44.5 L25 41.8 L19 27.8 L30.5 27.8 Z" ' +
      'fill="#111318" stroke="#ffffff" stroke-width="2.6" stroke-linejoin="round"/></svg>';
    parent.appendChild(el);
    return el;
  }

  /**
   * Choreography for a pointer on a timeline. Its tip sits at (x, y). Moves travel on a slight
   * arc, because x and y ease differently, the way a hand moves a mouse.
   */
  const TIP = [4, 3];
  function hand(tl, el) {
    return {
      place(t, x, y) {
        tl.set(el, { x: x - TIP[0], y: y - TIP[1] }, t);
      },
      move(t, x, y, duration) {
        const d = duration || 0.7;
        tl.to(el, { x: x - TIP[0], duration: d, ease: "fs.move" }, t);
        tl.to(el, { y: y - TIP[1], duration: d, ease: "fs.swap" }, t);
        return t + d;
      },
      click(t) {
        tl.to(el, { scale: 0.85, duration: 0.07, ease: "fs.swap" }, t);
        tl.to(el, { scale: 1, duration: 0.16, ease: "fs.out" }, t + 0.07);
        return t + 0.23;
      },
    };
  }

  window.FS = { rng, applyTheme, drive, tree, Terminal, h, clamp01, smooth, LIGHTS, pointer, hand };
})();
