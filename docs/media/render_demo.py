#!/usr/bin/env python3
"""The amm demo film, rendered deterministically with Pillow.

No Claude Code session, daemon or network access is needed. Requires Pillow,
NumPy and FFmpeg. Every frame is a pure function of time, so the film is
reproducible. Re-render it from the repository root with:

    uv run --with pillow --with numpy python docs/media/render_demo.py --output docs/media/demo.mp4
    uv run --with pillow --with numpy python docs/media/render_demo.py --storyboard storyboard.jpg
    uv run --with pillow --with numpy python docs/media/render_demo.py --frame 30.5 --output frame.png

``--output demo.mp4`` also writes ``demo.gif``, the README preview, and
``demo-poster.jpg``, the video poster, beside it.
``--social social.png`` writes the 1280x640 GitHub social preview image.
The soundtrack comes from ``demo_audio.py`` in this directory.
"""

from __future__ import annotations

import argparse
import math
import shutil
import subprocess
import sys
import tempfile
from functools import cache, lru_cache
from multiprocessing import Pool
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFont

W, H, FPS, DURATION = 1920, 1080, 60, 54
BG = (10, 11, 20)
INK = (238, 240, 248)
ORANGE = (255, 146, 74)  # the manager, and amm's mark
PEACH = (255, 200, 158)
SKY = (112, 168, 255)  # the workers
TEAL = (84, 212, 186)
ORCHID = (190, 142, 255)
GOLD = (255, 210, 98)
CORAL = (255, 98, 112)
MUTED = (150, 156, 182)
DIM = (68, 74, 102)
RULE = (34, 38, 62)
CARD = (18, 20, 35)
STARTS = (0, 5, 11, 17, 23, 29, 35, 41, 46, 50)
ENDS = STARTS[1:] + (DURATION,)
CHAPTERS = (
    "AGENT MIDDLE MANAGEMENT",
    "WRITE BRIEFS",
    "WORKERS IN WORKTREES",
    "GUARDRAILS",
    "WATCHDOG",
    "THE DONE GATE",
    "INTEGRATE AND TEST",
    "DASHBOARD",
    "INSTALL",
    "AMM",
)
WIPE = 0.7
TIERS = (
    ("light", "Haiku", TEAL, 40),
    ("standard", "Sonnet", SKY, 80),
    ("heavy", "Opus", ORCHID, 120),
)


# Maths -----------------------------------------------------------------------


def clamp(x):
    return min(1.0, max(0.0, x))


def smooth(x):
    x = clamp(x)
    return x * x * (3 - 2 * x)


def ease(x):
    x = clamp(x)
    return 1 - (1 - x) ** 3


def mix(a, b, p):
    p = clamp(p)
    return tuple(round(x + (y - x) * p) for x, y in zip(a, b, strict=True))


# Type ------------------------------------------------------------------------

FONT_DIRS = (
    Path("/usr/share/fonts/TTF"),
    Path("/usr/share/fonts/truetype/fira"),
    Path("/usr/share/fonts/truetype/dejavu"),
    Path("/usr/share/fonts/dejavu"),
    Path.home() / ".local/share/fonts",
    Path.home() / ".fonts",
)
FACES = {
    "light": ("FiraSans-Light.ttf", "DejaVuSans-ExtraLight.ttf", "DejaVuSans.ttf"),
    "regular": ("FiraSans-Regular.ttf", "DejaVuSans.ttf"),
    "medium": ("FiraSans-Medium.ttf", "DejaVuSans.ttf"),
    "semibold": ("FiraSans-SemiBold.ttf", "DejaVuSans-Bold.ttf"),
    "bold": ("FiraSans-Bold.ttf", "DejaVuSans-Bold.ttf"),
    "mono": ("DejaVuSansMono.ttf",),
    "monobold": ("DejaVuSansMono-Bold.ttf", "DejaVuSansMono.ttf"),
}


@cache
def font(size, weight="regular"):
    for name in FACES[weight]:
        for root in FONT_DIRS:
            if (root / name).exists():
                return ImageFont.truetype(str(root / name), size)
    raise RuntimeError(f"No font found for {weight!r}; install Fira Sans or DejaVu")


@lru_cache(maxsize=4000)
def lettering(value, size, color, weight):
    """Text on a transparent stamp whose top is the font's ascender, so segments line up."""
    f = font(size, weight)
    box = f.getbbox(value)
    ascent, descent = f.getmetrics()
    im = Image.new("RGBA", (max(1, box[2] - box[0] + 4), ascent + descent + 4))
    ImageDraw.Draw(im).text((2 - box[0], 2), value, font=f, fill=color)
    return im


def width_of(value, size, weight="regular"):
    return font(size, weight).getlength(value)


def txt(im, x, y, value, size=32, color=INK, weight="regular", align="left", opacity=1.0):
    if opacity <= 0 or not value:
        return
    stamp = lettering(value, size, color, weight)
    if align == "center":
        x -= stamp.width / 2
    elif align == "right":
        x -= stamp.width
    if opacity < 1:
        stamp = stamp.copy()
        stamp.putalpha(stamp.getchannel("A").point(lambda v: round(v * opacity)))
    im.paste(stamp, (round(x), round(y)), stamp)


def reveal(im, x, y, value, size, local, at=0.0, color=INK, weight="bold", align="left", rise=28):
    p = ease((local - at) / 0.6)
    txt(im, x, y + rise * (1 - p), value, size, color, weight, align, p)


def typed(im, x, y, value, size, local, at=0.0, color=INK, weight="regular", speed=28.0):
    """Text that appears character by character, with a cursor while it types."""
    if local < at:
        return 0
    shown = min(len(value), int((local - at) * speed))
    txt(im, x, y, value[:shown], size, color, weight)
    if shown < len(value) and int(local * 3) % 2 == 0:
        cx = x + width_of(value[:shown], size, weight) + 2
        ImageDraw.Draw(im).rectangle((cx, y + 2, cx + 3, y + size), fill=color)
    return shown


def wrap(value, size, width, weight="regular"):
    lines, current = [], ""
    for word in value.split():
        trial = (current + " " + word).strip()
        if current and width_of(trial, size, weight) > width:
            lines.append(current)
            current = word
        else:
            current = trial
    return lines + [current] if current else lines


# Shapes ----------------------------------------------------------------------


def draw(im):
    return ImageDraw.Draw(im)


def rect(im, box, fill=CARD, outline=None, width=1, radius=14):
    draw(im).rounded_rectangle(tuple(round(v) for v in box), radius=radius, fill=fill, outline=outline, width=width)


def line(im, xy, fill=RULE, width=2):
    draw(im).line(tuple(round(v) for v in xy), fill=fill, width=width)


def dot(im, cx, cy, r, fill, outline=None, width=0):
    draw(im).ellipse((cx - r, cy - r, cx + r, cy + r), fill=fill, outline=outline, width=width)


def arrow(im, x0, y0, x1, y1, color=ORANGE, width=4):
    line(im, (x0, y0, x1, y1), color, width)
    angle = math.atan2(y1 - y0, x1 - x0)
    head = [(x1 + 4 * math.cos(angle), y1 + 4 * math.sin(angle))]
    head += [(x1 - 16 * math.cos(angle + a), y1 - 16 * math.sin(angle + a)) for a in (-0.5, 0.5)]
    draw(im).polygon(head, fill=color)


def check(im, cx, cy, color=TEAL, size=12, width=4):
    draw(im).line(
        [(cx - size, cy), (cx - size * 0.3, cy + size * 0.7), (cx + size, cy - size * 0.8)],
        fill=color,
        width=width,
        joint="curve",
    )


def cross(im, cx, cy, color=CORAL, size=10, width=4):
    line(im, (cx - size, cy - size, cx + size, cy + size), color, width)
    line(im, (cx - size, cy + size, cx + size, cy - size), color, width)


def badge(im, cx, cy, ok, p, r=18):
    """A filled circle with a check (passed) or a cross (refused)."""
    if p <= 0:
        return
    dot(im, cx, cy, r, mix(CARD, TEAL if ok else CORAL, p))
    if ok:
        check(im, cx, cy, mix(CARD, BG, p), r * 0.45, 4)
    else:
        cross(im, cx, cy, mix(CARD, BG, p), r * 0.36, 4)


def chip(im, x, y, value, size=24, color=ORANGE, weight="mono", opacity=1.0, pad=16):
    """A rounded label. Returns its width so callers can flow chips."""
    w = width_of(value, size, weight) + pad * 2
    if opacity > 0:
        rect(im, (x, y, x + w, y + size + 18), mix(BG, CARD, opacity), mix(BG, mix(DIM, color, 0.5), opacity), 2, 10)
        txt(im, x + pad, y + 8, value, size, color, weight, opacity=opacity)
    return w


def panel(im, box, p, label=None, right=None, radius=20):
    """A card that rises into place; returns its top edge after the rise."""
    x0, y0, x1, y1 = box
    dy = 24 * (1 - p)
    rect(im, (x0, y0 + dy, x1, y1 + dy), mix(BG, CARD, p), mix(BG, RULE, p), 2, radius)
    if label:
        txt(im, x0 + 32, y0 + dy + 28, label, 20, MUTED, "mono", opacity=p)
    if right:
        txt(im, x1 - 32, y0 + dy + 28, right, 20, MUTED, "mono", "right", p)
    return y0 + dy


@lru_cache(maxsize=1)
def backdrop():
    im = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(im)
    for y in range(H):
        d.line((0, y, W, y), fill=mix(BG, (14, 15, 30), y / H))
    for y in range(36, H, 48):
        for x in range(36, W, 48):
            d.point((x, y), fill=(26, 28, 46))
    return im


@lru_cache(maxsize=1)
def glow_sprite():
    size = 120
    im = Image.new("RGB", (size, size))
    px = im.load()
    for y in range(size):
        for x in range(size):
            d = math.hypot(x - size / 2 + 0.5, y - size / 2 + 0.5) / (size / 2)
            a = clamp(1 - d) ** 2.4 * 0.24
            c = mix(SKY, ORANGE, clamp(1.1 - d))
            px[x, y] = tuple(round(v * a) for v in c)
    return im.resize((1500, 1500), Image.Resampling.BICUBIC)


# Where the light sits in each chapter; it drifts between them.
GLOW = (
    (1560, 340),
    (1500, 900),
    (960, 980),
    (360, 860),
    (1580, 720),
    (400, 280),
    (1500, 300),
    (960, 960),
    (300, 300),
    (960, 560),
)


def glow_at(t):
    index = next((i for i, end in enumerate(ENDS) if t < end), len(ENDS) - 1)
    local = t - STARTS[index]
    a = GLOW[max(0, index - 1)] if local < 1.6 and index else GLOW[index]
    b = GLOW[index]
    p = smooth(local / 1.6) if index else 1
    return a[0] + (b[0] - a[0]) * p, a[1] + (b[1] - a[1]) * p


def base(t):
    layer = Image.new("RGB", (W, H))
    x, y = glow_at(t)
    layer.paste(glow_sprite(), (round(x - 750), round(y - 750)))
    return ImageChops.add(backdrop(), layer)


def title(im, value, local, subtitle=None):
    reveal(im, 100, 150, value, 84, local, at=0.1)
    if subtitle:
        reveal(im, 104, 262, subtitle, 32, local, at=0.3, color=MUTED, weight="regular")


def foot(im, *lines, local=99.0, at=0.0):
    p = ease((local - at) / 0.5)
    for i, value in enumerate(lines):
        txt(im, 104, 948 + i * 34, value, 24, MUTED, "regular", opacity=p)


def mark(im, x, y, size, opacity=1.0):
    """The badge: an orange tile holding a tiny org chart, one manager over three workers."""
    if opacity <= 0:
        return
    rect(im, (x, y, x + size, y + size), mix(BG, ORANGE, opacity), radius=round(size * 0.26))
    ink = mix(ORANGE, BG, opacity)
    s = size / 100
    w = max(2, round(6 * s))
    line(im, (x + 50 * s, y + 34 * s, x + 50 * s, y + 52 * s), ink, w)
    line(im, (x + 24 * s, y + 52 * s, x + 76 * s, y + 52 * s), ink, w)
    for cx in (24, 50, 76):
        line(im, (x + cx * s, y + 52 * s, x + cx * s, y + 64 * s), ink, w)
        dot(im, x + cx * s, y + 72 * s, 9 * s, ink)
    dot(im, x + 50 * s, y + 27 * s, 12 * s, ink)


def chrome(im, index, t):
    alpha = 1 - smooth((t - STARTS[-1] - 0.2) / 0.5)
    if alpha > 0:
        mark(im, 104, 58, 42, alpha)
        txt(im, 160, 62, "amm", 30, INK, "bold", opacity=alpha)
        txt(im, 1816, 70, f"{index + 1:02d} / {CHAPTERS[index]}", 22, MUTED, "mono", "right", alpha)
    line(im, (0, 1, W * t / DURATION, 1), ORANGE, 3)


# 1 · Hook --------------------------------------------------------------------

PROMISES = (
    (CORAL, "Blocks edits outside a brief's scope, pushes and agent spawns"),
    (GOLD, "Watches every tool call; corrects, or escalates a tier"),
    (TEAL, "Refuses “done” until the checks pass and a report is filed"),
    (SKY, "Keeps the manager's context small: status deltas, not transcripts"),
)


def org_chart(im, local, cx=1560, top=400):
    """The manager and its three workers, one per tier."""
    p = ease((local - 0.9) / 0.6)
    if p <= 0:
        return
    my = top + 60
    for i, (tier, model, color, _) in enumerate(TIERS):
        q = ease((local - 1.4 - i * 0.2) / 0.5)
        if q <= 0:
            continue
        wx, wy = cx + (i - 1) * 190, top + 330
        line(im, (cx, my + 64, cx, my + 120), mix(BG, DIM, q), 3)
        line(im, (cx, my + 120, wx, my + 120), mix(BG, DIM, q), 3)
        line(im, (wx, my + 120, wx, wy - 54), mix(BG, DIM, q), 3)
        rise = 18 * (1 - q)
        dot(im, wx, wy + rise, 50, mix(BG, CARD, q), mix(BG, color, q), 4)
        txt(im, wx, wy - 15 + rise, model, 24, color, "semibold", "center", q)
        txt(im, wx, wy + 66 + rise, tier, 22, MUTED, "mono", "center", q)
        txt(im, wx, wy + 98 + rise, "worktree", 18, DIM, "mono", "center", q)
    dot(im, cx, my + 20 * (1 - p), 64, mix(BG, CARD, p), mix(BG, ORANGE, p), 5)
    txt(im, cx, my - 18 + 20 * (1 - p), "manager", 26, ORANGE, "semibold", "center", p)


def hook(t):
    t += 0.9  # The first frame already carries the message.
    im = base(t - 0.9)
    reveal(im, 100, 186, "Agent middle", 120, t, at=0)
    reveal(im, 100, 322, "management.", 120, t, at=0.2, color=ORANGE)
    reveal(im, 106, 498, "Your Claude Code session becomes a manager that writes briefs.", 36, t, at=0.55)
    reveal(
        im,
        106,
        552,
        "Cheaper worker agents do the work in isolated git worktrees.",
        30,
        t,
        at=0.7,
        color=MUTED,
        weight="regular",
    )
    p = ease((t - 1.3) / 0.6)
    if p:
        y = panel(im, (104, 640, 1180, 900), p, "AMM SUPERVISES THEM")
        for i, (color, value) in enumerate(PROMISES):
            q = ease((t - 1.6 - i * 0.18) / 0.5)
            yy = y + 68 + i * 47
            dot(im, 146, yy + 15, 7, mix(CARD, color, q))
            txt(im, 172, yy, value, 27, INK, "regular", opacity=q)
    org_chart(im, t)
    foot(im, "One Rust daemon per machine  ·  ~/.amm  ·  127.0.0.1:7878  ·  all state in SQLite", local=t, at=2.4)
    return im


# 2 · Write briefs ------------------------------------------------------------

SESSION = (
    (0.6, "user", "> delegate this plan with amm"),
    (1.4, "call", "amm_run_start   test_cmd=\"cargo test\""),
    (1.9, "call", "amm_brief_add   “Rate-limit the API client”"),
    (2.2, "call", "amm_brief_add   “Retry on HTTP 429”"),
    (2.5, "call", "amm_brief_add   “Document the client”"),
    (3.1, "call", "amm_dispatch    → 3 workers"),
)


def briefs(t):
    im = base(t + STARTS[1])
    title(
        im,
        "Write briefs, not code.",
        t,
        "The manager splits the goal into self-contained briefs: a goal, a scope, checks and a tier.",
    )
    p = ease((t - 0.4) / 0.6)
    if p:
        y = panel(im, (104, 350, 900, 900), p, "MANAGER SESSION")
        for i, (at, kind, value) in enumerate(SESSION):
            q = ease((t - at) / 0.35)
            yy = y + 80 + i * 52
            if kind == "user":
                typed(im, 140, yy, value, 24, t, at, INK, "mono", speed=36)
                continue
            dot(im, 146, yy + 14, 7, mix(CARD, ORANGE, q))
            txt(im, 170, yy, value, 22, PEACH, "mono", opacity=q)
        q = ease((t - 3.7) / 0.5)
        if q:
            yy = y + 420
            line(im, (140, yy - 20, 864, yy - 20), mix(CARD, RULE, q), 2)
            txt(im, 140, yy, "EACH WORKER'S WHOLE PROMPT", 18, MUTED, "mono", opacity=q)
            txt(im, 140, yy + 36, "amm worker. Your brief token is \"4f2a9c1\".", 24, SKY, "mono", opacity=q)
    p = ease((t - 1.2) / 0.6)
    if p:
        y = panel(im, (960, 350, 1816, 900), p, "BRIEF  4f2a9c1")
        chip(im, 1636, y + 20, "standard", 20, SKY, opacity=p, pad=14)
        rows = (
            ("Goal", "Add a token-bucket rate limiter to the API client, with tests. Keep the public API unchanged."),
            ("Scope", None),
            ("Checks", None),
            ("Tier", None),
        )
        yy = y + 90
        for i, (label, value) in enumerate(rows):
            q = ease((t - 1.8 - i * 0.35) / 0.45)
            line(im, (996, yy - 14, 1780, yy - 14), mix(CARD, RULE, q), 2)
            txt(im, 996, yy, label, 24, MUTED, "regular", opacity=q)
            if label == "Goal":
                for n, piece in enumerate(wrap(value, 26, 600, "medium")):
                    txt(im, 1150, yy, piece, 26, INK, "medium", opacity=q)
                    yy += 38
                yy += 34
            elif label == "Scope":
                x = 1150
                for glob in ("src/client/**", "tests/client_*.rs"):
                    x += chip(im, x, yy - 6, glob, 20, PEACH, opacity=q, pad=12) + 12
                yy += 76
            elif label == "Checks":
                chip(im, 1150, yy - 6, "cargo test client", 20, TEAL, opacity=q, pad=12)
                yy += 76
            else:
                txt(im, 1150, yy, "standard · Sonnet", 26, SKY, "medium", opacity=q)
                txt(im, 1150, yy + 40, "light (Haiku) for mechanical edits, heavy (Opus) for tricky logic", 20, MUTED, opacity=q)
    foot(
        im,
        "One brief is one unit a worker can finish alone. Briefs with overlapping scopes run one after another.",
        "The worker has none of the manager's context, so the goal says everything it needs.",
        local=t,
        at=4.2,
    )
    return im


# 3 · Workers in worktrees ----------------------------------------------------

WORKERS = (
    ("9b1e07d", "Retry on HTTP 429", "agent-a3f1", ("Read", "Edit", "Bash", "Edit", "Bash")),
    ("4f2a9c1", "Rate-limit the API client", "agent-c82d", ("Read", "Read", "Edit", "Edit", "Bash")),
    ("c03d5aa", "Fix the flaky reconnect", "agent-e05b", ("Read", "Grep", "Read", "Edit", "Bash")),
)
TOOL_COLOR = {"Read": MUTED, "Grep": MUTED, "Edit": PEACH, "Bash": TEAL}


def workers(t):
    im = base(t + STARTS[2])
    title(
        im,
        "Cheaper workers, own worktrees.",
        t,
        "Each brief runs in an isolated git worktree, on the cheapest tier that can do it.",
    )
    for i, ((tier, model, color, turns), (bid, name, wt, calls)) in enumerate(zip(TIERS, WORKERS, strict=True)):
        x = 104 + i * 580
        p = ease((t - 0.4 - i * 0.3) / 0.6)
        if p <= 0:
            continue
        y = panel(im, (x, 350, x + 552, 900), p)
        dot(im, x + 52, y + 54, 20, mix(CARD, color, p))
        txt(im, x + 88, y + 30, model, 40, INK, "bold", opacity=p)
        txt(im, x + 520, y + 42, tier, 22, color, "mono", "right", p)
        txt(im, x + 32, y + 104, bid, 20, MUTED, "mono", opacity=p)
        txt(im, x + 32 + width_of(bid, 20, "mono") + 14, y + 100, name, 26, INK, "semibold", opacity=p)
        txt(im, x + 32, y + 150, f".claude/worktrees/{wt}", 20, color, "mono", opacity=p)
        txt(im, x + 32, y + 180, f"branch  worktree-{wt}", 20, MUTED, "mono", opacity=p)
        line(im, (x + 32, y + 228, x + 520, y + 228), mix(CARD, RULE, p), 2)
        # Tool calls stream in, newest at the bottom.
        start = 1.3 + i * 0.25
        for n, call in enumerate(calls):
            q = ease((t - start - n * 0.55) / 0.3)
            if q <= 0:
                continue
            yy = y + 252 + n * 40
            dot(im, x + 44, yy + 12, 6, mix(CARD, TOOL_COLOR[call], q))
            txt(im, x + 62, yy, call, 22, TOOL_COLOR[call], "mono", opacity=q)
            detail = {"Read": "src/client/mod.rs", "Grep": "reconnect", "Edit": "src/client/*.rs", "Bash": "cargo test client"}
            txt(im, x + 160, yy, detail[call], 22, MUTED, "mono", opacity=q)
        used = sum(1 for n in range(len(calls)) if t >= start + n * 0.55) * (2 + i)
        frac = used / turns
        bar_y = y + 500
        txt(im, x + 32, bar_y - 34, "TURNS", 18, MUTED, "mono", opacity=p)
        txt(im, x + 520, bar_y - 34, f"{used} / {turns}", 18, MUTED, "mono", "right", p)
        rect(im, (x + 32, bar_y, x + 520, bar_y + 10), mix(CARD, RULE, p), radius=5)
        if frac:
            rect(im, (x + 32, bar_y, x + 32 + 488 * frac, bar_y + 10), mix(CARD, color, p), radius=5)
    foot(
        im,
        "Up to four workers at once by default. Each one sees only its brief, never the manager's conversation.",
        local=t,
        at=3.6,
    )
    return im


# 4 · Guardrails --------------------------------------------------------------

GUARDS = (
    ("Edit", "src/client/limiter.rs", True, "in scope: the edit goes through"),
    ("Edit", "src/server/routes.rs", False, "outside your scope (src/client/**, tests/client_*.rs)"),
    ("Bash", "git push origin HEAD", False, "workers may not push. Your manager merges your branch."),
    ("Agent", "spawn a helper", False, "workers may not spawn agents. Do the work yourself."),
)


def guardrails(t):
    im = base(t + STARTS[3])
    title(
        im,
        "Guardrails at every tool call.",
        t,
        "amm's PreToolUse hook checks each call against the brief before it runs.",
    )
    gate_x = 820
    p = ease((t - 0.3) / 0.5)
    if p:
        rect(im, (gate_x - 60, 340, gate_x + 60, 880), mix(BG, CARD, p), mix(BG, ORANGE, p * 0.7), 2, 22)
        txt(im, gate_x, 358, "amm", 24, ORANGE, "bold", "center", p)
        txt(im, gate_x, 390, "hook", 18, MUTED, "mono", "center", p)
        txt(im, 106, 372, "WORKER 4f2a9c1 CALLS", 20, MUTED, "mono", opacity=p)
        txt(im, gate_x + 170, 372, "AMM ANSWERS", 20, MUTED, "mono", opacity=p)
    for i, (tool, target, ok, message) in enumerate(GUARDS):
        at = 0.8 + i * 1.05
        q = ease((t - at) / 0.4)
        if q <= 0:
            continue
        y = 440 + i * 110
        # The call slides toward the gate.
        slide = 30 * (1 - q)
        w = chip(im, 104 - slide, y, tool, 24, SKY, "monobold", q, 14)
        txt(im, 104 - slide + w + 16, y + 8, target, 24, INK, "mono", opacity=q)
        r = ease((t - at - 0.35) / 0.4)
        if r:
            arrow(im, 660, y + 21, 660 + (gate_x - 80 - 660) * r, y + 21, mix(BG, DIM, r), 3)
        s = ease((t - at - 0.6) / 0.4)
        if s <= 0:
            continue
        color = TEAL if ok else CORAL
        arrow(im, gate_x + 70, y + 21, gate_x + 70 + 70 * s, y + 21, mix(BG, color, s), 3)
        badge(im, gate_x + 190, y + 21, ok, s, 20)
        txt(im, gate_x + 228, y - 4, "allowed" if ok else "denied", 26, color, "semibold", opacity=s)
        txt(im, gate_x + 228, y + 32, message, 22, MUTED if ok else INK, "regular", opacity=s)
    foot(
        im,
        "A denial comes back to the worker as an instruction, so it changes course instead of stalling.",
        "Bash is not confined to the worktree; the done gate checks the branch diff instead.",
        local=t,
        at=4.8,
    )
    return im


# 5 · Watchdog ----------------------------------------------------------------

WATCH = (
    (0.5, "ok", "Read", "src/client/mod.rs"),
    (0.9, "ok", "Edit", "src/client/limiter.rs"),
    (1.4, "fail", "Bash", "cargo test client    exit 101"),
    (1.8, "fail", "Bash", "cargo test client    exit 101"),
    (2.2, "fail", "Bash", "cargo test client    exit 101"),
    (2.7, "warn", None, "amm watchdog: the same command has failed 3 times. Stop retrying it. Change approach, or report “blocked”."),
    (3.7, "fail", "Bash", "cargo test client    exit 101"),
    (4.2, "esc", None, "escalated: 4f2a9c1  light → standard  (repeat-failure)"),
)
RULES = (
    ("repeat failure", "the same command fails 3 times", GOLD),
    ("no progress", "8 calls without an edit or a passing check (light)", GOLD),
    ("budget", "80% of the tier's turns used: finish up", GOLD),
)


def watchdog(t):
    im = base(t + STARTS[4])
    title(
        im,
        "Watched, corrected, escalated.",
        t,
        "Every tool result goes to amm's watchdog, which knows when a worker is stuck.",
    )
    p = ease((t - 0.3) / 0.5)
    if p:
        y = panel(im, (104, 350, 1100, 900), p, "WORKER 4f2a9c1  ·  light", "PostToolUse")
        yy = y + 78
        for at, kind, tool, value in WATCH:
            q = ease((t - at) / 0.3)
            if q <= 0:
                break
            if kind in ("ok", "fail"):
                badge(im, 152, yy + 14, kind == "ok", q, 12)
                txt(im, 180, yy, tool, 22, SKY, "monobold", opacity=q)
                txt(im, 260, yy, value, 22, INK if kind == "ok" else MUTED, "mono", opacity=q)
                yy += 44
            else:
                yy += 10
                color = GOLD if kind == "warn" else ORCHID
                pieces = wrap(value, 24, 860, "medium")
                h = len(pieces) * 34 + 22
                rect(im, (136, yy - 8, 1068, yy - 8 + h), mix(CARD, mix(CARD, color, 0.14), q), radius=10)
                line(im, (136, yy - 6, 136, yy - 10 + h), mix(CARD, color, q), 4)
                for n, piece in enumerate(pieces):
                    txt(im, 160, yy + 2 + n * 34, piece, 24, color, "medium", opacity=q)
                yy += h + 16
    for i, (name, detail, color) in enumerate(RULES):
        q = ease((t - 0.7 - i * 0.2) / 0.5)
        if q <= 0:
            continue
        y = 350 + i * 116 + 20 * (1 - q)
        lit = name == "repeat failure" and t >= 2.7
        rect(im, (1160, y, 1816, y + 100), mix(BG, CARD, q), mix(BG, GOLD if lit else RULE, q), 2, 16)
        txt(im, 1192, y + 18, name, 28, GOLD if lit else INK, "semibold", opacity=q)
        txt(im, 1192, y + 58, detail, 21, MUTED, opacity=q)
    # The escalation ladder.
    q = ease((t - 1.2) / 0.5)
    if q:
        txt(im, 1160, 714, "FIRES AGAIN?  ONE TIER UP", 18, MUTED, "mono", opacity=q)
        climb = smooth((t - 4.2) / 0.6)
        for i, (tier, model, color, _) in enumerate(TIERS):
            x = 1160 + i * 233
            active = (1 - climb) if i == 0 else (climb if i == 1 else 0)
            rect(
                im,
                (x, 754, x + 190, 860),
                mix(BG, mix(CARD, color, 0.18 * active), q),
                mix(BG, mix(RULE, color, active), q),
                2,
                16,
            )
            txt(im, x + 95, 772, model, 28, mix(MUTED, color, active), "semibold", "center", q)
            txt(im, x + 95, 816, tier, 20, MUTED, "mono", "center", q)
            if i < 2:
                arrow(im, x + 198, 807, x + 222, 807, mix(BG, DIM, q), 3)
    foot(
        im,
        "A correction first. If the same rule fires again, the brief goes back out one tier up.",
        local=t,
        at=4.8,
    )
    return im


# 6 · The done gate -----------------------------------------------------------

ATTEMPTS = (
    (
        "FIRST STOP",
        (
            (False, "Report filed", "you have not called amm_report"),
            (False, "Checks pass", "check `cargo test client` exited 101"),
            (True, "Changes in scope", "2 files, all inside src/client/**"),
        ),
        (CORAL, "Stop blocked. Fix these, then finish again."),
    ),
    (
        "SECOND STOP",
        (
            (True, "Report filed", "amm_report  status=done  2 files"),
            (True, "Checks pass", "cargo test client  ·  41 passed"),
            (True, "Changes in scope", "the branch diff, not just the edits"),
        ),
        (TEAL, "Done. Commit pinned and queued to merge."),
    ),
)


def gate(t):
    im = base(t + STARTS[5])
    title(
        im,
        "No “done” until it is done.",
        t,
        "When a worker stops, amm runs the brief's checks before it may finish.",
    )
    for i, (label, items, (color, verdict)) in enumerate(ATTEMPTS):
        at = 0.4 + i * 2.4
        p = ease((t - at) / 0.6)
        if p <= 0:
            continue
        x = 104 + i * 876
        y = panel(im, (x, 350, x + 836, 880), p, label, "SubagentStop")
        for n, (ok, name, detail) in enumerate(items):
            q = ease((t - at - 0.5 - n * 0.35) / 0.4)
            yy = y + 90 + n * 104
            badge(im, x + 60, yy + 26, ok, q, 20)
            txt(im, x + 100, yy, name, 30, INK, "semibold", opacity=q)
            txt(im, x + 100, yy + 42, detail, 22, MUTED if ok else mix(MUTED, CORAL, 0.6), "mono", opacity=q)
        q = ease((t - at - 1.7) / 0.45)
        if q:
            yy = y + 420
            rect(im, (x + 32, yy, x + 804, yy + 74), mix(CARD, mix(CARD, color, 0.16), q), mix(CARD, color, q), 2, 14)
            txt(im, x + 418, yy + 20, verdict, 28, color, "semibold", "center", q)
    foot(
        im,
        "After three failed gates the worker stops and the brief escalates to the next tier.",
        local=t,
        at=4.9,
    )
    return im


# 7 · Integrate and test ------------------------------------------------------

MERGES = (
    ("4f2a9c1", SKY, 470, 1.0),
    ("9b1e07d", TEAL, 690, 2.0),
    ("c03d5aa", ORCHID, 910, 3.0),
)
STATUS = (
    (0.9, INK, "done: 4f2a9c1 Rate-limit the API client"),
    (1.5, TEAL, "merged: 4f2a9c1 → tests pass"),
    (2.5, TEAL, "merged: 9b1e07d → tests pass"),
    (3.5, TEAL, "merged: c03d5aa → tests pass"),
    (4.1, ORANGE, "→ run done: amm/r7ad8a2"),
    (4.1, ORANGE, "   (3 merges, tests pass)"),
)


def integrate(t):
    im = base(t + STARTS[6])
    title(
        im,
        "Merged and tested. Never pushed.",
        t,
        "Passed briefs merge into amm/<run>, and the full test command runs after every merge.",
    )
    p = ease((t - 0.3) / 0.5)
    if p:
        panel(im, (104, 350, 1140, 900), p, "INTEGRATION WORKTREE")
    main_y, run_y, brief_y = 800, 620, 470
    q = ease((t - 0.5) / 0.6)
    if q:
        line(im, (160, main_y, 160 + 920 * q, main_y), mix(BG, DIM, q), 4)
        for x in (200, 420, 1040):
            if x < 160 + 920 * q:
                dot(im, x, main_y, 10, mix(BG, DIM, q))
        txt(im, 168, main_y + 24, "master  (untouched)", 20, MUTED, "mono", opacity=q)
    r = ease((t - 0.7) / 0.6)
    if r:
        line(im, (200, main_y, 240, run_y), mix(BG, ORANGE, r), 4)
        line(im, (240, run_y, 240 + 840 * r, run_y), mix(BG, ORANGE, r), 4)
        txt(im, 252, run_y + 22, "amm/r7ad8a2", 22, ORANGE, "monobold", opacity=r)
    for bid, color, mx, at in MERGES:
        s = ease((t - at + 0.6) / 0.5)
        if s <= 0:
            continue
        fx = mx - 180
        pts = ((fx, run_y), (fx + 40, brief_y), (mx - 40, brief_y), (mx, run_y))
        # Draw the branch progressively along its path.
        for (ax, ay), (bx, by) in zip(pts, pts[1:]):
            line(im, (ax, ay, bx, by), mix(BG, color, s), 4)
        dot(im, fx + 70, brief_y, 9, mix(BG, color, s))
        dot(im, mx - 70, brief_y, 9, mix(BG, color, s))
        txt(im, (fx + mx) / 2, brief_y - 52, bid, 20, color, "mono", "center", s)
        m = ease((t - at) / 0.4)
        if m:
            dot(im, mx, run_y, 15, mix(BG, ORANGE, m), mix(BG, INK, m), 3)
            badge(im, mx, run_y + 54, True, ease((t - at - 0.4) / 0.4), 14)
            txt(im, mx, run_y + 80, "tests", 18, MUTED, "mono", "center", ease((t - at - 0.4) / 0.4))
    p = ease((t - 0.6) / 0.5)
    if p:
        y = panel(im, (1180, 350, 1816, 900), p, "amm_status  ·  deltas only")
        for i, (at, color, value) in enumerate(STATUS):
            q = ease((t - at) / 0.35)
            txt(im, 1212, y + 84 + i * 52, value, 21, color, "mono", opacity=q)
        q = ease((t - 4.6) / 0.5)
        txt(im, 1212, y + 420, "Six short lines in the manager's", 22, MUTED, opacity=q)
        txt(im, 1212, y + 452, "context, not three transcripts.", 22, MUTED, opacity=q)
    foot(
        im,
        "amm never pushes and never touches your base branch. Merging amm/<run> is your call.",
        local=t,
        at=4.8,
    )
    return im


# 8 · Dashboard ---------------------------------------------------------------

COLUMNS = ("Queued", "Running", "Done", "Merged", "Needs attention")
# (id, title, tier color, [(time, column)]) — each card glides to its next column.
CARDS = (
    ("4f2a9c1", "Rate-limit the client", SKY, ((0, 2), (1.6, 3))),
    ("9b1e07d", "Retry on HTTP 429", TEAL, ((0, 1), (1.2, 2), (2.6, 3))),
    ("c03d5aa", "Fix flaky reconnect", ORCHID, ((0, 1), (2.2, 2))),
    ("7d40b2e", "Document the client", TEAL, ((0, 0), (1.9, 1))),
    ("e19a6f3", "Bump the MSRV", SKY, ((0, 4),)),
)


def board_at(time):
    """Each card's (column, row) once every move up to ``time`` has landed."""
    places, filled = {}, [0] * len(COLUMNS)
    for bid, _, _, steps in CARDS:
        col = [c for at, c in steps if at <= time][-1]
        places[bid] = (col, filled[col])
        filled[col] += 1
    return places


MOVES = sorted({at for *_, steps in CARDS for at, _ in steps if at > 0})


def card_place(local, bid):
    """Every move glides, and the cards it displaces glide with it."""
    col, row = board_at(0)[bid]
    for at in MOVES:
        p = smooth((local - at) / 0.6)
        if p <= 0:
            continue
        (c0, r0), (c1, r1) = board_at(at - 1e-6)[bid], board_at(at)[bid]
        col += (c1 - c0) * p
        row += (r1 - r0) * p
    return col, row


def dashboard(t):
    im = base(t + STARTS[7])
    title(
        im,
        "Every run, live.",
        t,
        "/amm:dashboard opens the daemon's page: runs, briefs as a board, events, tokens and cost.",
    )
    p = ease((t - 0.3) / 0.6)
    if p <= 0:
        return im
    y = 350 + 24 * (1 - p)
    rect(im, (104, y, 1816, y + 550), mix(BG, CARD, p), mix(BG, RULE, p), 2, 20)
    for i, c in enumerate((CORAL, GOLD, TEAL)):
        dot(im, 140 + i * 26, y + 30, 7, mix(BG, c, p * 0.8))
    rect(im, (236, y + 14, 900, y + 48), mix(CARD, BG, p), radius=10)
    txt(im, 256, y + 20, "http://127.0.0.1:7878/", 20, MUTED, "mono", opacity=p)
    line(im, (104, y + 64, 1816, y + 64), mix(BG, RULE, p), 2)
    txt(im, 140, y + 86, "amm", 32, INK, "bold", opacity=p)
    totals = (("workers", "$1.84"), ("manager", "$0.41"), ("tokens", "2.3M"))
    x = 260
    for k, v in totals:
        txt(im, x, y + 96, k, 20, MUTED, opacity=p)
        txt(im, x + width_of(k, 20) + 10, y + 92, v, 24, INK, "semibold", opacity=p)
        x += width_of(k, 20) + width_of(v, 24, "semibold") + 50
    live = 0.6 + 0.4 * math.sin(t * 4)
    dot(im, 1700, y + 106, 7, mix(CARD, TEAL, p * live))
    txt(im, 1716, y + 94, "live", 22, TEAL, "medium", opacity=p)
    col_w, gap, left = 316, 16, 136
    top = y + 150
    for i, name in enumerate(COLUMNS):
        x = left + i * (col_w + gap)
        rect(im, (x, top, x + col_w, y + 524), mix(CARD, (22, 25, 42), p), radius=14)
        txt(im, x + 18, top + 14, name, 22, MUTED if i < 4 else mix(MUTED, CORAL, 0.6), "semibold", opacity=p)
    placed = []
    for n, (bid, name, color, _) in enumerate(CARDS):
        q = ease((t - 0.8 - n * 0.12) / 0.4)
        if q > 0:
            col, row = card_place(t, bid)
            lift = math.sin(math.pi * (col % 1))  # 0 at rest, 1 halfway between columns
            placed.append((lift > 0, lift, q, col, row, bid, name, color))
    # Cards in flight are drawn last, raised over the ones at rest.
    for _, lift, q, col, row, bid, name, color in sorted(placed, key=lambda c: c[0]):
        x = left + col * (col_w + gap) + 12
        yy = top + 58 + row * 104 + 12 * (1 - q) - 10 * lift
        if lift:
            rect(im, (x + 6, yy + 14, x + col_w - 18, yy + 102), mix(CARD, BG, 0.7 * lift), radius=12)
        border = mix(mix(RULE, color, 0.5), color, lift)
        rect(im, (x, yy, x + col_w - 24, yy + 90), mix(CARD, (30, 33, 54), q), mix(CARD, border, q), 2, 12)
        line(im, (x + 2, yy + 14, x + 2, yy + 76), mix(CARD, color, q), 4)
        txt(im, x + 20, yy + 14, bid, 18, color, "mono", opacity=q)
        txt(im, x + 20, yy + 46, name, 22, INK, "medium", opacity=q)
    foot(
        im,
        "Updates over Server-Sent Events. One daemon per machine holds every run in SQLite under ~/.amm.",
        local=t,
        at=3.0,
    )
    return im


# 9 · Install -----------------------------------------------------------------

STEPS = (
    (
        "Add the marketplace.",
        "In Claude Code, add the amm plugin marketplace.",
        ("/plugin marketplace add 7tg/amm",),
    ),
    (
        "Install the plugin.",
        "The launcher fetches the amm binary for your platform, or builds it with cargo.",
        ("/plugin install amm@amm",),
    ),
    (
        "Delegate.",
        "Hand the main session a goal or a plan file and supervise the run.",
        ("/amm:delegate", "or: “delegate this plan with amm”"),
    ),
)


def install(t):
    im = base(t + STARTS[8])
    title(im, "Install. Delegate. Review.", t, "Two commands in Claude Code, then hand it the work.")
    for i, (label, detail, code) in enumerate(STEPS):
        x = 104 + i * 578
        p = ease((t - 0.4 - i * 0.4) / 0.6)
        if p <= 0:
            continue
        y = 370 + 24 * (1 - p)
        rect(im, (x, y, x + 552, y + 420), mix(BG, CARD, p), mix(BG, RULE, p), 2, 22)
        dot(im, x + 58, y + 58, 26, mix(CARD, ORANGE, p))
        txt(im, x + 58, y + 40, str(i + 1), 32, BG, "bold", "center", p)
        txt(im, x + 104, y + 38, label, 38, INK, "bold", opacity=p)
        for n, piece in enumerate(wrap(detail, 26, 480)):
            txt(im, x + 36, y + 116 + n * 34, piece, 26, MUTED, opacity=p)
        for n, value in enumerate(code):
            q = ease((t - 0.9 - i * 0.4 - n * 0.2) / 0.4)
            if n == 0:
                chip(im, x + 36, y + 250, value, 24, PEACH, "monobold", q, 18)
            else:
                txt(im, x + 36, y + 332, value, 22, MUTED, "mono", opacity=q)
    foot(
        im,
        "One Rust binary  ·  Linux or macOS, x86_64 or arm64  ·  hooks fail open while it installs",
        local=t,
        at=2.0,
    )
    return im


# 10 · Ending -----------------------------------------------------------------


def ending(t):
    im = base(t + STARTS[9])
    p = ease((t - 0.1) / 0.7)
    wordmark = width_of("amm", 160, "bold")
    left = (W - (150 + 48 + wordmark)) / 2
    mark(im, left, 214 + 20 * (1 - p), 150, p)
    reveal(im, left + 198, 190, "amm", 160, t, at=0.15)
    reveal(im, 960, 410, "AGENT  MIDDLE  MANAGEMENT", 28, t, at=0.35, color=ORANGE, weight="mono", align="center")
    reveal(
        im,
        960,
        478,
        "Your session writes the briefs. Cheaper agents do the work. amm keeps them honest.",
        38,
        t,
        at=0.45,
        color=INK,
        weight="semibold",
        align="center",
    )
    q = ease((t - 0.9) / 0.5)
    if q:
        value = "/plugin install amm@amm"
        w = width_of(value, 46, "monobold") + 80
        rect(
            im,
            (960 - w / 2, 590 + 16 * (1 - q), 960 + w / 2, 676 + 16 * (1 - q)),
            mix(BG, CARD, q),
            mix(BG, ORANGE, q),
            2,
            18,
        )
        txt(im, 960, 610 + 16 * (1 - q), value, 46, PEACH, "monobold", "center", q)
    reveal(im, 960, 740, "github.com/7tg/amm", 52, t, at=1.4, weight="bold", align="center")
    txt(
        im,
        960,
        840,
        "If it saved you tokens, a star helps others find it.",
        30,
        MUTED,
        "regular",
        "center",
        ease((t - 1.9) / 0.6),
    )
    return im


# Social preview ----------------------------------------------------------------


def social():
    """The 2:1 card GitHub shows when the repository is linked."""
    im = base(0).crop((0, 60, W, H - 60))
    mark(im, 100, 90, 64)
    txt(im, 184, 96, "amm", 44, INK, "bold")
    txt(im, 100, 232, "Agent middle", 140, INK, "bold")
    txt(im, 100, 384, "management.", 140, ORANGE, "bold")
    txt(im, 104, 592, "A Claude Code plugin: your session writes briefs, cheaper workers do the work", 40, INK, "semibold")
    txt(im, 104, 650, "in isolated worktrees, and amm checks every step before anything merges.", 34, MUTED)
    x = 104
    for (tier, model, color, _) in TIERS:
        x += chip(im, x, 770, f"{model} · {tier}", 32, color, pad=20) + 24
    txt(im, 1816, 788, "/plugin install amm@amm", 32, PEACH, "monobold", "right")
    return im.resize((1280, 640), Image.Resampling.LANCZOS)


# Assembly ----------------------------------------------------------------------

SCENES = (hook, briefs, workers, guardrails, watchdog, gate, integrate, dashboard, install, ending)


def scene_at(t):
    index = next((i for i, end in enumerate(ENDS) if t < end), len(ENDS) - 1)
    return index, t - STARTS[index]


def frame(t):
    index, local = scene_at(t)
    im = SCENES[index](local)
    if index and local < WIPE:
        old = SCENES[index - 1](ENDS[index - 1] - STARTS[index - 1] - 0.001)
        p = smooth(local / WIPE)
        lean = 260
        edge = -lean - 60 + (W + 2 * lean + 120) * p
        mask = Image.new("L", (W, H), 0)
        draw(mask).polygon([(-10, 0), (edge + lean, 0), (edge, H), (-10, H)], fill=255)
        im = Image.composite(im, old, mask)
        d = draw(im)
        d.polygon([(edge + lean, 0), (edge + lean + 46, 0), (edge + 46, H), (edge, H)], fill=ORANGE)
        d.polygon([(edge + lean + 46, 0), (edge + lean + 58, 0), (edge + 58, H), (edge + 46, H)], fill=SKY)
    chrome(im, index, t)
    return im


def frame_bytes(n):
    return frame(n / FPS).tobytes()


def storyboard(destination):
    times = tuple(t for start, end in zip(STARTS, ENDS, strict=True) for t in (start + 1.2, end - 0.6))
    rows = math.ceil(len(times) / 4)
    sheet = Image.new("RGB", (1920, rows * 300), BG)
    for i, t in enumerate(times):
        tile = frame(t).resize((480, 270), Image.Resampling.LANCZOS)
        x, y = i % 4 * 480, i // 4 * 300
        sheet.paste(tile, (x, y))
        txt(sheet, x + 10, y + 272, f"{t:04.1f}s", 20, MUTED, "mono")
    destination.parent.mkdir(parents=True, exist_ok=True)
    sheet.save(destination, quality=88)
    print(f"Wrote {destination}", flush=True)


def poster(destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    # The title card without the progress line, so the poster reads as a cover.
    ending(DURATION - STARTS[-1] - 1.0).save(destination, quality=90)
    print(f"Wrote {destination}", flush=True)


def save_still(t, destination):
    frame(t).save(destination)
    print(f"Wrote {destination}", flush=True)


def ffmpeg_path():
    path = shutil.which("ffmpeg")
    if not path:
        raise SystemExit("FFmpeg is required.")
    return path


def encode_film(destination, audio, fps, workers):
    destination.parent.mkdir(parents=True, exist_ok=True)
    cmd = [ffmpeg_path(), "-hide_banner", "-loglevel", "error", "-y"]
    cmd += [
        "-f",
        "rawvideo",
        "-pixel_format",
        "rgb24",
        "-video_size",
        f"{W}x{H}",
        "-framerate",
        str(fps),
        "-i",
        "pipe:0",
    ]
    if audio:
        cmd += ["-i", str(audio), "-map", "0:v", "-map", "1:a", "-c:a", "aac", "-b:a", "192k"]
    else:
        cmd += ["-an"]
    cmd += ["-vf", "scale=out_color_matrix=bt709:out_range=tv", "-c:v", "libx264", "-preset", "medium", "-crf", "19"]
    cmd += ["-pix_fmt", "yuv420p", "-movflags", "+faststart", "-color_primaries", "bt709", "-color_trc", "bt709"]
    cmd += ["-colorspace", "bt709", "-color_range", "tv", "-t", str(DURATION), str(destination)]
    total = DURATION * fps
    global FPS
    FPS = fps
    with subprocess.Popen(cmd, stdin=subprocess.PIPE) as proc, Pool(workers) as pool:
        try:
            for n, data in enumerate(pool.imap(frame_bytes, range(total), chunksize=8)):
                proc.stdin.write(data)
                if n % (fps * 5) == 0:
                    print(f"Rendered {n // fps}/{DURATION}s", flush=True)
        finally:
            proc.stdin.close()
        if proc.wait():
            raise RuntimeError("FFmpeg encoding failed")
    print(f"Wrote {destination}", flush=True)


def encode_gif(source, destination, width=800, fps=10, colors=80):
    destination.parent.mkdir(parents=True, exist_ok=True)
    base_cmd = [ffmpeg_path(), "-hide_banner", "-loglevel", "error", "-y"]
    scale = f"fps={fps},scale={width}:-2:flags=lanczos"
    with tempfile.TemporaryDirectory(prefix="amm-gif-") as directory:
        palette = str(Path(directory) / "palette.png")
        subprocess.run(
            base_cmd
            + [
                "-i",
                str(source),
                "-vf",
                f"{scale},palettegen=max_colors={colors}:stats_mode=diff",
                "-frames:v",
                "1",
                palette,
            ],
            check=True,
        )
        subprocess.run(
            base_cmd
            + [
                "-i",
                str(source),
                "-i",
                palette,
                "-filter_complex",
                f"[0:v]{scale}[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle",
            ]
            + ["-loop", "0", str(destination)],
            check=True,
        )
    size = destination.stat().st_size
    if size >= 8_000_000:
        raise RuntimeError(f"{destination} is {size / 1e6:.1f} MB, over the 8 MB preview limit")
    print(f"Wrote {destination} ({size / 1e6:.1f} MB)", flush=True)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--output", type=Path, help="film (.mp4), or a still with --frame")
    p.add_argument("--frame", type=float, nargs="+", help="render the frame at each of these seconds as an image")
    p.add_argument("--storyboard", type=Path, help="write a contact sheet of every chapter")
    p.add_argument("--gif-from", type=Path, help="only rebuild the GIF preview from this film")
    p.add_argument("--social", type=Path, help="write the 1280x640 social preview image")
    p.add_argument("--poster", type=Path, help="write the video poster still (.jpg)")
    p.add_argument("--no-audio", action="store_true")
    p.add_argument("--fps", type=int, default=FPS)
    p.add_argument("--workers", type=int, default=None)
    args = p.parse_args()
    if args.storyboard:
        storyboard(args.storyboard)
    if args.social:
        args.social.parent.mkdir(parents=True, exist_ok=True)
        social().save(args.social, optimize=True)
        print(f"Wrote {args.social}", flush=True)
    if args.poster:
        poster(args.poster)
    if args.gif_from:
        encode_gif(args.gif_from, (args.output or args.gif_from).with_suffix(".gif"))
    elif args.output and args.frame is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        if len(args.frame) == 1:
            stills = [(args.frame[0], args.output)]
        else:
            stills = [(t, args.output.with_stem(f"{args.output.stem}-{t:05.2f}")) for t in args.frame]
        with Pool(args.workers) as pool:
            pool.starmap(save_still, stills)
    elif args.output:
        with tempfile.TemporaryDirectory(prefix="amm-film-") as directory:
            audio = None
            if not args.no_audio:
                audio = Path(directory) / "score.wav"
                score = Path(__file__).resolve().with_name("demo_audio.py")
                subprocess.run(
                    [sys.executable, str(score), "--output", str(audio), "--duration", str(DURATION)], check=True
                )
            encode_film(args.output, audio, args.fps, args.workers)
        encode_gif(args.output, args.output.with_suffix(".gif"))
        poster(args.output.with_name(args.output.stem + "-poster.jpg"))
    elif not (args.storyboard or args.social or args.poster):
        p.error("Use --output, --storyboard, --social, --poster or --gif-from.")


if __name__ == "__main__":
    main()
