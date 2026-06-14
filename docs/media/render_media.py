#!/usr/bin/env python3
"""Render the runtime's real session frames (frames.json) into SVG media.

Produces:
  docs/media/dom-exploration.svg            -- animated (CSS keyframes), 6 frames
  docs/media/dom-exploration-filmstrip.svg  -- static grid of all 6 frames

No third-party dependencies: HTML is parsed with the stdlib html.parser, and the
output is hand-written SVG (which GitHub renders inline). The visuals are a
faithful projection of the DOM the runtime actually produced.
"""
import json
import os
from html.parser import HTMLParser
from xml.sax.saxutils import escape

HERE = os.path.dirname(os.path.abspath(__file__))

FW, FH = 940, 470          # per-frame canvas
PER = 3.0                  # seconds per frame in the animation


# --------------------------------------------------------------------------- #
# Tiny HTML -> tree -> app model
# --------------------------------------------------------------------------- #
class Node:
    def __init__(self, tag, attrs):
        self.tag = tag
        self.attrs = dict(attrs)
        self.children = []
        self.text = ""


class TreeBuilder(HTMLParser):
    VOID = {"br", "img", "input"}

    def __init__(self):
        super().__init__()
        self.root = Node("#root", [])
        self.stack = [self.root]

    def handle_starttag(self, tag, attrs):
        n = Node(tag, attrs)
        self.stack[-1].children.append(n)
        if tag not in self.VOID:
            self.stack.append(n)

    def handle_startendtag(self, tag, attrs):
        self.stack[-1].children.append(Node(tag, attrs))

    def handle_endtag(self, tag):
        if len(self.stack) > 1:
            self.stack.pop()

    def handle_data(self, data):
        if data.strip():
            self.stack[-1].text += data


def find(node, pred, out):
    for c in node.children:
        if pred(c):
            out.append(c)
        find(c, pred, out)


def cls(node):
    return node.attrs.get("class", "").split()


def extract_app(html):
    tb = TreeBuilder()
    tb.feed(html)
    apps = []
    find(tb.root, lambda n: n.tag == "div" and "app" in cls(n), apps)
    if not apps:
        return {"theme": "theme-light", "title": "", "tasks": []}
    app = apps[0]
    theme = "theme-dark" if "theme-dark" in cls(app) else "theme-light"
    h1s = []
    find(app, lambda n: n.tag == "h1", h1s)
    title = h1s[0].text if h1s else ""
    lis = []
    find(app, lambda n: n.tag == "li" and "task" in cls(n), lis)
    tasks = []
    for li in lis:
        labels = [c for c in li.children if "label" in cls(c)]
        label = labels[0].text if labels else ""
        tasks.append({"label": label, "done": "done" in cls(li)})
    return {"theme": theme, "title": title, "tasks": tasks}


# --------------------------------------------------------------------------- #
# SVG drawing
# --------------------------------------------------------------------------- #
THEMES = {
    "theme-light": dict(win="#ffffff", winEdge="#e2e8f0", text="#0f172a",
                        sub="#64748b", accent="#4f46e5", done="#94a3b8",
                        chip="#eef2ff", boxOn="#4f46e5"),
    "theme-dark": dict(win="#0f172a", winEdge="#1e293b", text="#e2e8f0",
                       sub="#94a3b8", accent="#38bdf8", done="#475569",
                       chip="#0b2030", boxOn="#38bdf8"),
}


def rrect(x, y, w, h, r, fill, stroke=None, sw=1, extra=""):
    s = f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{r}" ry="{r}" fill="{fill}"'
    if stroke:
        s += f' stroke="{stroke}" stroke-width="{sw}"'
    return s + f' {extra}/>'


def text(x, y, s, size, fill, weight="400", anchor="start", mono=False, deco=""):
    fam = ("ui-monospace,SFMono-Regular,Menlo,Consolas,monospace" if mono
           else "system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif")
    d = f' text-decoration="{deco}"' if deco else ""
    return (f'<text x="{x}" y="{y}" font-family="{fam}" font-size="{size}" '
            f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}"{d}>'
            f'{escape(s)}</text>')


def window(x, y, w, h, app, title_override=None):
    t = THEMES[app["theme"]]
    out = [rrect(x, y, w, h, 12, t["win"], t["winEdge"], 1.5)]
    # chrome bar
    out.append(rrect(x, y, w, 30, 12, t["winEdge"]))
    out.append(rrect(x, y + 18, w, 12, 0, t["winEdge"]))  # square off bottom of bar
    for i, c in enumerate(["#ff5f57", "#febc2e", "#28c840"]):
        out.append(f'<circle cx="{x+18+i*16}" cy="{y+15}" r="5" fill="{c}"/>')
    out.append(rrect(x + 70, y + 8, w - 86, 16, 8, t["win"], t["winEdge"], 1))
    out.append(text(x + 80, y + 20, "agent://tasks.app", 10, t["sub"], mono=True))
    # body
    bx, by = x + 22, y + 56
    out.append(text(bx, by, title_override or app["title"], 22, t["text"], "700"))
    ry = by + 26
    if not app["tasks"]:
        out.append(text(bx, ry + 16, "— no tasks yet —", 13, t["sub"]))
    for tk in app["tasks"]:
        out.append(rrect(bx, ry, w - 44, 34, 8, t["chip"]))
        box_fill = t["boxOn"] if tk["done"] else t["sub"]
        out.append(text(bx + 12, ry + 22, "[x]" if tk["done"] else "[ ]", 13,
                        box_fill, "700", mono=True))
        deco = "line-through" if tk["done"] else ""
        col = t["done"] if tk["done"] else t["text"]
        out.append(text(bx + 48, ry + 22, tk["label"], 14, col, "500", deco=deco))
        out.append(text(bx + w - 56, ry + 22, "x", 13, t["sub"]))
        ry += 42
    # add button
    out.append(rrect(bx, ry + 2, 116, 30, 8, t["accent"]))
    out.append(text(bx + 58, ry + 22, "+ Add task", 13, "#ffffff", "600", anchor="middle"))
    return "".join(out)


def journal_panel(x, y, w, h, lines):
    out = [rrect(x, y, w, h, 12, "#0b1020", "#1e293b", 1.5)]
    out.append(text(x + 16, y + 26, "● journal", 12, "#64748b", "600", mono=True))
    ly = y + 50
    for ln in lines[:11]:
        if ln.startswith("●"):
            out.append(text(x + 16, ly, trunc(ln, 44), 11.5, "#34d399", "600", mono=True))
        elif ln.startswith("{"):
            op = ""
            try:
                op = json.loads(ln).get("op", "")
            except Exception:
                pass
            out.append(text(x + 16, ly, "·", 11.5, "#64748b", mono=True))
            out.append(text(x + 28, ly, op, 11.5, "#7dd3fc", "600", mono=True))
            out.append(text(x + 28 + 9 * len(op) + 6, ly, trunc(strip_op(ln), 30),
                            11, "#94a3b8", mono=True))
        else:
            out.append(text(x + 16, ly, trunc(ln, 46), 11.5, "#cbd5e1", mono=True))
        ly += 21
    return "".join(out)


def trunc(s, n):
    return s if len(s) <= n else s[: n - 1] + "…"


def strip_op(ln):
    try:
        d = json.loads(ln)
        d.pop("op", None)
        parts = [f"{k}={v}" for k, v in d.items()]
        return " ".join(parts)
    except Exception:
        return ""


def frame_body(frame, w=FW, h=FH):
    """Render a single frame's content (caption + window(s) + journal)."""
    out = []
    # header
    out.append(text(28, 34, frame["caption"], 19, "#0f172a", "700"))
    out.append(text(28, 54, frame["note"], 12.5, "#64748b"))
    top = 72
    if frame.get("html_b"):
        a = extract_app(frame["html"])
        b = extract_app(frame["html_b"])
        out.append(window(24, top, 300, h - top - 18, a, "Tasks"))
        out.append(text(24 + 8, top - 4, "original", 11, "#94a3b8", "600"))
        out.append(window(338, top, 300, h - top - 18, b))
        out.append(text(338 + 8, top - 4, "forked · edited", 11, "#4f46e5", "600"))
        out.append(journal_panel(656, top, w - 656 - 22, h - top - 18, frame["journal"]))
    else:
        a = extract_app(frame["html"])
        out.append(window(24, top, 560, h - top - 18, a))
        out.append(journal_panel(606, top, w - 606 - 22, h - top - 18, frame["journal"]))
    return "".join(out)


def animated_svg(frames):
    n = len(frames)
    total = n * PER
    css = ["text{dominant-baseline:alphabetic}"]
    groups = []
    eps = 0.01
    for i, fr in enumerate(frames):
        a = (i / n) * 100
        b = ((i + 1) / n) * 100
        # Build an ordered list of unique opacity stops with hard cuts at the
        # window edges (epsilon-offset so no two stops share a percentage).
        stops = []
        stops.append((0.0, 1 if i == 0 else 0))
        if a > 0:
            stops.append((a - eps, 0))
            stops.append((a, 1))
        stops.append((b - eps, 1))
        if b < 100:
            stops.append((b, 0))
        stops.append((100.0, 1 if i == n - 1 else 0))
        # De-dup by percentage, keep last write.
        seen = {}
        for p, o in stops:
            seen[round(min(100.0, max(0.0, p)), 3)] = o
        body = "".join(f"{p:g}%{{opacity:{o}}}" for p, o in sorted(seen.items()))
        css.append(f"@keyframes f{i}{{{body}}}")
        css.append(f".g{i}{{opacity:0;animation:f{i} {total:.0f}s infinite}}")
        groups.append(
            f'<g class="g{i}">'
            f'<rect width="{FW}" height="{FH}" fill="#f8fafc"/>'
            f"{frame_body(fr)}"
            f"</g>"
        )
    # frame indicator dots (static)
    dots = []
    dn = len(frames)
    cx0 = FW / 2 - (dn - 1) * 9
    for i in range(dn):
        dots.append(f'<circle cx="{cx0 + i*18:.0f}" cy="{FH-9}" r="3" fill="#cbd5e1"/>')
    svg = (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {FW} {FH}" '
        f'width="{FW}" height="{FH}" font-family="system-ui">'
        f"<style>{''.join(css)}</style>"
        f'<rect width="{FW}" height="{FH}" rx="14" fill="#f8fafc"/>'
        f"{''.join(groups)}"
        f"{''.join(dots)}"
        f"</svg>"
    )
    return svg


def filmstrip_svg(frames):
    cols, rows = 2, 3
    gap = 18
    scale = 0.62
    cw, ch = FW * scale, FH * scale
    W = cols * cw + (cols + 1) * gap
    H = rows * ch + (rows + 1) * gap
    cells = []
    for i, fr in enumerate(frames):
        r, c = divmod(i, cols)
        x = gap + c * (cw + gap)
        y = gap + r * (ch + gap)
        cells.append(
            f'<g transform="translate({x:.1f},{y:.1f}) scale({scale})">'
            f'<rect width="{FW}" height="{FH}" rx="14" fill="#ffffff" '
            f'stroke="#e2e8f0" stroke-width="2"/>'
            f"{frame_body(fr)}"
            f"</g>"
        )
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W:.0f} {H:.0f}" '
        f'width="{W:.0f}" height="{H:.0f}" font-family="system-ui">'
        f'<rect width="{W:.0f}" height="{H:.0f}" fill="#eef2f6"/>'
        f"{''.join(cells)}"
        f"</svg>"
    )


def main():
    frames = json.load(open(os.path.join(HERE, "frames.json")))
    with open(os.path.join(HERE, "dom-exploration.svg"), "w") as f:
        f.write(animated_svg(frames))
    with open(os.path.join(HERE, "dom-exploration-filmstrip.svg"), "w") as f:
        f.write(filmstrip_svg(frames))
    print("wrote dom-exploration.svg and dom-exploration-filmstrip.svg")


if __name__ == "__main__":
    main()
