# /// script
# requires-python = ">=3.11"
# dependencies = ["playwright>=1.45", "pillow>=10"]
# ///
"""GIFs for your technical README, generated from the real thing.

    uv run readme_gifs.py terminal examples/terminal.toml
    uv run readme_gifs.py web      examples/web.toml
    uv run readme_gifs.py slides   examples/slides.toml
    uv run readme_gifs.py all      examples/          (every *.toml in a folder)

terminal  runs your real commands, captures their real output and replays it
          in a drawn terminal window (typed command, lines appearing).
web       records a real web page in a headless browser while clicking,
          typing and waiting as scripted; can also save a still PNG.
slides    turns a list of images into a slideshow GIF with crossfades.

Each scene is a small TOML file; paths in it are relative to that file.
The web kind uses the installed Edge or Chrome. Without either, run
`uv run --with playwright playwright install chromium` once.
"""
from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import time
import tomllib
from io import BytesIO
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

# ---------------------------------------------------------------------------
# Shared: GIF writing
# ---------------------------------------------------------------------------


def save_gif(frames: list[tuple[Image.Image, int]], path: Path, colors: int = 128,
             shared_palette: bool = False) -> None:
    """frames: (RGB image, duration ms). Identical neighbours are merged."""
    path.parent.mkdir(parents=True, exist_ok=True)
    pal = (frames[-1][0].quantize(colors=colors, method=Image.Quantize.MEDIANCUT)
           if shared_palette else None)
    images, durations = [], []
    for im, ms in frames:
        q = (im.quantize(palette=pal, dither=Image.Dither.NONE) if pal else
             im.quantize(colors=colors, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE))
        if images and q.tobytes() == images[-1].tobytes() and q.getpalette() == images[-1].getpalette():
            durations[-1] += ms
        else:
            images.append(q)
            durations.append(max(20, int(ms)))
    images[0].save(path, save_all=True, append_images=images[1:], duration=durations,
                   loop=0, optimize=False)
    print(f"{path}  {len(images)} frames  {sum(durations) / 1000:.1f} s  "
          f"{path.stat().st_size / 1024:.0f} KB")


# ---------------------------------------------------------------------------
# terminal
# ---------------------------------------------------------------------------

THEMES = {
    "dark": {"bg": (13, 17, 23), "title_bg": (22, 27, 34), "border": (48, 54, 61),
             "fg": (230, 237, 243), "dim": (125, 133, 144), "blue": (88, 166, 255),
             "green": (63, 185, 80), "cyan": (57, 197, 207), "yellow": (227, 179, 65),
             "magenta": (188, 140, 255), "red": (248, 81, 73)},
    "light": {"bg": (255, 255, 255), "title_bg": (246, 248, 250), "border": (208, 215, 222),
              "fg": (31, 35, 40), "dim": (101, 109, 118), "blue": (9, 105, 218),
              "green": (26, 127, 55), "cyan": (27, 124, 131), "yellow": (154, 103, 0),
              "magenta": (130, 80, 223), "red": (207, 34, 46)},
}
FONT_PATHS = ["C:/Windows/Fonts/CascadiaMono.ttf", "C:/Windows/Fonts/consola.ttf",
              "/System/Library/Fonts/Menlo.ttc", "/System/Library/Fonts/Monaco.ttf",
              "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
              "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
              "/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf"]
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
DEFAULT_HIGHLIGHTS = [
    {"regex": r"\b(ok|OK|passed|PASSED|PASS|success|done)\b", "colors": ["green"]},
    {"regex": r"\b(error|ERROR|failed|FAILED|FAIL)\b", "colors": ["red"]},
    {"regex": r"\b(warning|WARNING|WARN)\b", "colors": ["yellow"]},
]


class Terminal:
    def __init__(self, cfg: dict):
        self.c = THEMES[cfg.get("theme", "dark")]
        self.cols = cfg.get("cols", 100)
        self.px = cfg.get("font_size", 15)
        self.line_h = int(self.px * 1.47)
        self.pad, self.title_h = 20, 36
        font = cfg.get("font") or next((p for p in FONT_PATHS if Path(p).exists()), None)
        self.font = ImageFont.truetype(font, self.px) if font else ImageFont.load_default(self.px)
        self.cw = self.font.getlength("M")
        self.title = cfg.get("title", "")
        self.prompt = [(cfg.get("prompt", "~/project"), self.c["blue"]), (" $ ", self.c["dim"])]
        self.rules = [(re.compile(h["regex"]), [self.c[n] for n in h["colors"]])
                      for h in cfg.get("highlight", []) + DEFAULT_HIGHLIGHTS]
        self.lines: list[list] = []
        self.frames: list[tuple[Image.Image, int]] = []
        self.rows = 0   # fixed once the whole scene is known

    # -- text to coloured segments -----------------------------------------
    def colour(self, line: str) -> list:
        """Colour regex groups (or the whole match if the regex has no groups)."""
        marks = [self.c["fg"]] * len(line)
        for rx, cols in reversed(self.rules):          # earlier rules win
            for m in rx.finditer(line):
                spans = ([m.span(g) for g in range(1, rx.groups + 1)] if rx.groups else [m.span()])
                for (a, b), col in zip(spans, cols * len(spans)):
                    if a >= 0:
                        marks[a:b] = [col] * (b - a)
        segs = []
        for ch, col in zip(line, marks):
            if segs and segs[-1][1] == col:
                segs[-1] = (segs[-1][0] + ch, col)
            else:
                segs.append((ch, col))
        return segs or [("", self.c["fg"])]

    def command(self, cmd: str) -> list:
        segs = []
        for i, word in enumerate(cmd.split(" ")):
            sep = " " if i else ""
            if re.match(r"^[A-Za-z_]+=", word):
                k, v = word.split("=", 1)
                segs += [(sep + k + "=", self.c["cyan"]), (v, self.c["fg"])]
            elif word.startswith("-") and i:
                segs.append((sep + word, self.c["dim"]))
            else:
                segs.append((sep + word, self.c["fg"] if i else self.c["green"]))
        return segs

    def wrap(self, segs: list) -> list:
        rows, row, n = [], [], 0
        for text, col in segs:
            while text:
                take = self.cols - n
                row.append((text[:take], col))
                n += len(text[:take])
                text = text[take:]
                if n == self.cols:
                    rows.append(row)
                    row, n = [], 0
        rows.append(row)
        return rows

    # -- drawing -----------------------------------------------------------
    def render(self, extra=None, cursor=True) -> Image.Image:
        c = self.c
        w = int(self.pad * 2 + self.cols * self.cw)
        h = self.title_h + self.pad * 2 + self.rows * self.line_h
        im = Image.new("RGB", (w, h), c["bg"])
        d = ImageDraw.Draw(im)
        d.rectangle([0, 0, w - 1, self.title_h], fill=c["title_bg"])
        for i, dot in enumerate([(255, 95, 87), (254, 188, 46), (40, 200, 64)]):
            cx = 22 + i * 22
            d.ellipse([cx - 6, self.title_h // 2 - 6, cx + 6, self.title_h // 2 + 6], fill=dot)
        d.text(((w - self.font.getlength(self.title)) / 2, self.title_h / 2), self.title,
               font=self.font, fill=c["dim"], anchor="lm")
        d.rectangle([0, 0, w - 1, h - 1], outline=c["border"])
        rows = [r for segs in self.lines + ([extra] if extra else []) for r in self.wrap(segs)]
        y = self.title_h + self.pad
        for r in rows:
            x = self.pad
            for text, col in r:
                d.text((x, y), text, font=self.font, fill=col)
                x += self.cw * len(text)
            y += self.line_h
        if cursor and rows:
            x = self.pad + self.cw * sum(len(t) for t, _ in rows[-1])
            d.rectangle([x, y - self.line_h + 3, x + self.cw - 1, y - 3], fill=c["fg"])
        return im

    def snap(self, ms, **kw):
        self.frames.append((self.render(**kw), ms))


def run_command(cmd: str, cwd: Path) -> list[str]:
    out = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True,
                         encoding="utf-8", errors="replace",
                         env={**os.environ, "PYTHONIOENCODING": "utf-8", "NO_COLOR": "1"})
    text = ANSI.sub("", out.stdout + out.stderr).replace("\r\n", "\n")
    return [l.split("\r")[-1].rstrip() for l in text.split("\n")]


def make_terminal(cfg: dict, base: Path) -> None:
    term = Terminal(cfg)
    cwd = (base / cfg.get("cwd", ".")).resolve()
    steps = []
    for step in cfg["step"]:
        lines = run_command(step.get("run", step["cmd"]), cwd)
        while lines and not lines[-1]:
            lines.pop()
        for a, b in step.get("replace", []):          # e.g. hide a temp path
            lines = [l.replace(a, b) for l in lines]
        limit = step.get("max_lines")
        if limit and len(lines) > limit:
            lines = lines[:limit] + [f"... ({len(lines) - limit} more lines)"]
        steps.append((step, lines))
    # fixed canvas: every row the finished scene needs, plus the final prompt
    term.rows = 1 + sum(len(term.wrap(term.prompt + term.command(s["cmd"])))
                        + sum(len(term.wrap(term.colour(l))) for l in lines) + 1
                        for s, lines in steps)
    type_ms, chars = cfg.get("type_ms", 40), cfg.get("type_chars", 3)
    for step, lines in steps:
        cmd = step["cmd"]
        for k in range(0, len(cmd) + 1, chars):
            term.snap(type_ms, extra=term.prompt + term.command(cmd[:k]))
        term.snap(step.get("pause_ms", 450), extra=term.prompt + term.command(cmd))
        term.lines.append(term.prompt + term.command(cmd))
        for line in lines:
            term.lines.append(term.colour(line))
            term.snap(step.get("line_ms", cfg.get("line_ms", 120)), cursor=False)
        term.lines.append([])
    for _ in range(3):                                  # idle, blinking cursor
        term.snap(550, extra=term.prompt, cursor=True)
        term.snap(550, extra=term.prompt, cursor=False)
    term.snap(cfg.get("end_ms", 3000), extra=term.prompt, cursor=True)
    save_gif(term.frames, base / cfg["out"], colors=160, shared_palette=True)
    if cfg.get("still"):
        term.frames[-1][0].save(base / cfg["still"])
        print(base / cfg["still"])


# ---------------------------------------------------------------------------
# web
# ---------------------------------------------------------------------------

def launch(p):
    for channel in ("msedge", "chrome", None):
        try:
            return p.chromium.launch(channel=channel) if channel else p.chromium.launch()
        except Exception:
            continue
    sys.exit("no Chromium-based browser found (see the module docstring)")


def make_web(cfg: dict, base: Path) -> None:
    from playwright.sync_api import sync_playwright

    url = cfg["url"]
    if "://" not in url:
        url = (base / url).resolve().as_uri()
    server = None
    if cfg.get("serve"):                                # e.g. your dev server
        server = subprocess.Popen(cfg["serve"], shell=True, cwd=(base / cfg.get("cwd", ".")),
                                  stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
        time.sleep(cfg.get("serve_wait", 2.0))
    frames: list[tuple[Image.Image, float]] = []
    try:
        with sync_playwright() as p:
            browser = launch(p)
            page = browser.new_page(viewport={"width": cfg.get("width", 960),
                                              "height": cfg.get("height", 540)},
                                    color_scheme=cfg.get("color_scheme", "light"))
            page.goto(url)
            page.wait_for_timeout(int(cfg.get("settle", 0.4) * 1000))

            def grab():
                frames.append((Image.open(BytesIO(page.screenshot())).convert("RGB"), time.time()))

            def record(seconds):
                end = time.time() + seconds
                grab()
                while time.time() < end:
                    grab()

            for act in cfg["action"]:
                if "wait" in act:
                    record(act["wait"])
                elif "click" in act:
                    page.click(act["click"])
                    grab()
                elif "type" in act:
                    for ch in act["text"]:
                        page.type(act["type"], ch)
                        grab()
                elif "press" in act:
                    page.keyboard.press(act["press"])
                    grab()
                elif "hover" in act:
                    page.hover(act["hover"])
                    grab()
                elif "scroll" in act:
                    for _ in range(act.get("steps", 12)):
                        page.mouse.wheel(0, act["scroll"] / act.get("steps", 12))
                        grab()
                elif "until" in act:                    # record until a selector shows up
                    end = time.time() + act.get("timeout", 30)
                    while time.time() < end and not page.locator(act["until"]).is_visible():
                        grab()
                    grab()
                elif "still" in act:                    # full-quality PNG of this moment
                    page.screenshot(path=str(base / act["still"]))
                    print(base / act["still"])
                else:
                    sys.exit(f"unknown action: {act}")
            browser.close()
    finally:
        if server:
            server.terminate()
    speed = cfg.get("speed", 1.0)                       # 2.0 = play twice as fast
    timed = [(im, (nxt - t) * 1000 / speed)
             for (im, t), (_, nxt) in zip(frames, frames[1:] + [(None, frames[-1][1] + cfg.get("end", 2.0) * speed)])]
    save_gif(timed, base / cfg["out"], colors=cfg.get("colors", 128))


# ---------------------------------------------------------------------------
# slides
# ---------------------------------------------------------------------------

def make_slides(cfg: dict, base: Path) -> None:
    paths = [p for pattern in cfg["images"] for p in sorted(base.glob(pattern))]
    if not paths:
        sys.exit(f"no images match {cfg['images']}")
    w = cfg.get("width", 960)
    imgs = [Image.open(p).convert("RGB") for p in paths]
    h = max(int(im.height * w / im.width) for im in imgs)
    bg = tuple(cfg.get("background", [13, 17, 23]))
    slides = []
    for im in imgs:                                     # fit to width, centre vertically
        im = im.resize((w, int(im.height * w / im.width)), Image.Resampling.LANCZOS)
        canvas = Image.new("RGB", (w, h), bg)
        canvas.paste(im, (0, (h - im.height) // 2))
        slides.append(canvas)
    hold, fade, steps = cfg.get("hold_ms", 2200), cfg.get("fade_ms", 400), cfg.get("fade_steps", 6)
    frames = []
    for i, slide in enumerate(slides):
        frames.append((slide, hold))
        nxt = slides[(i + 1) % len(slides)]
        frames += [(Image.blend(slide, nxt, k / (steps + 1)), fade // steps) for k in range(1, steps + 1)]
    save_gif(frames, base / cfg["out"], colors=cfg.get("colors", 128))


# ---------------------------------------------------------------------------

KINDS = {"terminal": make_terminal, "web": make_web, "slides": make_slides}


def run_scene(path: Path, kind: str | None = None) -> None:
    cfg = tomllib.loads(path.read_text(encoding="utf-8"))
    kind = kind or cfg.get("kind")
    if kind not in KINDS:
        sys.exit(f"{path}: set kind = \"terminal\" | \"web\" | \"slides\"")
    KINDS[kind](cfg, path.parent)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("kind", choices=[*KINDS, "all"])
    ap.add_argument("scene", type=Path, help="scene .toml (or a folder of them for 'all')")
    args = ap.parse_args()
    if args.kind == "all":
        scenes = sorted(args.scene.glob("*.toml"))
        # slides usually reuse stills made by the other scenes: run them last
        scenes.sort(key=lambda p: tomllib.loads(p.read_text(encoding="utf-8")).get("kind") == "slides")
        for scene in scenes:
            run_scene(scene)
    else:
        run_scene(args.scene, args.kind)


if __name__ == "__main__":
    main()
